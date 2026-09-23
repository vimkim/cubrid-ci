use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::AppError;
use crate::gha_collect::{CollectResult, PrIdentity};
use crate::gha_evidence::BinaryConsumption;
use crate::model::Suite;
use crate::storage::write_json_immutable;

static OBSERVATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct CollectionObservation {
    id: String,
    directory: PathBuf,
    max_binary_total_bytes: u64,
    acquisition: AcquisitionLedger,
}

pub(crate) struct ObservationParameters<'a> {
    pub output_dir: &'a Path,
    pub repository: &'a str,
    pub pr: &'a PrIdentity,
    pub commit: &'a str,
    pub requested_suites: &'a [Suite],
    pub fetch_concurrency: usize,
    pub fetch_concurrency_source: &'a str,
    pub include_binaries: bool,
    pub max_binary_bytes: u64,
    pub max_binary_total_bytes: u64,
}

#[derive(Serialize)]
struct ObservationRequest<'a> {
    schema_version: u32,
    observation_id: &'a str,
    repository: &'a str,
    pr: &'a PrIdentity,
    commit: &'a str,
    started_at: DateTime<Utc>,
    requested_suites: &'a [Suite],
    fetch_concurrency: ConfiguredValue<'a>,
    binary_budget: BinaryBudgetRequest,
}

#[derive(Serialize)]
struct ConfiguredValue<'a> {
    value: usize,
    source: &'a str,
}

#[derive(Serialize)]
struct BinaryBudgetRequest {
    include: bool,
    per_file_bytes: u64,
    total_bytes: u64,
}

#[derive(Serialize)]
struct ObservationResult<'a> {
    schema_version: u32,
    observation_id: &'a str,
    finished_at: DateTime<Utc>,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a CollectResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ObservationError<'a>>,
    binary_budget: BinaryBudgetResult<'a>,
    acquisition: BTreeMap<String, SuiteAcquisition>,
}

#[derive(Serialize)]
struct ObservationError<'a> {
    kind: &'static str,
    diagnostic: &'static str,
    message: &'a str,
}

#[derive(Serialize)]
struct BinaryBudgetResult<'a> {
    configured_bytes: u64,
    consumed_bytes: u64,
    remaining_bytes: u64,
    artifacts: &'a [BinaryConsumption],
}

#[derive(Default)]
pub(crate) struct AcquisitionLedger {
    scheduling_stopped: Mutex<bool>,
    suites: Mutex<BTreeMap<String, SuiteAcquisition>>,
}

#[derive(Clone, Default, Serialize)]
struct SuiteAcquisition {
    shards: BTreeMap<String, ShardAcquisition>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<AcquisitionFailure>,
}

