use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use url::Url;

use crate::cli::CollectArgs;
use crate::config::{ConfigOverride, ResolvedConfig};
use crate::error::AppError;
use crate::model::Suite;
use crate::observation::{AcquisitionLedger, CollectionObservation, ObservationParameters};
use crate::storage::write_json_atomic;

const MAX_STATUS_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum ProgressEvent {
    Starting {
        total: usize,
    },
    StatusReady {
        total: usize,
    },
    Collecting {
        total: usize,
    },
    SuiteFinished {
        suite: Suite,
        completed: usize,
        total: usize,
    },
    Verifying {
        total: usize,
    },
    Finished,
}

pub trait CollectProgress: Send + Sync {
    fn report(&self, event: ProgressEvent);
}

struct NoProgress;

impl CollectProgress for NoProgress {
    fn report(&self, _event: ProgressEvent) {}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectResult {
    pub schema_version: u32,
    pub ok: bool,
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<crate::commit_collect::CommitSelection>,
    pub commit: String,
    pub collected_at: DateTime<Utc>,
    pub output_dir: PathBuf,
    pub suites: BTreeMap<String, SuiteResult>,
    pub errors: Vec<CollectIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrIdentity {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub head_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuiteResult {
    pub state: SuiteState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuiteState {
    Running,
    NotObserved,
    Completed,
    JobLevelFailure,
    CollectionFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusIdentity {
    pub state: String,
    pub reported_for_sha: String,
    pub detail_url: String,
    pub reported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionIdentity {
    pub run_id: u64,
    pub attempt: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectIssue {
    pub suite: String,
    pub kind: String,
    pub diagnostic: String,
    pub message: String,
}

struct BinaryOptions<'a> {
    include: bool,
    per_file_limit: u64,
    total_remaining: &'a mut u64,
    consumption: &'a mut Vec<crate::gha_evidence::BinaryConsumption>,
}

#[derive(Clone, Copy)]
struct SuiteResources<'a> {
    artifact_base: Option<&'a Url>,
    evidence_dir: &'a std::path::Path,
    fetch_permits: &'a Semaphore,
    fetch_concurrency: usize,
    acquisition: &'a AcquisitionLedger,
}

#[derive(Clone)]
pub(crate) struct PinnedRun {
    pub(crate) attempt: u64,
    pub(crate) json: Arc<Vec<u8>>,
    pub(crate) context: Arc<crate::gha_evidence::RunContextCache>,
}

struct PinnedExecution {
    identity: ExecutionIdentity,
    run_json: Arc<Vec<u8>>,
    run_context: Arc<crate::gha_evidence::RunContextCache>,
}

impl CollectResult {
    pub fn exit_code(&self) -> u8 {
        if self.ok {
            0
        } else if self.errors.iter().any(|issue| issue.kind == "storage") {
            6
        } else if self.errors.iter().any(|issue| issue.kind == "integrity") {
            4
        } else if self.errors.iter().any(|issue| issue.kind == "remote") {
            5
        } else if self.errors.iter().any(|issue| issue.kind == "input") {
            2
        } else {
            3
        }
    }

    pub fn human_summary(&self) -> String {
        let suites = self
            .suites
            .iter()
            .map(|(name, result)| self.human_suite_summary(name, result))
            .collect::<Vec<_>>()
            .join("\n");
        let completeness = if self.ok {
            "evidence"
        } else {
            "incomplete evidence"
        };
        format!(
            "collected {completeness} snapshot for {}{} at {} -> {}\n{}",
            self.repository,
            self.pr
                .as_ref()
                .map(|pr| format!("#{}", pr.number))
                .unwrap_or_default(),
            &self.commit[..7],
            self.output_dir.display(),
            suites
        )
    }

    fn human_suite_summary(&self, name: &str, result: &SuiteResult) -> String {
        let state = result.state.as_str();
        let ci_state = result.status.as_ref().map(|status| status.state.as_str());
        if result.state == SuiteState::Completed
            && let Some(path) = &result.summary
            && let Ok(bytes) = std::fs::read(self.output_dir.join(path))
            && let Ok(summary) = serde_json::from_slice::<crate::gha_evidence::SuiteSummary>(&bytes)
        {
            return format!(
                "{name}: {} ({} failed / {} executed)",
                ci_state.unwrap_or(&summary.ci_state),
                summary.counts.failures + summary.counts.errors,
                summary.counts.run
            );
        }
        match ci_state {
            Some(ci_state) if result.state == SuiteState::Completed => {
                format!("{name}: {ci_state}")
            }
            Some(ci_state) => format!("{name}: {ci_state} ({state})"),
            None => format!("{name}: {state}"),
        }
    }
}

impl SuiteState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NotObserved => "not_observed",
            Self::Completed => "completed",
            Self::JobLevelFailure => "job_level_failure",
            Self::CollectionFailed => "collection_failed",
        }
    }
}

pub async fn run(args: &CollectArgs) -> Result<CollectResult, AppError> {
    run_with_progress(args, &NoProgress).await
}

pub async fn run_with_progress(
    args: &CollectArgs,
    progress: &dyn CollectProgress,
) -> Result<CollectResult, AppError> {
    let requested_suites = Suite::ALL
        .into_iter()
        .filter(|suite| args.suite.is_empty() || args.suite.contains(suite))
        .collect::<Vec<_>>();
    let total_suites = requested_suites.len();
    progress.report(ProgressEvent::Starting {
        total: total_suites,
    });
    let explicit = args.pr.is_some();
    let selected_commit = select_commit(args)?;
    let mut snapshot = status_snapshot(args.pr.as_deref())?;
    validate_snapshot(&snapshot, args.pr.as_deref(), &selected_commit, explicit)?;

    if args.wait {
        let deadline = tokio::time::Instant::now() + args.timeout;
        while !requested_suites
            .iter()
            .all(|suite| suite_is_terminal(&snapshot, *suite, &selected_commit))
        {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(args.poll_interval.min(deadline - now)).await;
            snapshot = status_snapshot(args.pr.as_deref())?;
            validate_snapshot(&snapshot, args.pr.as_deref(), &selected_commit, explicit)?;
        }
    }
    progress.report(ProgressEvent::StatusReady {
        total: total_suites,
    });

    collect_selected(args, snapshot, selected_commit, progress).await
}

pub(crate) async fn collect_selected(
    args: &CollectArgs,
    snapshot: StatusSnapshot,
    selected_commit: String,
    progress: &dyn CollectProgress,
) -> Result<CollectResult, AppError> {
    let requested_suites = Suite::ALL
        .into_iter()
        .filter(|suite| args.suite.is_empty() || args.suite.contains(suite))
        .collect::<Vec<_>>();
    let total_suites = requested_suites.len();
    let config = ResolvedConfig::load(ConfigOverride {
        data_dir: args.data_dir.clone(),
        artifact_base: args.artifact_base.clone(),
        fetch_concurrency: args.fetch_concurrency,
    })?;
    let output_dir = config
        .data_dir
        .join("github-actions/CUBRID-cubrid")
        .join(
            snapshot
                .pr
                .as_ref()
                .map(|pr| format!("pr-{}", pr.number))
                .unwrap_or_else(|| "commits".to_owned()),
        )
        .join(&selected_commit);
    let pr = snapshot.pr.as_ref().map(|pr| PrIdentity {
        number: pr.number,
        url: pr.url.clone(),
        title: pr.title.clone(),
        head_sha: pr.head_sha.clone(),
    });
    let observation = CollectionObservation::begin(ObservationParameters {
        output_dir: &output_dir,
        repository: &snapshot.repository,
        pr: pr.as_ref(),
        selection: snapshot.selection.as_ref(),
        commit: &selected_commit,
        requested_suites: &requested_suites,
        fetch_concurrency: config.fetch_concurrency,
        fetch_concurrency_source: config.fetch_concurrency_source.as_str(),
        include_binaries: args.include_binaries,
        max_binary_bytes: args.max_binary_bytes,
        max_binary_total_bytes: args.max_binary_total_bytes,
    })?;
    let fetch_permits = Arc::new(Semaphore::new(config.fetch_concurrency));
    let mut suites = BTreeMap::new();
    let mut errors = Vec::new();
    let mut binary_bytes_remaining = args.max_binary_total_bytes;
    let mut binary_consumption = Vec::new();
    let mut pinned_runs = BTreeMap::new();
    let pinned_executions = requested_suites
        .iter()
        .map(|suite| pin_execution(&snapshot, *suite, &selected_commit, &mut pinned_runs))
        .collect::<Vec<_>>();
    progress.report(ProgressEvent::Collecting {
        total: total_suites,
    });
    if args.include_binaries {
        for (completed, (suite, pinned_execution)) in requested_suites
            .into_iter()
            .zip(pinned_executions)
            .enumerate()
        {
            let (suite_result, issue) = select_suite(
                &snapshot,
                suite,
                &selected_commit,
                pinned_execution,
                BinaryOptions {
                    include: true,
                    per_file_limit: args.max_binary_bytes,
                    total_remaining: &mut binary_bytes_remaining,
                    consumption: &mut binary_consumption,
                },
                SuiteResources {
                    artifact_base: config.artifact_base.as_ref(),
                    evidence_dir: &output_dir,
                    fetch_permits: &fetch_permits,
                    fetch_concurrency: config.fetch_concurrency,
                    acquisition: observation.acquisition(),
                },
            )
            .await;
            suites.insert(suite.job_name().to_owned(), suite_result);
            if let Some(issue) = issue {
                errors.push(issue);
            }
            progress.report(ProgressEvent::SuiteFinished {
                suite,
                completed: completed + 1,
                total: total_suites,
            });
        }
    } else {
        let snapshot_ref = &snapshot;
        let selected_commit_ref = selected_commit.as_str();
        let artifact_base = config.artifact_base.as_ref();
        let output_dir_ref = &output_dir;
        let max_binary_bytes = args.max_binary_bytes;
        let fetch_permits = &fetch_permits;
        let acquisition = observation.acquisition();
        let resources = SuiteResources {
            artifact_base,
            evidence_dir: output_dir_ref,
            fetch_permits,
            fetch_concurrency: config.fetch_concurrency,
            acquisition,
        };
        let collections = requested_suites
            .into_iter()
            .zip(pinned_executions)
            .enumerate()
            .map(move |(position, (suite, pinned_execution))| async move {
                let mut unused_binary_budget = 0;
                let mut unused_binary_consumption = Vec::new();
                let result = select_suite(
                    snapshot_ref,
                    suite,
                    selected_commit_ref,
                    pinned_execution,
                    BinaryOptions {
                        include: false,
                        per_file_limit: max_binary_bytes,
                        total_remaining: &mut unused_binary_budget,
                        consumption: &mut unused_binary_consumption,
                    },
                    resources,
                )
                .await;
                (position, suite, result)
            });
        let mut collections = collections.collect::<FuturesUnordered<_>>();
        let mut completed = 0;
        let mut collected = Vec::with_capacity(total_suites);
        while let Some((position, suite, result)) = collections.next().await {
            completed += 1;
            progress.report(ProgressEvent::SuiteFinished {
                suite,
                completed,
                total: total_suites,
            });
            collected.push((position, suite, result));
        }
        collected.sort_by_key(|(position, _, _)| *position);
        for (_, suite, (suite_result, issue)) in collected {
            suites.insert(suite.job_name().to_owned(), suite_result);
            if let Some(issue) = issue {
                errors.push(issue);
            }
        }
    }
    let ok = suites
        .values()
        .all(|suite| suite.state == SuiteState::Completed);
    // An explicit run is a requested candidate, not a commit-attached status.
    if snapshot
        .selection
        .as_ref()
        .is_some_and(|selection| selection.mode == "explicit_run")
    {
        for suite in suites.values_mut() {
            suite.status = None;
        }
    }
    let result = CollectResult {
        schema_version: if snapshot.selection.is_some() { 3 } else { 2 },
        ok,
        repository: snapshot.repository,
        pr,
        selection: snapshot.selection,
        commit: selected_commit.clone(),
        collected_at: Utc::now(),
        output_dir: output_dir.clone(),
        suites,
        errors,
    };
    progress.report(ProgressEvent::Verifying {
        total: total_suites,
    });
    if let Some(pr) = &snapshot.pr
        && let Err(error) = verify_pr_head(pr.number, &selected_commit)
    {
        observation.finish_error(&error, binary_bytes_remaining, &binary_consumption)?;
        return Err(error);
    }
    if let Some(selection) = &result.selection
        && let Err(error) = selection.verify_runs()
    {
        observation.finish_error(&error, binary_bytes_remaining, &binary_consumption)?;
        return Err(error);
    }
    if let Err(error) = write_json_atomic(&output_dir.join("manifest.json"), &result) {
        observation.finish_error(&error, binary_bytes_remaining, &binary_consumption)?;
        return Err(error);
    }
    observation.finish(&result, binary_bytes_remaining, &binary_consumption)?;
    progress.report(ProgressEvent::Finished);
    Ok(result)
}

fn pin_execution(
    snapshot: &StatusSnapshot,
    suite: Suite,
    commit: &str,
    pinned_runs: &mut BTreeMap<u64, Result<PinnedRun, Arc<AppError>>>,
) -> Option<Result<PinnedExecution, AppError>> {
    let name = format!("gha-ci: {}", suite.job_name());
    let check = snapshot
        .checks
        .iter()
        .find(|check| check.name == name && check.provider == "GitHub Actions")?;
    let status = check.current.as_ref()?;
    if check.freshness != "current" || !status.reported_for_sha.eq_ignore_ascii_case(commit) {
        return Some(Err(AppError::Integrity(
            "suite status does not identify the selected commit".to_owned(),
        )));
    }
    Some(actions_run_id(&status.detail_url).and_then(|run_id| {
        let run = match pinned_runs.get(&run_id) {
            Some(run) => run.clone(),
            None => {
                let run = if let Some(selection) = &snapshot.selection {
                    selection.pinned_run(run_id)
                } else {
                    load_actions_run(run_id)
                }
                .map_err(Arc::new);
                pinned_runs.insert(run_id, run.clone());
                run
            }
        }
        .map_err(AppError::Shared)?;
        Ok(PinnedExecution {
            identity: ExecutionIdentity {
                run_id,
                attempt: run.attempt,
            },
            run_json: run.json,
            run_context: run.context,
        })
    }))
}

fn suite_is_terminal(snapshot: &StatusSnapshot, suite: Suite, commit: &str) -> bool {
    let name = format!("gha-ci: {}", suite.job_name());
    snapshot.checks.iter().any(|check| {
        check.name == name
            && check.provider == "GitHub Actions"
            && check.freshness == "current"
            && check.current.as_ref().is_some_and(|status| {
                status.reported_for_sha.eq_ignore_ascii_case(commit)
                    && !status.state.eq_ignore_ascii_case("pending")
            })
    })
}

fn select_commit(args: &CollectArgs) -> Result<String, AppError> {
    if args.pr.is_some() {
        let commit = args.commit.as_deref().ok_or_else(|| {
            AppError::Input("--commit is required when a PR number or URL is provided".to_owned())
        })?;
        return validate_full_sha(commit);
    }
    if args.commit.is_some() {
        return Err(AppError::Input(
            "--commit is only accepted with an explicit PR; current-directory collection uses local HEAD"
                .to_owned(),
        ));
    }
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::Input(format!("failed to read local HEAD: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Input(format!(
            "failed to read local HEAD: git exited with {}",
            output.status
        )));
    }
    let commit = String::from_utf8(output.stdout)
        .map_err(|_| AppError::Input("git returned a non-UTF-8 commit ID".to_owned()))?;
    validate_full_sha(commit.trim())
}

