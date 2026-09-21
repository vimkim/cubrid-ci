use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::CollectArgs;
use crate::config::{ConfigOverride, ResolvedConfig};
use crate::error::AppError;
use crate::model::Suite;
use crate::storage::write_json_atomic;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectResult {
    pub schema_version: u32,
    pub ok: bool,
    pub repository: String,
    pub pr: PrIdentity,
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
    EvidencePending,
    Completed,
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
    pub message: String,
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
            .map(|(name, result)| format!("{name}={}", result.state.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let completeness = if self.ok {
            "evidence"
        } else {
            "incomplete evidence"
        };
        format!(
            "collected {completeness} snapshot for {}#{} at {}: {} -> {}",
            self.repository,
            self.pr.number,
            &self.commit[..7],
            suites,
            self.output_dir.display()
        )
    }
}

impl SuiteState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NotObserved => "not_observed",
            Self::EvidencePending => "evidence_pending",
            Self::Completed => "completed",
            Self::CollectionFailed => "collection_failed",
        }
    }
}

pub async fn run(args: &CollectArgs) -> Result<CollectResult, AppError> {
    let explicit = args.pr.is_some();
    let selected_commit = select_commit(args)?;
    let snapshot = status_snapshot(args.pr.as_deref())?;
    validate_snapshot(&snapshot, args.pr.as_deref(), &selected_commit, explicit)?;

    let config = ResolvedConfig::load(ConfigOverride {
        data_dir: args.data_dir.clone(),
        artifact_base: args.artifact_base.clone(),
    })?;
    let requested_suites = if args.suite.is_empty() {
        Suite::ALL.to_vec()
    } else {
        args.suite.clone()
    };

    let output_dir = config
        .data_dir
        .join("github-actions/CUBRID-cubrid")
        .join(format!("pr-{}", snapshot.pr.number))
        .join(&selected_commit);
    let mut suites = BTreeMap::new();
    let mut errors = Vec::new();
    for suite in requested_suites {
        let (suite_result, issue) = select_suite(
            &snapshot,
            suite,
            &selected_commit,
            config.artifact_base.as_ref(),
            &output_dir,
        )
        .await;
        suites.insert(suite.job_name().to_owned(), suite_result);
        if let Some(issue) = issue {
            errors.push(issue);
        }
    }
    let ok = suites
        .values()
        .all(|suite| suite.state == SuiteState::Completed);
    let result = CollectResult {
        schema_version: 2,
        ok,
        repository: snapshot.repository,
        pr: PrIdentity {
            number: snapshot.pr.number,
            url: snapshot.pr.url,
            title: snapshot.pr.title,
            head_sha: snapshot.pr.head_sha,
        },
        commit: selected_commit,
        collected_at: Utc::now(),
        output_dir: output_dir.clone(),
        suites,
        errors,
    };
    write_json_atomic(&output_dir.join("manifest.json"), &result)?;
    Ok(result)
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

fn validate_full_sha(value: &str) -> Result<String, AppError> {
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
    let output = command.stdin(Stdio::null()).output().map_err(|error| {
        AppError::Input(format!(
            "failed to execute cubrid-pr-status; install it and ensure it is on PATH: {error}"
        ))
    })?;
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

fn canonical_pr(pr: &str) -> Result<String, AppError> {
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
        if requested_number != Some(snapshot.pr.number) {
            return Err(AppError::Integrity(format!(
                "requested PR does not match returned PR #{}",
                snapshot.pr.number
            )));
        }
    }
    if !snapshot.pr.head_sha.eq_ignore_ascii_case(commit) {
        let mode = if explicit { "selected" } else { "local HEAD" };
        return Err(AppError::Integrity(format!(
            "{mode} commit {commit} does not match published PR head {}",
            snapshot.pr.head_sha
        )));
    }
    Ok(())
}

async fn select_suite(
    snapshot: &StatusSnapshot,
    suite: Suite,
    commit: &str,
    artifact_base: Option<&Url>,
    evidence_dir: &std::path::Path,
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
    let run_id = match actions_run_id(&status.detail_url) {
        Ok(run_id) => run_id,
        Err(error) => return failed_suite(suite, error),
    };
    let attempt = match current_attempt(run_id) {
        Ok(attempt) => attempt,
        Err(error) => return failed_suite(suite, error),
    };
    let execution = ExecutionIdentity { run_id, attempt };
    if !status.state.eq_ignore_ascii_case("pending") && suite == Suite::Sql {
        let request = crate::gha_evidence::SqlRequest {
            run_id,
            attempt,
            commit,
            ci_state: &status.state,
            artifact_base,
            evidence_dir,
            status_snapshot_json: &snapshot.raw_json,
        };
        return match crate::gha_evidence::collect_sql(request).await {
            Ok(_) => (
                SuiteResult {
                    state: SuiteState::Completed,
                    status: Some(status.clone()),
                    execution: Some(execution),
                    summary: Some(PathBuf::from(format!(
                        "providers/github-actions/runs/{run_id}/attempts/{attempt}/test_sql/summary.json"
                    ))),
                },
                None,
            ),
            Err(error) => failed_suite_with_identity(suite, status.clone(), execution, error),
        };
    }
    let state = if status.state.eq_ignore_ascii_case("pending") {
        SuiteState::Running
    } else {
        SuiteState::EvidencePending
    };
    (
        SuiteResult {
            state,
            status: Some(status.clone()),
            execution: Some(execution),
            summary: None,
        },
        None,
    )
}

fn failed_suite(suite: Suite, error: AppError) -> (SuiteResult, Option<CollectIssue>) {
    (
        SuiteResult {
            state: SuiteState::CollectionFailed,
            status: None,
            execution: None,
            summary: None,
        },
        Some(CollectIssue {
            suite: suite.job_name().to_owned(),
            kind: error.kind().as_str().to_owned(),
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
    let issue = CollectIssue {
        suite: suite.job_name().to_owned(),
        kind: error.kind().as_str().to_owned(),
        message: error.to_string(),
    };
    (
        SuiteResult {
            state: SuiteState::CollectionFailed,
            status: Some(status),
            execution: Some(execution),
            summary: None,
        },
        Some(issue),
    )
}

fn actions_run_id(value: &str) -> Result<u64, AppError> {
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

fn current_attempt(run_id: u64) -> Result<u64, AppError> {
    let endpoint = format!("repos/CUBRID/cubrid/actions/runs/{run_id}");
    let output = Command::new("gh")
        .args(["api", &endpoint])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::Remote(format!("failed to execute gh: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Remote(format!(
            "gh api failed for Actions run {run_id} with {}",
            output.status
        )));
    }
    let run: ActionsRun = serde_json::from_slice(&output.stdout)
        .map_err(|source| AppError::Json { endpoint, source })?;
    if run.id != run_id || run.run_attempt == 0 {
        return Err(AppError::Integrity(format!(
            "Actions metadata does not match run {run_id}"
        )));
    }
    Ok(run.run_attempt)
}

#[derive(Debug, Deserialize)]
struct StatusSnapshot {
    repository: String,
    pr: StatusPr,
    #[serde(default)]
    checks: Vec<StatusCheck>,
    #[serde(skip)]
    raw_json: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct StatusPr {
    number: u64,
    title: String,
    url: String,
    head_sha: String,
}

#[derive(Debug, Deserialize)]
struct StatusCheck {
    name: String,
    provider: String,
    freshness: String,
    current: Option<StatusIdentity>,
}

#[derive(Debug, Deserialize)]
struct ActionsRun {
    id: u64,
    run_attempt: u64,
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