#[derive(Clone, Serialize)]
struct ShardAcquisition {
    state: ShardAcquisitionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<AcquisitionStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    consumed_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShardAcquisitionState {
    Retained,
    Failed,
    NotAttempted,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcquisitionStage {
    PlanMetadata,
    PlannedShardIndex,
    PlanTable,
    FailureInventory,
    Verdict,
    PublishedShardIndex,
    BuildProvenance,
    TestcaseProvenance,
    WorkflowResult,
    ShardCompletion,
    TestResultIndex,
    Junit,
}

impl AcquisitionStage {
    pub(crate) const fn order(self) -> usize {
        match self {
            Self::PlanMetadata => 0,
            Self::PlannedShardIndex => 1,
            Self::PlanTable => 2,
            Self::FailureInventory => 3,
            Self::Verdict => 4,
            Self::PublishedShardIndex => 5,
            Self::BuildProvenance => 6,
            Self::TestcaseProvenance => 7,
            Self::WorkflowResult => 8,
            Self::ShardCompletion => 9,
            Self::TestResultIndex => 10,
            Self::Junit => 11,
        }
    }
}

#[derive(Clone, Serialize)]
struct AcquisitionFailure {
    stage: AcquisitionStage,
    endpoint: String,
    consumed_bytes: u64,
    kind: String,
    diagnostic: String,
    message: String,
}

impl AcquisitionLedger {
    fn scheduling_state(&self) -> Result<std::sync::MutexGuard<'_, bool>, AppError> {
        self.scheduling_stopped.lock().map_err(|_| {
            AppError::storage(
                "collection observation scheduling lock",
                std::io::Error::other("lock poisoned"),
            )
        })
    }

    pub(crate) fn register(&self, suite: Suite, shards: &[String]) -> Result<(), AppError> {
        let mut suites = self.suites.lock().map_err(|_| {
            AppError::storage(
                "collection observation acquisition lock",
                std::io::Error::other("lock poisoned"),
            )
        })?;
        let suite = suites.entry(suite.job_name().to_owned()).or_default();
        for shard in shards {
            suite
                .shards
                .entry(shard.clone())
                .or_insert(ShardAcquisition {
                    state: ShardAcquisitionState::NotAttempted,
                    stage: None,
                    endpoint: None,
                    consumed_bytes: None,
                    kind: None,
                    diagnostic: None,
                    message: None,
                });
        }
        Ok(())
    }

    pub(crate) fn retained(&self, suite: Suite, shard: &str) -> Result<(), AppError> {
        self.record(
            suite,
            shard,
            ShardAcquisition {
                state: ShardAcquisitionState::Retained,
                stage: None,
                endpoint: None,
                consumed_bytes: None,
                kind: None,
                diagnostic: None,
                message: None,
            },
        )
    }

    pub(crate) fn failed(
        &self,
        suite: Suite,
        shard: &str,
        stage: AcquisitionStage,
        endpoint: Option<&str>,
        consumed_bytes: u64,
        error: &AppError,
    ) -> Result<(), AppError> {
        let mut stopped = self.scheduling_state()?;
        *stopped = true;
        self.record(
            suite,
            shard,
            ShardAcquisition {
                state: ShardAcquisitionState::Failed,
                stage: Some(stage),
                endpoint: endpoint.map(ToOwned::to_owned),
                consumed_bytes: Some(consumed_bytes),
                kind: Some(error.kind().as_str().to_owned()),
                diagnostic: Some(error.diagnostic_kind().to_owned()),
                message: Some(error.to_string()),
            },
        )
    }

    pub(crate) fn suite_failed(
        &self,
        suite: Suite,
        stage: AcquisitionStage,
        endpoint: &str,
        consumed_bytes: u64,
        error: &AppError,
    ) -> Result<(), AppError> {
        let mut stopped = self.scheduling_state()?;
        *stopped = true;
        let mut suites = self.suites.lock().map_err(|_| {
            AppError::storage(
                "collection observation acquisition lock",
                std::io::Error::other("lock poisoned"),
            )
        })?;
        suites
            .entry(suite.job_name().to_owned())
            .or_default()
            .errors
            .push(AcquisitionFailure {
                stage,
                endpoint: endpoint.to_owned(),
                consumed_bytes,
                kind: error.kind().as_str().to_owned(),
                diagnostic: error.diagnostic_kind().to_owned(),
                message: error.to_string(),
            });
        Ok(())
    }

    fn record(
        &self,
        suite: Suite,
        shard: &str,
        acquisition: ShardAcquisition,
    ) -> Result<(), AppError> {
        let mut suites = self.suites.lock().map_err(|_| {
            AppError::storage(
                "collection observation acquisition lock",
                std::io::Error::other("lock poisoned"),
            )
        })?;
        suites
            .entry(suite.job_name().to_owned())
            .or_default()
            .shards
            .insert(shard.to_owned(), acquisition);
        Ok(())
    }

    pub(crate) fn stop(&self) -> Result<(), AppError> {
        let mut stopped = self.scheduling_state()?;
        *stopped = true;
        Ok(())
    }

    pub(crate) fn schedule_if_running(
        &self,
        schedule: impl FnOnce() -> bool,
    ) -> Result<bool, AppError> {
        let stopped = self.scheduling_state()?;
        if *stopped {
            return Ok(false);
        }
        Ok(schedule())
    }

    fn snapshot(&self) -> Result<BTreeMap<String, SuiteAcquisition>, AppError> {
        self.suites
            .lock()
            .map(|suites| suites.clone())
            .map_err(|_| {
                AppError::storage(
                    "collection observation acquisition lock",
                    std::io::Error::other("lock poisoned"),
                )
            })
    }
}

impl CollectionObservation {
    pub(crate) fn begin(parameters: ObservationParameters<'_>) -> Result<Self, AppError> {
        let observations = parameters.output_dir.join("observations");
        fs::create_dir_all(&observations)
            .map_err(|source| AppError::storage(&observations, source))?;
        let started_at = Utc::now();
        let observation = loop {
            let sequence = OBSERVATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let id = format!(
                "{}-{}-{sequence}",
                started_at.format("%Y%m%dT%H%M%S%.9fZ"),
                std::process::id()
            );
            let directory = observations.join(&id);
            match fs::create_dir(&directory) {
                Ok(()) => {
                    break Self {
                        id,
                        directory,
                        max_binary_total_bytes: parameters.max_binary_total_bytes,
                        acquisition: AcquisitionLedger::default(),
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(AppError::storage(directory, error)),
            }
        };
        write_json_immutable(
            &observation.directory.join("request.json"),
            &ObservationRequest {
                schema_version: 1,
                observation_id: &observation.id,
                repository: parameters.repository,
                pr: parameters.pr,
                commit: parameters.commit,
                started_at,
                requested_suites: parameters.requested_suites,
                fetch_concurrency: ConfiguredValue {
                    value: parameters.fetch_concurrency,
                    source: parameters.fetch_concurrency_source,
                },
                binary_budget: BinaryBudgetRequest {
                    include: parameters.include_binaries,
                    per_file_bytes: parameters.max_binary_bytes,
                    total_bytes: parameters.max_binary_total_bytes,
                },
            },
        )?;
        Ok(observation)
    }

    pub(crate) fn finish(
        &self,
        command: &CollectResult,
        binary_bytes_remaining: u64,
        binary_consumption: &[BinaryConsumption],
    ) -> Result<(), AppError> {
        write_json_immutable(
            &self.directory.join("result.json"),
            &ObservationResult {
                schema_version: 1,
                observation_id: &self.id,
                finished_at: Utc::now(),
                outcome: if command.ok { "complete" } else { "incomplete" },
                command: Some(command),
                error: None,
                binary_budget: self.binary_budget(binary_bytes_remaining, binary_consumption),
                acquisition: self.acquisition.snapshot()?,
            },
        )
    }

    pub(crate) fn finish_error(
        &self,
        error: &AppError,
        binary_bytes_remaining: u64,
        binary_consumption: &[BinaryConsumption],
    ) -> Result<(), AppError> {
        let message = error.to_string();
        write_json_immutable(
            &self.directory.join("result.json"),
            &ObservationResult {
                schema_version: 1,
                observation_id: &self.id,
                finished_at: Utc::now(),
                outcome: "failed",
                command: None,
                error: Some(ObservationError {
                    kind: error.kind().as_str(),
                    diagnostic: error.diagnostic_kind(),
                    message: &message,
                }),
                binary_budget: self.binary_budget(binary_bytes_remaining, binary_consumption),
                acquisition: self.acquisition.snapshot()?,
            },
        )
    }

    fn binary_budget<'a>(
        &self,
        remaining_bytes: u64,
        artifacts: &'a [BinaryConsumption],
    ) -> BinaryBudgetResult<'a> {
        BinaryBudgetResult {
            configured_bytes: self.max_binary_total_bytes,
            consumed_bytes: self.max_binary_total_bytes - remaining_bytes,
            remaining_bytes,
            artifacts,
        }
    }

    pub(crate) fn acquisition(&self) -> &AcquisitionLedger {
        &self.acquisition
    }
}
