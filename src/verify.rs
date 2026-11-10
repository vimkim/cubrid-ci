use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::cli::VerifyArgs;
use crate::config::{ConfigOverride, ResolvedConfig};
use crate::error::{AppError, ExitKind};
use crate::gha_collect::{CollectResult, SuiteState, canonical_pr, validate_full_sha};
use crate::gha_evidence::{
    VerifiedRerunSource, execution_path, read_retained, validate_json_schema, verify_retained_suite,
};
use crate::model::Suite;

const REPOSITORY: &str = "CUBRID/cubrid";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Established,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Complete,
    Incomplete,
    Interrupted,
    Failed,
    Unvalidated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestState {
    Validated,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummariesState {
    Validated,
    Partial,
    None,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryState {
    Validated,
    Invalid,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineState {
    ValidatedExact,
    PartialExact,
    Unavailable,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Integrity,
    Unavailable,
    Storage,
}

/// Facts about one collection observation and its retained bundle. The
/// `assessment` keys match the head inputs of the analyzer's report-mode gate.
#[derive(Debug, Serialize)]
pub struct VerificationReport {
    pub schema_version: u32,
    pub ok: bool,
    pub output_dir: Option<PathBuf>,
    pub observation_dir: Option<PathBuf>,
    pub assessment: Assessment,
    pub baseline: Option<BaselineState>,
    pub suites: BTreeMap<String, SuiteVerification>,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Serialize)]
pub struct Assessment {
    pub identity: Identity,
    pub observation: ObservationState,
    pub manifest: ManifestState,
    pub requested_summaries: SummariesState,
    pub result_matches_observation: bool,
    pub requested_suites_reconciled: bool,
}

#[derive(Debug, Serialize)]
pub struct SuiteVerification {
    pub state: SuiteState,
    pub summary: SummaryState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub rerun_sources: Vec<VerifiedRerunSource>,
}

#[derive(Debug, Serialize)]
pub struct Issue {
    pub scope: String,
    pub kind: IssueKind,
    pub message: String,
}

impl VerificationReport {
    pub fn exit_code(&self) -> u8 {
        let has = |kind| self.issues.iter().any(|issue| issue.kind == kind);
        if self.ok {
            0
        } else if has(IssueKind::Storage) {
            ExitKind::Storage.code()
        } else if has(IssueKind::Integrity) {
            ExitKind::Integrity.code()
        } else {
            ExitKind::Unavailable.code()
        }
    }

    pub fn human_summary(&self) -> String {
        let assessment = &self.assessment;
        let mut lines = vec![format!(
            "{} {}: identity {}, observation {}, manifest {}, summaries {}",
            if self.ok { "verified" } else { "not verified" },
            self.output_dir
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(no evidence snapshot)".to_owned()),
            name(&assessment.identity),
            name(&assessment.observation),
            name(&assessment.manifest),
            name(&assessment.requested_summaries),
        )];
        for (suite_name, suite) in &self.suites {
            lines.push(format!(
                "{suite_name}: {}, summary {}",
                name(&suite.state),
                name(&suite.summary)
            ));
        }
        for issue in &self.issues {
            lines.push(format!(
                "{} {}: {}",
                name(&issue.kind),
                issue.scope,
                issue.message
            ));
        }
        lines.join("\n")
    }
}

fn name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

struct Pins {
    pr: Option<u64>,
    head: Option<String>,
    suites: BTreeSet<Suite>,
    since: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Findings(Vec<Issue>);

impl Findings {
    fn add(&mut self, scope: &str, kind: IssueKind, message: impl Into<String>) {
        self.0.push(Issue {
            scope: scope.to_owned(),
            kind,
            message: message.into(),
        });
    }

    fn integrity(&mut self, scope: &str, message: impl Into<String>) {
        self.add(scope, IssueKind::Integrity, message);
    }

    fn unavailable(&mut self, scope: &str, message: impl Into<String>) {
        self.add(scope, IssueKind::Unavailable, message);
    }

    fn error(&mut self, scope: &str, error: &AppError) {
        self.add(scope, issue_kind(error), error.to_string());
    }

    fn has_contradiction(&self) -> bool {
        self.0
            .iter()
            .any(|issue| issue.kind != IssueKind::Unavailable)
    }
}

fn issue_kind(error: &AppError) -> IssueKind {
    match error.kind() {
        ExitKind::Unavailable => IssueKind::Unavailable,
        ExitKind::Storage if !matches!(error.root(), AppError::Serialization(_)) => {
            IssueKind::Storage
        }
        _ => IssueKind::Integrity,
    }
}

struct Observation {
    id: String,
    dir: PathBuf,
    request: Value,
    result: Option<Value>,
}

/// Verifies retained evidence offline. Only invalid input returns `Err`; every
/// evidence finding is reported in the returned report.
pub fn run(args: &VerifyArgs) -> Result<VerificationReport, AppError> {
    let pins = Pins {
        pr: args.pr.as_deref().map(pr_number).transpose()?,
        head: args.pr_head.as_deref().map(validate_full_sha).transpose()?,
        suites: Suite::requested(&args.suite).into_iter().collect(),
        since: args.since,
    };
    let saved = fs::read(&args.result).map_err(|error| {
        AppError::Input(format!(
            "read collector result {}: {error}",
            args.result.display()
        ))
    })?;
    let saved_value = serde_json::from_slice::<Value>(&saved).ok();
    let saved_result = saved_value
        .clone()
        .and_then(|value| serde_json::from_value::<CollectResult>(value).ok());
    let mut findings = Findings::default();

    let snapshot = match (&saved_result, &pins) {
        (Some(result), _) => Some(result.output_dir.clone()),
        (
            None,
            Pins {
                pr: Some(pr),
                head: Some(head),
                since: Some(_),
                ..
            },
        ) => {
            let config = ResolvedConfig::load(ConfigOverride {
                data_dir: args.data_dir.clone(),
                artifact_base: None,
                fetch_concurrency: None,
            })?;
            Some(
                config
                    .data_dir
                    .join("github-actions/CUBRID-cubrid")
                    .join(format!("pr-{pr}"))
                    .join(head),
            )
        }
        (None, _) => {
            findings.unavailable(
                "observation",
                "the collector result has no command result; --pr, --pr-head, and --since are required to locate an interrupted or failed observation",
            );
            None
        }
    };
    let unselected_identity = saved_result
        .as_ref()
        .map_or(Identity::Unknown, |result| result_identity(result, &pins));
    let Some(snapshot) = snapshot else {
        return Ok(unselected(None, unselected_identity, findings));
    };

    let (observations, unreadable) = load_observations(&snapshot);
    let matches = observations
        .iter()
        .filter(|observation| started_since(&observation.request, pins.since))
        .filter(|observation| match (&saved_result, &saved_value) {
            (Some(_), Some(saved)) => observation
                .result
                .as_ref()
                .is_some_and(|result| result.get("command") == Some(saved)),
            _ => request_matches_pins(&observation.request, &pins),
        })
        .collect::<Vec<_>>();
    let observation = match matches.as_slice() {
        [observation] => *observation,
        [] => {
            findings.unavailable(
                "observation",
                "no collection observation in the evidence snapshot matches this collector result and its pins",
            );
            findings.0.extend(unreadable.0);
            return Ok(unselected(Some(snapshot), unselected_identity, findings));
        }
        many => {
            findings.unavailable(
                "observation",
                format!(
                    "{} collection observations match; the observation for this invocation is ambiguous",
                    many.len()
                ),
            );
            return Ok(unselected(Some(snapshot), unselected_identity, findings));
        }
    };

    let mut state = observation_state(observation, &mut findings);
    let command = observation
        .result
        .as_ref()
        .and_then(|result| result.get("command"))
        .cloned();
    let parsed_command = command
        .clone()
        .map(serde_json::from_value::<CollectResult>)
        .transpose()
        .unwrap_or_else(|error| {
            findings.integrity(
                "observation",
                format!("observation command is malformed: {error}"),
            );
            None
        });
    if parsed_command.is_none() && command.is_some() {
        state = ObservationState::Unvalidated;
    }

    let request_suites = suite_set(&observation.request["requested_suites"]);
    let mut reconciled = request_suites.as_ref() == Some(&pins.suites);
    if !reconciled {
        findings.unavailable(
            "observation",
            "the observation's requested suites differ from the requested suite set",
        );
    }

    let mut identity = request_identity(observation, &pins, &mut findings);
    let mut manifest = ManifestState::Missing;
    let mut suites = BTreeMap::new();
    let mut baseline_relationship = false;
    if let (Some(command), Some(parsed)) = (&command, &parsed_command) {
        baseline_relationship = parsed
            .selection
            .as_ref()
            .is_some_and(|selection| selection.baseline.is_some());
        if !command_identity(
            observation,
            command,
            parsed,
            &snapshot,
            &pins,
            &mut findings,
        ) {
            identity = Identity::Unknown;
        }
        let command_suites = parsed
            .suites
            .keys()
            .map(|name| suite_named(name))
            .collect::<Option<BTreeSet<_>>>();
        if command_suites != request_suites {
            reconciled = false;
            findings.integrity(
                "observation",
                "the command result's suites differ from the observation request",
            );
        }
        manifest = manifest_state(
            &snapshot,
            command,
            &observations,
            observation,
            &mut findings,
        );
        let include_binaries = observation.request["binary_budget"]["include"] == true;
        for (suite, result) in &parsed.suites {
            suites.insert(
                suite.clone(),
                verify_suite(
                    &snapshot,
                    suite,
                    result,
                    &parsed.commit,
                    include_binaries,
                    &mut findings,
                ),
            );
            if state == ObservationState::Incomplete && result.state != SuiteState::Completed {
                findings.unavailable(
                    suite,
                    format!(
                        "the suite is {} and has no validated summary",
                        name(&result.state)
                    ),
                );
            }
        }
    } else {
        findings.unavailable(
            "manifest",
            "the selected observation has no terminal command result, so no manifest belongs to it",
        );
    }

    let requested_summaries = summaries_state(&suites);
    let result_matches_observation = saved_result.is_some() && command.is_some();
    if findings
        .0
        .iter()
        .any(|issue| issue.scope == "observation" && issue.kind == IssueKind::Integrity)
    {
        state = ObservationState::Unvalidated;
    }
    let pinned = pins.pr.is_some() && pins.head.is_some();
    // Unpinned is unconfirmed, not contradicted, so it stays out of the baseline's `invalid`.
    let identity_contradicted = identity == Identity::Unknown;
    if baseline_relationship && !pinned {
        findings.unavailable(
            "identity",
            "an exact baseline needs --pr and --pr-head to check its baseline relationship",
        );
        identity = Identity::Unknown;
    }
    let assessment = Assessment {
        identity,
        observation: state,
        manifest,
        requested_summaries,
        result_matches_observation,
        requested_suites_reconciled: reconciled,
    };
    let ok = assessment.identity == Identity::Established
        && assessment.observation == ObservationState::Complete
        && assessment.manifest == ManifestState::Validated
        && assessment.requested_summaries == SummariesState::Validated
        && assessment.result_matches_observation
        && assessment.requested_suites_reconciled
        && findings.0.is_empty();
    let baseline = baseline_relationship.then(|| {
        if ok {
            BaselineState::ValidatedExact
        } else if findings.has_contradiction() || identity_contradicted {
            BaselineState::Invalid
        } else if pinned
            && matches!(
                assessment.observation,
                ObservationState::Complete | ObservationState::Incomplete
            )
            && assessment.manifest == ManifestState::Validated
            && assessment.result_matches_observation
            && assessment.requested_suites_reconciled
            && assessment.requested_summaries == SummariesState::Partial
        {
            BaselineState::PartialExact
        } else {
            BaselineState::Unavailable
        }
    });
    Ok(VerificationReport {
        schema_version: 1,
        ok,
        output_dir: Some(snapshot),
        observation_dir: Some(observation.dir.clone()),
        assessment,
        baseline,
        suites,
        issues: findings.0,
    })
}

/// The identity a command result claims when no observation corroborates it.
/// It still names the PR and commit, so a bounded warning report is possible.
fn result_identity(result: &CollectResult, pins: &Pins) -> Identity {
    let (pr, head) = match (&result.pr, &result.selection) {
        (Some(pr), None) if pr.head_sha == result.commit => {
            (Some(pr.number), Some(result.commit.as_str()))
        }
        (None, Some(selection)) if selection.commit == result.commit => match &selection.baseline {
            Some(baseline) if baseline.merge_base == result.commit => {
                (Some(baseline.pr_number), Some(baseline.head_sha.as_str()))
            }
            Some(_) => return Identity::Unknown,
            None => (None, None),
        },
        _ => return Identity::Unknown,
    };
    if result.repository == REPOSITORY
        && pins.pr.is_none_or(|pinned| Some(pinned) == pr)
        && pins
            .head
            .as_deref()
            .is_none_or(|pinned| Some(pinned) == head)
    {
        Identity::Established
    } else {
        Identity::Unknown
    }
}

fn unselected(
    output_dir: Option<PathBuf>,
    identity: Identity,
    findings: Findings,
) -> VerificationReport {
    VerificationReport {
        schema_version: 1,
        ok: false,
        output_dir,
        observation_dir: None,
        assessment: Assessment {
            identity,
            observation: ObservationState::Unvalidated,
            manifest: ManifestState::Missing,
            requested_summaries: SummariesState::None,
            result_matches_observation: false,
            requested_suites_reconciled: false,
        },
        baseline: None,
        suites: BTreeMap::new(),
        issues: findings.0,
    }
}

fn pr_number(value: &str) -> Result<u64, AppError> {
    let url = canonical_pr(value)?;
    url.rsplit('/')
        .next()
        .and_then(|number| number.parse().ok())
        .filter(|number| *number > 0)
        .ok_or_else(|| AppError::Input("PR must be a number or CUBRID GitHub PR URL".to_owned()))
}

fn suite_named(name: &str) -> Option<Suite> {
    Suite::ALL
        .into_iter()
        .find(|suite| suite.job_name() == name)
}

fn suite_set(value: &Value) -> Option<BTreeSet<Suite>> {
    value
        .as_array()?
        .iter()
        .map(|suite| suite.as_str().and_then(suite_named))
        .collect()
}

fn started_since(request: &Value, since: Option<DateTime<Utc>>) -> bool {
    since.is_none_or(|since| {
        request["started_at"]
            .as_str()
            .and_then(|value| value.parse::<DateTime<Utc>>().ok())
            .is_some_and(|started| started >= since)
    })
}

fn request_matches_pins(request: &Value, pins: &Pins) -> bool {
    pins.pr.is_some_and(|pr| request["pr"]["number"] == pr)
        && pins
            .head
            .as_deref()
            .is_some_and(|head| request["commit"] == head)
        && suite_set(&request["requested_suites"]).as_ref() == Some(&pins.suites)
}

/// Readable observations, plus findings for an unreadable observations
/// directory or for entries whose request or result cannot be read or parsed.
fn load_observations(snapshot: &Path) -> (Vec<Observation>, Findings) {
    let directory = snapshot.join("observations");
    let mut unreadable = Findings::default();
    let mut ids = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                unreadable.error("observation", &AppError::storage(&directory, error));
            }
            return (Vec::new(), unreadable);
        }
    };
    ids.sort();
    let mut observations = Vec::new();
    for id in ids {
        let relative = Path::new("observations").join(&id);
        let result_path = relative.join("result.json");
        let loaded = read_json(snapshot, &relative.join("request.json")).and_then(|request| {
            let result = snapshot
                .join(&result_path)
                .exists()
                .then(|| read_json(snapshot, &result_path))
                .transpose()?;
            Ok((request, result))
        });
        match loaded {
            Ok((request, result)) => observations.push(Observation {
                dir: snapshot.join(&relative),
                id,
                request,
                result,
            }),
            Err(error) => unreadable.add(
                "observation",
                issue_kind(&error),
                format!(
                    "observation {id} is unreadable or malformed and may be the one this result came from: {error}"
                ),
            ),
        }
    }
    (observations, unreadable)
}

