use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    #[serde(rename = "test_medium")]
    Medium,
    #[serde(rename = "test_sql")]
    Sql,
    #[serde(rename = "test_shell")]
    Shell,
}

impl Suite {
    pub const ALL: [Self; 3] = [Self::Medium, Self::Sql, Self::Shell];

    pub const fn job_name(self) -> &'static str {
        match self {
            Self::Medium => "test_medium",
            Self::Sql => "test_sql",
            Self::Shell => "test_shell",
        }
    }

    pub const fn command_name(self) -> &'static str {
        match self {
            Self::Medium => "test-medium",
            Self::Sql => "test-sql",
            Self::Shell => "test-shell",
        }
    }

    pub fn status_context(self) -> String {
        format!("ci/circleci: {}", self.job_name())
    }

    pub const fn prerequisites(self) -> &'static [&'static str] {
        match self {
            Self::Medium | Self::Sql => &["build", "build_debug"],
            Self::Shell => &["build", "build_debug", "download-build"],
        }
    }
}

impl fmt::Display for Suite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.job_name())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryRef {
    pub owner: String,
    pub name: String,
    pub pr_number: u64,
}

impl RepositoryRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    pub state: String,
    pub head: PullRef,
    pub base: PullRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitStatus {
    pub context: String,
    pub state: String,
    #[serde(default)]
    pub target_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CommitStatus {
    pub fn is_pending(&self) -> bool {
        self.state.eq_ignore_ascii_case("pending")
    }

    pub fn is_success(&self) -> bool {
        self.state.eq_ignore_ascii_case("success")
    }

    pub fn is_terminal(&self) -> bool {
        !self.is_pending()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircleJob {
    pub build_num: u64,
    pub status: String,
    pub vcs_revision: String,
    #[serde(default)]
    pub queued_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub start_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub stop_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub build_time_millis: Option<u64>,
    #[serde(default)]
    pub parallel: Option<u32>,
    pub workflows: CircleWorkflow,
    #[serde(default)]
    pub steps: Vec<CircleStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircleWorkflow {
    pub job_name: String,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub workflow_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircleStep {
    pub name: String,
    #[serde(default)]
    pub actions: Vec<CircleAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircleAction {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub failed: Option<bool>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub run_time_millis: Option<u64>,
    #[serde(default)]
    pub output_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircleTestsResponse {
    pub tests: Vec<TestCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    pub name: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(rename = "class", default)]
    pub class_name: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    pub result: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub run_time: Option<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircleArtifact {
    pub path: String,
    pub url: String,
    #[serde(default)]
    pub node_index: Option<u32>,
    #[serde(default)]
    pub pretty_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub path: String,
    pub url: String,
    pub node_index: Option<u32>,
    pub downloaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteState {
    Missing,
    Pending,
    BuildFailed,
    Completed,
    CollectionFailed,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteManifest {
    pub state: SuiteState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub circleci_job_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl Default for SuiteManifest {
    fn default() -> Self {
        Self {
            state: SuiteState::Missing,
            circleci_job_number: None,
            status_state: None,
            status_url: None,
            diagnostic: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitManifest {
    pub schema_version: u32,
    pub tool_version: String,
    pub repository: String,
    pub pr_number: u64,
    pub pr_url: String,
    pub pr_title: String,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_commit: Option<String>,
    pub resolved_commit: String,
    pub short_sha: String,
    pub directory_identity: String,
    pub collected_at: DateTime<Utc>,
    pub prerequisites: BTreeMap<String, Option<CommitStatus>>,
    pub suites: BTreeMap<String, SuiteManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestDurationStats {
    pub count: usize,
    pub total_seconds: f64,
    pub min_seconds: Option<f64>,
    pub median_seconds: Option<f64>,
    pub p95_seconds: Option<f64>,
    pub max_seconds: Option<f64>,
    pub slowest: Vec<SlowTest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowTest {
    pub name: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSummary {
    pub schema_version: u32,
    pub suite: Suite,
    pub repository: String,
    pub pr_number: u64,
    pub pr_url: String,
    pub commit: String,
    pub short_sha: String,
    pub github_status_context: String,
    pub github_status_state: String,
    pub github_status_url: String,
    pub circleci_job_number: u64,
    pub circleci_job_status: String,
    pub circleci_job_url: String,
    pub circleci_job_api: String,
    pub circleci_tests_api: String,
    pub queued_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub queue_duration_seconds: Option<f64>,
    pub wall_duration_seconds: Option<f64>,
    pub parallelism: Option<u32>,
    pub failed_node_indexes: Vec<u32>,
    pub test_count: usize,
    pub result_counts: BTreeMap<String, usize>,
    pub success_count: usize,
    pub failure_count: usize,
    pub skipped_count: usize,
    pub error_count: usize,
    pub unknown_count: usize,
    pub test_durations: TestDurationStats,
    pub artifact_count: usize,
    pub downloaded_artifact_count: usize,
    pub downloaded_artifact_bytes: u64,
    pub prerequisites: BTreeMap<String, Option<CommitStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub testcase_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureMetadata {
    pub stable_id: String,
    pub name: String,
    pub file: Option<String>,
    pub class_name: Option<String>,
    pub source: Option<String>,
    pub result: String,
    pub run_time: Option<f64>,
    pub node_index: Option<u32>,
    pub diff_extraction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub ok: bool,
    pub suite: Suite,
    pub repository: String,
    pub pr_number: u64,
    pub commit: String,
    pub circleci_job_number: u64,
    pub ci_status: String,
    pub test_count: usize,
    pub failure_count: usize,
    pub output_dir: PathBuf,
}

impl CommandResult {
    pub fn human_summary(&self) -> String {
        format!(
            "collected {} job {} for {}#{} at {}: {} tests, {} failures -> {}",
            self.suite,
            self.circleci_job_number,
            self.repository,
            self.pr_number,
            &self.commit[..7.min(self.commit.len())],
            self.test_count,
            self.failure_count,
            self.output_dir.display()
        )
    }
}

pub type StatusMap = HashMap<String, CommitStatus>;