pub(crate) fn validate_full_sha(value: &str) -> Result<String, AppError> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(AppError::Input(
            "commit must be a full 40-character hexadecimal SHA".to_owned(),
        ))
    }
}

fn status_snapshot(pr: Option<&str>) -> Result<StatusSnapshot, AppError> {
    let mut command = Command::new("cubrid-pr-status");
    command.args(["--json", "--history", "0"]);
    if let Some(pr) = pr {
        command.arg(canonical_pr(pr)?);
    }
    let output = bounded_status_output(&mut command)?;
    if output.stdout.len() > MAX_STATUS_SNAPSHOT_BYTES {
        return Err(AppError::Oversized(format!(
            "cubrid-pr-status JSON exceeds the {MAX_STATUS_SNAPSHOT_BYTES}-byte limit"
        )));
    }
    if !output.status.success() {
        return Err(AppError::Remote(format!(
            "cubrid-pr-status exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut snapshot: StatusSnapshot =
        serde_json::from_slice(&output.stdout).map_err(|source| AppError::Json {
            endpoint: "cubrid-pr-status --json --history 0".to_owned(),
            source,
        })?;
    snapshot.raw_json = output.stdout;
    Ok(snapshot)
}

struct StatusOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn bounded_status_output(command: &mut Command) -> Result<StatusOutput, AppError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            AppError::Input(format!(
                "failed to execute cubrid-pr-status; install it and ensure it is on PATH: {error}"
            ))
        })?;
    let stdout = child.stdout.take().expect("status stdout is piped");
    let stderr = child.stderr.take().expect("status stderr is piped");
    let stdout_reader = std::thread::spawn(move || read_capped(stdout, MAX_STATUS_SNAPSHOT_BYTES));
    let stderr_reader = std::thread::spawn(move || read_capped(stderr, 1024 * 1024));
    let status = child
        .wait()
        .map_err(|error| AppError::Remote(format!("wait for cubrid-pr-status: {error}")))?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| AppError::Remote("read cubrid-pr-status stdout thread panicked".to_owned()))?
        .map_err(|error| AppError::Remote(format!("read cubrid-pr-status stdout: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| AppError::Remote("read cubrid-pr-status stderr thread panicked".to_owned()))?
        .map_err(|error| AppError::Remote(format!("read cubrid-pr-status stderr: {error}")))?;
    Ok(StatusOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_capped(mut reader: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let keep = (limit + 1).saturating_sub(retained.len()).min(count);
        retained.extend_from_slice(&buffer[..keep]);
    }
    Ok(retained)
}

pub(crate) fn canonical_pr(pr: &str) -> Result<String, AppError> {
    if !pr.is_empty() && pr.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(format!("https://github.com/CUBRID/cubrid/pull/{pr}"));
    }
    let url = Url::parse(pr)
        .map_err(|_| AppError::Input("PR must be a number or CUBRID GitHub PR URL".to_owned()))?;
    let segments = url.path_segments().map(|parts| parts.collect::<Vec<_>>());
    if url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && segments.as_deref().is_some_and(|parts| {
            parts.len() == 4
                && parts[0].eq_ignore_ascii_case("CUBRID")
                && parts[1].eq_ignore_ascii_case("cubrid")
                && parts[2] == "pull"
                && !parts[3].is_empty()
                && parts[3].bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        Ok(pr.to_owned())
    } else {
        Err(AppError::Input(
            "PR must be a number or CUBRID GitHub PR URL".to_owned(),
        ))
    }
}

fn validate_snapshot(
    snapshot: &StatusSnapshot,
    requested_pr: Option<&str>,
    commit: &str,
    explicit: bool,
) -> Result<(), AppError> {
    let pr_identity = snapshot
        .pr
        .as_ref()
        .ok_or_else(|| AppError::Integrity("PR status is missing PR identity".to_owned()))?;
    if snapshot.repository != "CUBRID/cubrid" {
        return Err(AppError::Integrity(format!(
            "status command returned repository {}",
            snapshot.repository
        )));
    }
    if let Some(pr) = requested_pr {
        let canonical = canonical_pr(pr)?;
        let requested_number = canonical
            .rsplit('/')
            .next()
            .and_then(|value| value.parse::<u64>().ok());
        if requested_number != Some(pr_identity.number) {
            return Err(AppError::Integrity(format!(
                "requested PR does not match returned PR #{}",
                pr_identity.number
            )));
        }
    }
    if !pr_identity.head_sha.eq_ignore_ascii_case(commit) {
        let mode = if explicit { "selected" } else { "local HEAD" };
        return Err(AppError::Integrity(format!(
            "{mode} commit {commit} does not match published PR head {}",
            pr_identity.head_sha
        )));
    }
    Ok(())
}

async fn select_suite(
    snapshot: &StatusSnapshot,
    suite: Suite,
    commit: &str,
    pinned_execution: Option<Result<PinnedExecution, AppError>>,
    binaries: BinaryOptions<'_>,
    resources: SuiteResources<'_>,
) -> (SuiteResult, Option<CollectIssue>) {
    let name = format!("gha-ci: {}", suite.job_name());
    let Some(check) = snapshot
        .checks
        .iter()
        .find(|check| check.name == name && check.provider == "GitHub Actions")
    else {
        return (
            SuiteResult {
                state: SuiteState::NotObserved,
                status: None,
                execution: None,
                summary: None,
            },
            None,
        );
    };
    let Some(status) = &check.current else {
        return (
            SuiteResult {
                state: SuiteState::NotObserved,
                status: None,
                execution: None,
                summary: None,
            },
            None,
        );
    };
    if check.freshness != "current" || !status.reported_for_sha.eq_ignore_ascii_case(commit) {
        return failed_suite(
            suite,
            AppError::Integrity("suite status does not identify the selected commit".to_owned()),
        );
    }
    let pinned_execution = match pinned_execution {
        Some(Ok(execution)) => execution,
        Some(Err(error)) => return failed_suite(suite, error),
        None => {
            return failed_suite(
                suite,
                AppError::Integrity("suite execution was not pinned before download".to_owned()),
            );
        }
    };
    let execution = pinned_execution.identity;
    let run_id = execution.run_id;
    let attempt = execution.attempt;
    if let Some(selection) = &snapshot.selection {
        if selection.is_old_attempt(run_id)
            && !resources
                .evidence_dir
                .join(format!(
                    "providers/github-actions/runs/{run_id}/attempts/{attempt}/{}/summary.json",
                    suite.job_name()
                ))
                .is_file()
        {
            return failed_suite_with_identity(suite, status.clone(), execution, AppError::Unavailable("historical attempt requires an existing validated local summary; remote artifacts are not attempt-addressable".to_owned()));
        }
    }
    if !status.state.eq_ignore_ascii_case("pending") {
        let request = crate::gha_evidence::SuiteRequest {
            suite,
            run_id,
            attempt,
            commit,
            ci_state: &status.state,
            artifact_base: resources.artifact_base,
            evidence_dir: resources.evidence_dir,
            status_snapshot_json: &snapshot.raw_json,
            run_json: &pinned_execution.run_json,
            run_context: &pinned_execution.run_context,
            lineage: &[],
            fetch_permits: resources.fetch_permits,
            fetch_concurrency: resources.fetch_concurrency,
            acquisition: resources.acquisition,
            include_binaries: binaries.include,
            max_binary_bytes: binaries.per_file_limit,
            binary_bytes_remaining: binaries.total_remaining,
            binary_consumption: binaries.consumption,
        };
        return match crate::gha_evidence::collect_suite(request).await {
            Ok(_) => (
                SuiteResult {
                    state: SuiteState::Completed,
                    status: Some(status.clone()),
                    execution: Some(execution),
                    summary: Some(PathBuf::from(format!(
                        "providers/github-actions/runs/{run_id}/attempts/{attempt}/{}/summary.json",
                        suite.job_name()
                    ))),
                },
                None,
            ),
            Err(error) => {
                if let Err(stop_error) = resources.acquisition.stop(suite) {
                    return failed_suite_with_identity(
                        suite,
                        status.clone(),
                        execution,
                        stop_error,
                    );
                }
                failed_suite_with_identity(suite, status.clone(), execution, error)
            }
        };
    }
    (
        SuiteResult {
            state: SuiteState::Running,
            status: Some(status.clone()),
            execution: Some(execution),
            summary: None,
        },
        None,
    )
}

fn failed_suite(suite: Suite, error: AppError) -> (SuiteResult, Option<CollectIssue>) {
    let state = if error.is_job_level() {
        SuiteState::JobLevelFailure
    } else {
        SuiteState::CollectionFailed
    };
    (
        SuiteResult {
            state,
            status: None,
            execution: None,
            summary: None,
        },
        Some(CollectIssue {
            suite: suite.job_name().to_owned(),
            kind: error.kind().as_str().to_owned(),
            diagnostic: error.diagnostic_kind().to_owned(),
            message: error.to_string(),
        }),
    )
}

fn failed_suite_with_identity(
    suite: Suite,
    status: StatusIdentity,
    execution: ExecutionIdentity,
    error: AppError,
) -> (SuiteResult, Option<CollectIssue>) {
    let state = if error.is_job_level() {
        SuiteState::JobLevelFailure
    } else {
        SuiteState::CollectionFailed
    };
    let issue = CollectIssue {
        suite: suite.job_name().to_owned(),
        kind: error.kind().as_str().to_owned(),
        diagnostic: error.diagnostic_kind().to_owned(),
        message: error.to_string(),
    };
    (
        SuiteResult {
            state,
            status: Some(status),
            execution: Some(execution),
            summary: None,
        },
        Some(issue),
    )
}

pub(crate) fn actions_run_id(value: &str) -> Result<u64, AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::Integrity("suite status has an invalid Actions URL".to_owned()))?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return Err(AppError::Integrity(
            "suite status URL is not on github.com".to_owned(),
        ));
    }
    let parts = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let position = parts
        .windows(2)
        .position(|pair| pair == ["actions", "runs"])
        .ok_or_else(|| AppError::Integrity("suite status URL has no Actions run ID".to_owned()))?;
    parts
        .get(position + 2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AppError::Integrity("suite status URL has an invalid run ID".to_owned()))
}

fn load_actions_run(run_id: u64) -> Result<PinnedRun, AppError> {
    let endpoint = format!("repos/CUBRID/cubrid/actions/runs/{run_id}");
    let output = gh_api_output(&endpoint)?;
    let run: ActionsRun =
        serde_json::from_slice(&output).map_err(|source| AppError::Json { endpoint, source })?;
    if run.id != run_id || run.run_attempt == 0 {
        return Err(AppError::Integrity(format!(
            "Actions metadata does not match run {run_id}"
        )));
    }
    Ok(PinnedRun {
        attempt: run.run_attempt,
        json: Arc::new(output),
        context: Arc::new(Mutex::new(None)),
    })
}

fn verify_pr_head(pr_number: u64, selected_commit: &str) -> Result<(), AppError> {
    let endpoint = format!("repos/CUBRID/cubrid/pulls/{pr_number}");
    let output = gh_api_output(&endpoint)?;
    let pull: GithubPull =
        serde_json::from_slice(&output).map_err(|source| AppError::Json { endpoint, source })?;
    if pull.number != pr_number || !pull.head.sha.eq_ignore_ascii_case(selected_commit) {
        return Err(AppError::Integrity(format!(
            "selected commit {selected_commit} is no longer pull request #{pr_number} head {}",
            pull.head.sha
        )));
    }
    Ok(())
}

fn gh_api_output(endpoint: &str) -> Result<Vec<u8>, AppError> {
    let output = Command::new("gh")
        .args(["api", endpoint])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::Remote(format!("failed to execute gh: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Remote(format!(
            "gh api failed for {endpoint} with {}",
            output.status
        )));
    }
    Ok(output.stdout)
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusSnapshot {
    pub(crate) repository: String,
    pub(crate) pr: Option<StatusPr>,
    #[serde(skip)]
    pub(crate) selection: Option<crate::commit_collect::CommitSelection>,
    #[serde(default)]
    pub(crate) checks: Vec<StatusCheck>,
    #[serde(skip)]
    pub(crate) raw_json: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusPr {
    number: u64,
    title: String,
    url: String,
    head_sha: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusCheck {
    pub(crate) name: String,
    pub(crate) provider: String,
    pub(crate) freshness: String,
    pub(crate) current: Option<StatusIdentity>,
}

#[derive(Debug, Deserialize)]
struct ActionsRun {
    id: u64,
    run_attempt: u64,
}

#[derive(Debug, Deserialize)]
struct GithubPull {
    number: u64,
    head: GithubPullHead,
}

#[derive(Debug, Deserialize)]
struct GithubPullHead {
    sha: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions_run_urls_with_or_without_a_job() {
        assert_eq!(
            actions_run_id("https://github.com/CUBRID/cubrid/actions/runs/123").unwrap(),
            123
        );
        assert_eq!(
            actions_run_id("https://github.com/CUBRID/cubrid/actions/runs/456/job/789").unwrap(),
            456
        );
    }
}