fn read_json(root: &Path, relative: &Path) -> Result<Value, AppError> {
    Ok(serde_json::from_slice(&read_retained(root, relative)?)?)
}

fn observation_state(observation: &Observation, findings: &mut Findings) -> ObservationState {
    let request = &observation.request;
    let (request_schema, result_schema) = match request["schema_version"].as_u64() {
        Some(1) => (
            include_str!("../schema/collection-observation-request-v1.schema.json"),
            include_str!("../schema/collection-observation-result-v1.schema.json"),
        ),
        Some(2) => (
            include_str!("../schema/collection-observation-request-v2.schema.json"),
            include_str!("../schema/collection-observation-result-v2.schema.json"),
        ),
        _ => {
            findings.integrity(
                "observation",
                "observation request has an unknown schema version",
            );
            return ObservationState::Unvalidated;
        }
    };
    if let Err(error) = validate_value("collection-observation-request", request_schema, request) {
        findings.error("observation", &error);
        return ObservationState::Unvalidated;
    }
    if request["observation_id"] != observation.id.as_str() {
        findings.integrity(
            "observation",
            "observation request ID differs from its directory name",
        );
        return ObservationState::Unvalidated;
    }
    let Some(result) = &observation.result else {
        return ObservationState::Interrupted;
    };
    if let Err(error) = validate_value("collection-observation-result", result_schema, result) {
        findings.error("observation", &error);
        return ObservationState::Unvalidated;
    }
    if result["observation_id"] != request["observation_id"]
        || result["schema_version"] != request["schema_version"]
    {
        findings.integrity(
            "observation",
            "observation request and result have different identities",
        );
        return ObservationState::Unvalidated;
    }
    check_binary_budget(request, result, findings);
    let command_ok = result["command"]["ok"].as_bool();
    match (result["outcome"].as_str(), command_ok) {
        (Some("complete"), Some(true)) => ObservationState::Complete,
        (Some("incomplete"), Some(false)) => ObservationState::Incomplete,
        (Some("failed"), None) => {
            findings.unavailable(
                "observation",
                format!(
                    "collection failed: {} ({}): {}",
                    result["error"]["kind"].as_str().unwrap_or("unknown"),
                    result["error"]["diagnostic"].as_str().unwrap_or("unknown"),
                    result["error"]["message"].as_str().unwrap_or_default()
                ),
            );
            ObservationState::Failed
        }
        _ => {
            findings.integrity(
                "observation",
                "observation outcome disagrees with its command result",
            );
            ObservationState::Unvalidated
        }
    }
}

fn check_binary_budget(request: &Value, result: &Value, findings: &mut Findings) {
    let budget = &result["binary_budget"];
    let field = |name: &str| budget[name].as_u64().map(u128::from);
    let artifacts = budget["artifacts"].as_array();
    let requested = suite_set(&request["requested_suites"]).unwrap_or_default();
    let consumed_by_artifacts = artifacts.map(|artifacts| {
        artifacts
            .iter()
            .map(|artifact| u128::from(artifact["consumed_bytes"].as_u64().unwrap_or(u64::MAX)))
            .sum::<u128>()
    });
    let reconciled = field("configured_bytes")
        == request["binary_budget"]["total_bytes"]
            .as_u64()
            .map(u128::from)
        && field("consumed_bytes")
            .zip(field("remaining_bytes"))
            .map(|(consumed, remaining)| consumed + remaining)
            == field("configured_bytes")
        && consumed_by_artifacts == field("consumed_bytes")
        && artifacts.is_some_and(|artifacts| {
            artifacts.iter().all(|artifact| {
                artifact["suite"]
                    .as_str()
                    .and_then(suite_named)
                    .is_some_and(|suite| requested.contains(&suite))
            })
        });
    if !reconciled {
        findings.integrity(
            "observation",
            "binary transfer budget does not reconcile with the request and per-artifact consumption",
        );
    }
}

fn request_identity(observation: &Observation, pins: &Pins, findings: &mut Findings) -> Identity {
    let request = &observation.request;
    let commit = request["commit"].as_str();
    let pr = request["pr"]["number"].as_u64();
    let baseline = &request["selection"]["baseline"];
    let (pinned_pr, pinned_head) = if baseline.is_object() {
        (
            baseline["pr_number"].as_u64(),
            baseline["head_sha"].as_str(),
        )
    } else {
        (pr, commit)
    };
    let mut established = request["repository"] == REPOSITORY && commit.is_some();
    if pins.pr.is_some() && pins.pr != pinned_pr {
        findings.integrity(
            "identity",
            "the observation does not belong to the pinned PR",
        );
        established = false;
    }
    if pins.head.is_some() && pins.head.as_deref() != pinned_head {
        findings.integrity(
            "identity",
            "the observation does not belong to the pinned PR head",
        );
        established = false;
    }
    if established {
        Identity::Established
    } else {
        Identity::Unknown
    }
}

fn command_identity(
    observation: &Observation,
    command: &Value,
    parsed: &CollectResult,
    snapshot: &Path,
    pins: &Pins,
    findings: &mut Findings,
) -> bool {
    let request = &observation.request;
    let mut consistent = true;
    let mut fail = |findings: &mut Findings, message: &str| {
        findings.integrity("identity", message);
        consistent = false;
    };
    if parsed.repository != REPOSITORY
        || request["repository"] != command["repository"]
        || request["commit"] != command["commit"]
        || request["pr"] != command["pr"]
        || request["selection"] != command["selection"]
    {
        fail(
            findings,
            "the command result and observation request disagree on repository, PR, selection, or commit",
        );
    }
    let all_completed = parsed
        .suites
        .values()
        .all(|suite| suite.state == SuiteState::Completed);
    if parsed.ok != all_completed || (parsed.ok && !parsed.errors.is_empty()) {
        fail(
            findings,
            "the command result's ok flag disagrees with its suite states and errors",
        );
    }
    let directory = match (&parsed.pr, &parsed.selection) {
        (Some(pr), None) if parsed.schema_version == 2 => {
            if pr.head_sha != parsed.commit || pr_number(&pr.url).ok() != Some(pr.number) {
                fail(
                    findings,
                    "the PR URL, number, or head in the command result disagree with its commit",
                );
            }
            format!("pr-{}", pr.number)
        }
        (None, Some(selection)) if parsed.schema_version == 3 => {
            if selection.commit != parsed.commit
                || selection
                    .baseline
                    .as_ref()
                    .is_some_and(|baseline| baseline.merge_base != parsed.commit)
            {
                fail(
                    findings,
                    "the commit selection or baseline merge base differs from the collected commit",
                );
            }
            if selection.baseline.is_none() && (pins.pr.is_some() || pins.head.is_some()) {
                fail(
                    findings,
                    "a commit collection has no PR relationship to check against the pins",
                );
            }
            "commits".to_owned()
        }
        _ => {
            fail(findings, "the command result has an unknown identity shape");
            String::new()
        }
    };
    let expected_tail = [
        "github-actions",
        "CUBRID-cubrid",
        directory.as_str(),
        parsed.commit.as_str(),
    ];
    let tail = snapshot
        .components()
        .rev()
        .take(4)
        .map(|component| match component {
            Component::Normal(name) => name.to_str().unwrap_or_default(),
            _ => "",
        })
        .collect::<Vec<_>>();
    if !snapshot.is_absolute()
        || tail.iter().rev().copied().ne(expected_tail.iter().copied())
        || snapshot
            .components()
            .any(|component| component == Component::ParentDir)
    {
        fail(
            findings,
            "the evidence snapshot directory is not the canonical path for its identity",
        );
    }
    consistent
}

fn manifest_state(
    snapshot: &Path,
    command: &Value,
    observations: &[Observation],
    selected: &Observation,
    findings: &mut Findings,
) -> ManifestState {
    let path = Path::new("manifest.json");
    if !snapshot.join(path).exists() {
        findings.unavailable("manifest", "the evidence snapshot has no manifest");
        return ManifestState::Missing;
    }
    let manifest = match read_retained(snapshot, path)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(AppError::from))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            findings.error("manifest", &error);
            return ManifestState::Invalid;
        }
    };
    let schema = match manifest["schema_version"].as_u64() {
        Some(2) => include_str!("../schema/manifest-v2.schema.json"),
        Some(3) => include_str!("../schema/manifest-v3.schema.json"),
        _ => {
            findings.integrity("manifest", "manifest has an unsupported schema version");
            return ManifestState::Invalid;
        }
    };
    if let Err(error) = validate_value("manifest", schema, &manifest) {
        findings.error("manifest", &error);
        return ManifestState::Invalid;
    }
    if manifest == *command {
        return ManifestState::Validated;
    }
    let later = observations.iter().find(|observation| {
        observation.id != selected.id
            && observation
                .result
                .as_ref()
                .is_some_and(|result| result.get("command") == Some(&manifest))
    });
    if later.is_some() && manifest_agrees(&manifest, command) {
        return ManifestState::Validated;
    }
    if let Some(later) = later {
        findings.unavailable(
            "manifest",
            format!(
                "the manifest was replaced by observation {}; this observation's result remains in its observation directory",
                later.id
            ),
        );
    } else {
        findings.integrity(
            "manifest",
            "the manifest differs from the command result and from every retained observation",
        );
    }
    ManifestState::Invalid
}

/// A later collection may replace the root manifest. It still agrees with
/// this observation when the identity, suite states, and errors match.
fn manifest_agrees(manifest: &Value, command: &Value) -> bool {
    let states = |value: &Value| {
        value["suites"].as_object().map(|suites| {
            suites
                .iter()
                .map(|(name, suite)| (name.clone(), suite["state"].clone()))
                .collect::<BTreeMap<_, _>>()
        })
    };
    [
        "/schema_version",
        "/repository",
        "/pr/number",
        "/pr/url",
        "/commit",
        "/output_dir",
        "/errors",
    ]
    .iter()
    .all(|pointer| manifest.pointer(pointer) == command.pointer(pointer))
        && states(manifest) == states(command)
}

fn verify_suite(
    snapshot: &Path,
    name: &str,
    result: &crate::gha_collect::SuiteResult,
    commit: &str,
    include_binaries: bool,
    findings: &mut Findings,
) -> SuiteVerification {
    let mut verification = SuiteVerification {
        state: result.state,
        summary: SummaryState::Absent,
        path: None,
        rerun_sources: Vec::new(),
    };
    let Some(suite) = suite_named(name) else {
        findings.integrity(name, "the command result names an unknown suite");
        verification.summary = SummaryState::Invalid;
        return verification;
    };
    if result.state != SuiteState::Completed {
        if result.summary.is_some() {
            findings.integrity(name, "a suite without a completed state declares a summary");
            verification.summary = SummaryState::Invalid;
        }
        return verification;
    }
    let Some(execution) = &result.execution else {
        findings.integrity(name, "a completed suite has no execution identity");
        verification.summary = SummaryState::Invalid;
        return verification;
    };
    let path = execution_path(execution.run_id, execution.attempt, suite).join("summary.json");
    if result.summary.as_ref() != Some(&path) {
        findings.integrity(
            name,
            "the completed suite's summary path is not the derived path for its execution",
        );
        verification.summary = SummaryState::Invalid;
        return verification;
    }
    match verify_retained_suite(
        snapshot,
        suite,
        execution.run_id,
        execution.attempt,
        commit,
        include_binaries,
    ) {
        Ok(sources) => {
            verification.summary = SummaryState::Validated;
            verification.path = Some(path);
            verification.rerun_sources = sources;
        }
        Err(error) => {
            findings.error(name, &error);
            verification.summary = SummaryState::Invalid;
        }
    }
    verification
}

fn summaries_state(suites: &BTreeMap<String, SuiteVerification>) -> SummariesState {
    let validated = suites
        .values()
        .filter(|suite| suite.summary == SummaryState::Validated)
        .count();
    if suites
        .values()
        .any(|suite| suite.summary == SummaryState::Invalid)
    {
        SummariesState::Invalid
    } else if validated > 0 && validated == suites.len() {
        SummariesState::Validated
    } else if validated > 0 {
        SummariesState::Partial
    } else {
        SummariesState::None
    }
}

fn validate_value(label: &str, schema: &str, value: &Value) -> Result<(), AppError> {
    validate_json_schema(label, schema, &serde_json::to_vec(value)?)
}
