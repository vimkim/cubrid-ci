use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use futures_util::{StreamExt, TryStreamExt, stream};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio_util::io::StreamReader;
use url::Url;
use walkdir::WalkDir;

use crate::diff::{extract_diff, stable_test_id};
use crate::error::AppError;
use crate::model::Suite;
use crate::observation::{AcquisitionLedger, AcquisitionStage};
use crate::storage::{write_json_immutable, write_string_if_absent, write_string_immutable};

const MAX_INDEX_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_JUNIT_BYTES: usize = 32 * 1024 * 1024;
const MAX_GITHUB_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_GITHUB_LOG_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct SuiteLayout {
    remote_name: &'static str,
    result_file: &'static str,
    result_format: ResultFormat,
}

#[derive(Debug, Clone, Copy)]
enum ResultFormat {
    SummaryInfo,
    TestStatus,
}

impl SuiteLayout {
    const fn for_suite(suite: Suite) -> Self {
        match suite {
            Suite::Medium => Self {
                remote_name: "medium",
                result_file: "summary_info",
                result_format: ResultFormat::SummaryInfo,
            },
            Suite::Sql => Self {
                remote_name: "sql",
                result_file: "summary_info",
                result_format: ResultFormat::SummaryInfo,
            },
            Suite::Shell => Self {
                remote_name: "shell",
                result_file: "test_status.data",
                result_format: ResultFormat::TestStatus,
            },
        }
    }
}

pub(crate) struct SuiteRequest<'a> {
    pub suite: Suite,
    pub run_id: u64,
    pub attempt: u64,
    pub commit: &'a str,
    pub ci_state: &'a str,
    pub artifact_base: Option<&'a Url>,
    pub evidence_dir: &'a Path,
    pub status_snapshot_json: &'a [u8],
    pub run_json: &'a [u8],
    pub run_context: &'a RunContextCache,
    pub lineage: &'a [u64],
    pub fetch_permits: &'a Semaphore,
    pub fetch_concurrency: usize,
    pub acquisition: &'a AcquisitionLedger,
    pub include_binaries: bool,
    pub max_binary_bytes: u64,
    pub binary_bytes_remaining: &'a mut u64,
    pub binary_consumption: &'a mut Vec<BinaryConsumption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSummary {
    pub schema_version: u32,
    pub suite: String,
    pub ci_state: String,
    pub collection_state: String,
    pub run_id: u64,
    pub attempt: u64,
    pub artifact_base: String,
    pub verdict: String,
    pub counts: TestCounts,
    pub shards: Vec<ShardSummary>,
    pub failures: Vec<FailureRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun: Option<RerunSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerunSource {
    pub mode: RerunMode,
    pub run_id: u64,
    pub attempt: u64,
    pub summary: PathBuf,
    pub summary_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerunMode {
    Partial,
    CarriedForward,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestCounts {
    pub planned: u64,
    pub run: u64,
    pub unrun: u64,
    pub tests: u64,
    pub passed: u64,
    pub failures: u64,
    pub errors: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardSummary {
    pub index: String,
    pub build: BuildProvenance,
    pub testcases: TestcaseProvenance,
    pub junit_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildProvenance {
    pub sha: String,
    pub mode: Option<String>,
    pub namespace: Option<String>,
    pub run_id: u64,
    pub run_attempt: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestcaseProvenance {
    pub sha: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub stable_id: String,
    pub name: String,
    pub shard: String,
    pub class_name: Option<String>,
    pub duration_seconds: Option<f64>,
    pub result: String,
    pub diff_extraction: String,
    pub message_path: PathBuf,
    pub diff_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BinaryInventory {
    schema_version: u32,
    files: Vec<BinaryRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BinaryRecord {
    shard: String,
    name: String,
    state: String,
    reason: Option<String>,
    size_bytes: Option<u64>,
    sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BinaryConsumption {
    pub suite: String,
    pub shard: String,
    pub name: String,
    pub consumed_bytes: u64,
}

#[derive(Debug, Serialize)]
struct FailureMetadata<'a> {
    schema_version: u32,
    stable_id: &'a str,
    name: &'a str,
    shard: &'a str,
    class_name: &'a Option<String>,
    duration_seconds: Option<f64>,
    result: &'a str,
    diff_extraction: &'a str,
    message_path: &'a Path,
    diff_path: &'a Option<PathBuf>,
}

#[derive(Debug)]
struct JunitCase {
    name: String,
    class_name: Option<String>,
    duration_seconds: Option<f64>,
    failure: Option<String>,
    error: Option<String>,
    skipped: bool,
}

struct FetchedShard {
    name: String,
    root: Url,
    junit_files: Vec<String>,
}

struct ShardFetchError {
    stage: AcquisitionStage,
    endpoint: Option<String>,
    consumed_bytes: u64,
    error: AppError,
}

struct FetchFailure {
    endpoint: String,
    consumed_bytes: u64,
    error: AppError,
}

impl FetchFailure {
    fn new(endpoint: &Url, consumed_bytes: u64, error: AppError) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            consumed_bytes,
            error,
        }
    }
}

impl From<FetchFailure> for AppError {
    fn from(failure: FetchFailure) -> Self {
        failure.error
    }
}

fn at_stage<T>(stage: AcquisitionStage, result: Result<T, AppError>) -> Result<T, ShardFetchError> {
    result.map_err(|error| ShardFetchError {
        stage,
        endpoint: None,
        consumed_bytes: 0,
        error,
    })
}

fn at_fetch_stage<T>(
    stage: AcquisitionStage,
    result: Result<T, FetchFailure>,
) -> Result<T, ShardFetchError> {
    result.map_err(|failure| ShardFetchError {
        stage,
        endpoint: Some(failure.endpoint),
        consumed_bytes: failure.consumed_bytes,
        error: failure.error,
    })
}

fn record_suite_fetch<T>(
    result: Result<T, FetchFailure>,
    acquisition: &AcquisitionLedger,
    suite: Suite,
    stage: AcquisitionStage,
) -> Result<T, AppError> {
    match result {
        Ok(value) => Ok(value),
        Err(failure) => {
            acquisition.suite_failed(
                suite,
                stage,
                &failure.endpoint,
                failure.consumed_bytes,
                &failure.error,
            )?;
            Err(failure.error)
        }
    }
}

pub(crate) async fn collect_suite(request: SuiteRequest<'_>) -> Result<SuiteSummary, AppError> {
    if request.lineage.contains(&request.run_id) || request.lineage.len() >= 32 {
        return Err(AppError::Integrity(
            "rerun source lineage is cyclic or exceeds 32 runs".to_owned(),
        ));
    }
    let layout = SuiteLayout::for_suite(request.suite);
    let execution_suite_dir = request
        .evidence_dir
        .join("providers/github-actions/runs")
        .join(request.run_id.to_string())
        .join("attempts")
        .join(request.attempt.to_string())
        .join(request.suite.job_name());
    let provider_raw = execution_suite_dir.join("raw");
    let suite_dir = execution_suite_dir;
    let summary_path = suite_dir.join("summary.json");
    let raw_index_path = provider_raw.join("index.json");
    let expected_summary_digest = if raw_index_path.exists() {
        Some(validate_raw_index(
            &provider_raw,
            request.run_id,
            request.attempt,
            request.suite.job_name(),
        )?)
    } else if summary_path.exists() {
        return Err(AppError::Integrity(format!(
            "stored summary has no raw evidence index: {}",
            summary_path.display()
        )));
    } else {
        None
    };
    if summary_path.exists()
        && (!request.include_binaries || suite_dir.join("binary-inventory.json").exists())
    {
        let bytes =
            fs::read(&summary_path).map_err(|source| AppError::storage(&summary_path, source))?;
        validate_summary_schema(&bytes)?;
        let actual_summary_digest = hex::encode(Sha256::digest(&bytes));
        if expected_summary_digest.as_deref() != Some(actual_summary_digest.as_str()) {
            return Err(AppError::Integrity(format!(
                "stored summary digest disagrees with the raw evidence index: {}",
                summary_path.display()
            )));
        }
        let summary: SuiteSummary =
            serde_json::from_slice(&bytes).map_err(|source| AppError::Json {
                endpoint: summary_path.display().to_string(),
                source,
            })?;
        if summary.run_id != request.run_id
            || summary.attempt != request.attempt
            || summary.suite != request.suite.job_name()
            || summary
                .commit
                .as_deref()
                .is_some_and(|commit| commit != request.commit)
            || summary
                .shards
                .iter()
                .any(|shard| !shard.build.sha.eq_ignore_ascii_case(request.commit))
        {
            return Err(AppError::Integrity(format!(
                "stored immutable summary does not match {} run {} attempt {}",
                request.suite, request.run_id, request.attempt
            )));
        }
        if let Some(source) = &summary.rerun {
            validate_rerun_source(&request, source).await?;
        }
        if request.include_binaries {
            validate_binary_inventory(&suite_dir)?;
        }
        let shard_names = summary
            .shards
            .iter()
            .map(|shard| shard.index.clone())
            .collect::<Vec<_>>();
        request.acquisition.register(request.suite, &shard_names)?;
        for shard in &shard_names {
            request.acquisition.retained(request.suite, shard)?;
        }
        return Ok(summary);
    }
    write_string_if_absent(
        &provider_raw.join("github/status-snapshot.json"),
        &String::from_utf8_lossy(request.status_snapshot_json),
    )?;
    let run_endpoint = format!("repos/CUBRID/cubrid/actions/runs/{}", request.run_id);
    let run_json = request.run_json;
    let run: ActionRun = serde_json::from_slice(run_json).map_err(|source| AppError::Json {
        endpoint: run_endpoint,
        source,
    })?;
    if run.id != request.run_id || run.run_attempt != request.attempt {
        return Err(AppError::Integrity(
            "Actions run metadata changed after suite selection".to_owned(),
        ));
    }
    let retained_run = provider_raw.join("github/run.json");
    if retained_run.exists() {
        let existing: serde_json::Value = serde_json::from_slice(
            &fs::read(&retained_run).map_err(|source| AppError::storage(&retained_run, source))?,
        )?;
        let selected: serde_json::Value = serde_json::from_slice(run_json)?;
        if existing != selected {
            return Err(AppError::Integrity(
                "stored Actions run metadata disagrees with the pinned execution".to_owned(),
            ));
        }
    } else {
        write_string_immutable(&retained_run, &String::from_utf8_lossy(run_json))?;
    }
    let run_context = {
        let mut cached = request
            .run_context
            .lock()
            .map_err(|_| AppError::Remote("shared Actions run context lock poisoned".to_owned()))?;
        let context = if let Some(context) = cached.as_ref() {
            context.clone()
        } else {
            let context = load_run_context(
                request.run_id,
                request.attempt,
                layout.remote_name,
                request.artifact_base,
                &provider_raw,
            )
            .map(Arc::new)
            .map_err(Arc::new);
            *cached = Some(context.clone());
            context
        };
        context.map_err(AppError::Shared)?
    };
    let resolution = resolve_artifact_base(&run_context, layout.remote_name, &provider_raw)?;
    let artifact_base = resolution.base;
    validate_artifact_base(&artifact_base)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AppError::Remote(format!("create evidence-server client: {error}")))?;
    let run_root = artifact_base
        .join(&format!("runs/{}/{}/", request.run_id, layout.remote_name))
        .map_err(|error| AppError::Remote(format!("construct evidence URL: {error}")))?;

    let split_meta = record_suite_fetch(
        classify_pipeline_fetch(
            fetch_text(
                &client,
                request.fetch_permits,
                run_root.join("plan/split.meta").unwrap(),
                MAX_INDEX_BYTES,
                &provider_raw.join("plan/split.meta"),
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            "plan metadata",
            false,
        ),
        request.acquisition,
        request.suite,
        AcquisitionStage::PlanMetadata,
    )?;
    let plan = parse_split_meta(&split_meta)?;
    let source_run = rerun_source_run(&run_context, &plan, &provider_raw)?;
    let source = if let Some(source_run) = source_run {
        Some(collect_rerun_source(&request, source_run, &provider_raw).await?)
    } else {
        None
    };
    let planned_index = record_suite_fetch(
        classify_pipeline_fetch(
            fetch_text(
                &client,
                request.fetch_permits,
                run_root.join("plan/shards/").unwrap(),
                MAX_INDEX_BYTES,
                &provider_raw.join("indexes/planned-shards.html"),
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            "planned-shard index",
            false,
        ),
        request.acquisition,
        request.suite,
        AcquisitionStage::PlannedShardIndex,
    )?;
    let planned_shards = directory_entries(&planned_index, false)?
        .into_iter()
        .filter_map(|name| name.strip_suffix(".list").map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    let suite = request.suite;
    request.acquisition.register(suite, &planned_shards)?;
    let plan_table_text = record_suite_fetch(
        classify_pipeline_fetch(
            fetch_text(
                &client,
                request.fetch_permits,
                run_root.join("plan/plan.tsv").unwrap(),
                MAX_TEXT_BYTES,
                &provider_raw.join("plan/plan.tsv"),
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            "plan table",
            false,
        ),
        request.acquisition,
        request.suite,
        AcquisitionStage::PlanTable,
    )?;
    let plan_table = if source.is_some() && plan.total == 0 && plan_table_text.trim().is_empty() {
        BTreeMap::new()
    } else {
        parse_plan_table(&plan_table_text)?
    };
    if plan_table.values().sum::<u64>() != plan.total {
        return Err(AppError::Integrity(
            "plan.tsv assigned counts do not add up to split.meta total".to_owned(),
        ));
    }

    let failed_list = record_suite_fetch(
        classify_pipeline_fetch(
            fetch_text(
                &client,
                request.fetch_permits,
                run_root.join("collect/failed.list").unwrap(),
                MAX_TEXT_BYTES,
                &provider_raw.join("collect/failed.list"),
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            "collector failure inventory",
            true,
        ),
        request.acquisition,
        request.suite,
        AcquisitionStage::FailureInventory,
    )?;
    let verdict = record_suite_fetch(
        classify_pipeline_fetch(
            fetch_text(
                &client,
                request.fetch_permits,
                run_root.join("collect/verdict").unwrap(),
                MAX_INDEX_BYTES,
                &provider_raw.join("collect/verdict"),
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            "collector verdict",
            true,
        ),
        request.acquisition,
        request.suite,
        AcquisitionStage::Verdict,
    )?;

    let shard_index = record_suite_fetch(
        classify_pipeline_fetch(
            fetch_text(
                &client,
                request.fetch_permits,
                run_root.join("shard/").unwrap(),
                MAX_INDEX_BYTES,
                &provider_raw.join("indexes/shards.html"),
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            "published-shard index",
            true,
        ),
        request.acquisition,
        request.suite,
        AcquisitionStage::PublishedShardIndex,
    )?;
    let shard_names = directory_entries(&shard_index, true)?;
    if let Some((source_summary, source_link)) = &source
        && plan.total == 0
    {
        if plan.parallelism != 0
            || !planned_shards.is_empty()
            || !shard_names.is_empty()
            || !plan_table.is_empty()
            || !failed_list.trim().is_empty()
        {
            return Err(AppError::Integrity(
                "zero-case rerun has shard or failure evidence".to_owned(),
            ));
        }
        if !plan.carry_forward {
            return Err(AppError::Unavailable("zero-case rerun changed or removed source cases; no carried-forward verdict can be established".to_owned()));
        }
        let verdict = verdict.trim();
        if verdict != source_summary.verdict
            || !(request.ci_state.eq_ignore_ascii_case("unreported")
                || request
                    .ci_state
                    .eq_ignore_ascii_case(&source_summary.ci_state))
        {
            return Err(AppError::Integrity(
                "carried-forward status and verdict disagree with the validated source suite"
                    .to_owned(),
            ));
        }
        let summary = SuiteSummary {
            schema_version: 3,
            suite: request.suite.job_name().to_owned(),
            ci_state: source_summary.ci_state.clone(),
            collection_state: "complete".to_owned(),
            run_id: request.run_id,
            attempt: request.attempt,
            artifact_base: artifact_base.to_string(),
            verdict: verdict.to_owned(),
            counts: TestCounts::default(),
            shards: Vec::new(),
            failures: Vec::new(),
            commit: Some(request.commit.to_owned()),
            rerun: Some(RerunSource {
                mode: RerunMode::CarriedForward,
                ..source_link.clone()
            }),
        };
        return publish_summary(&request, &provider_raw, &suite_dir, summary, Vec::new());
    }
    if shard_names.is_empty() {
        return job_level_failure(
            format!("terminal {} suite has no shard directories", request.suite),
            &resolution.jobs,
            &provider_raw,
        );
    }
    if shard_names != planned_shards || shard_names.len() as u64 != plan.parallelism {
        return job_level_failure(
            format!(
                "planned {} shards ({}) do not match published shards ({})",
                request.suite,
                planned_shards.len(),
                shard_names.len()
            ),
            &resolution.jobs,
            &provider_raw,
        );
    }
    if plan_table.keys().cloned().collect::<Vec<_>>() != planned_shards {
        return Err(AppError::Integrity(
            "plan.tsv shard identities disagree with planned shard files".to_owned(),
        ));
    }

    let make_fetch = |shard: String| {
        let shard_root = run_root.join(&format!("shard/{shard}/")).unwrap();
        let client = &client;
        let fetch_permits = request.fetch_permits;
        let jobs = &resolution.jobs;
        let provider_raw = &provider_raw;
        async move {
            let shard_name = shard.clone();
            let result = async {
                at_fetch_stage(
                    AcquisitionStage::BuildProvenance,
                    classify_pipeline_fetch(
                        fetch_text(
                            client,
                            fetch_permits,
                            shard_root.join("build.read").unwrap(),
                            MAX_INDEX_BYTES,
                            &provider_raw.join(format!("shards/{shard}/build.read")),
                        )
                        .await,
                        jobs,
                        provider_raw,
                        &format!("shard {shard} build provenance"),
                        false,
                    ),
                )?;
                at_fetch_stage(
                    AcquisitionStage::TestcaseProvenance,
                    classify_pipeline_fetch(
                        fetch_text(
                            client,
                            fetch_permits,
                            shard_root.join("tc.read").unwrap(),
                            MAX_INDEX_BYTES,
                            &provider_raw.join(format!("shards/{shard}/tc.read")),
                        )
                        .await,
                        jobs,
                        provider_raw,
                        &format!("shard {shard} testcase provenance"),
                        false,
                    ),
                )?;
                at_fetch_stage(
                    AcquisitionStage::WorkflowResult,
                    classify_pipeline_fetch(
                        fetch_text(
                            client,
                            fetch_permits,
                            shard_root.join(layout.result_file).unwrap(),
                            MAX_TEXT_BYTES,
                            &provider_raw.join(format!("shards/{shard}/{}", layout.result_file)),
                        )
                        .await,
                        jobs,
                        provider_raw,
                        &format!("shard {shard} workflow result"),
                        true,
                    ),
                )?;
                at_fetch_stage(
                    AcquisitionStage::ShardCompletion,
                    classify_pipeline_fetch(
                        fetch_text(
                            client,
                            fetch_permits,
                            shard_root.join("shard.done").unwrap(),
                            MAX_INDEX_BYTES,
                            &provider_raw.join(format!("shards/{shard}/shard.done")),
                        )
                        .await,
                        jobs,
                        provider_raw,
                        &format!("shard {shard} completion record"),
                        true,
                    ),
                )?;
                let results_index = at_fetch_stage(
                    AcquisitionStage::TestResultIndex,
                    classify_pipeline_fetch(
                        fetch_text(
                            client,
                            fetch_permits,
                            shard_root.join("test-results/").unwrap(),
                            MAX_INDEX_BYTES,
                            &provider_raw.join(format!("indexes/shard-{shard}-results.html")),
                        )
                        .await,
                        jobs,
                        provider_raw,
                        &format!("shard {shard} test-result index"),
                        true,
                    ),
                )?;
                let junit_names = at_stage(
                    AcquisitionStage::TestResultIndex,
                    directory_entries(&results_index, false),
                )?
                .into_iter()
                .filter(|name| name.ends_with(".xml"))
                .collect::<Vec<_>>();
                if junit_names.is_empty() {
                    return at_stage(
                        AcquisitionStage::Junit,
                        job_level_failure(
                            format!("shard {shard} has no {suite} JUnit XML"),
                            jobs,
                            provider_raw,
                        ),
                    );
                }
                let junit_files = stream::iter(junit_names.into_iter().map(|file| {
                    let url = shard_root.join(&format!("test-results/{file}")).unwrap();
                    let destination = provider_raw.join(format!("shards/{shard}/{file}"));
                    async move {
                        at_fetch_stage(
                            AcquisitionStage::Junit,
                            fetch_text(client, fetch_permits, url, MAX_JUNIT_BYTES, &destination)
                                .await,
                        )?;
                        Ok::<_, ShardFetchError>(file)
                    }
                }))
                .buffered(4)
                .try_collect()
                .await?;
                Ok::<_, ShardFetchError>(FetchedShard {
                    name: shard,
                    root: shard_root,
                    junit_files,
                })
            }
            .await;
            (shard_name, result)
        }
    };
    let mut remaining = shard_names.into_iter();
    let mut in_flight = futures_util::stream::FuturesUnordered::new();
    for _ in 0..request.fetch_concurrency {
        if !request.acquisition.schedule_if_running(|| {
            let Some(shard) = remaining.next() else {
                return false;
            };
            in_flight.push(make_fetch(shard));
            true
        })? {
            break;
        }
    }
    let mut fetched_shards = Vec::new();
    let mut fetch_errors = Vec::new();
    while let Some((shard, result)) = in_flight.next().await {
        match result {
            Ok(fetched) => {
                request.acquisition.retained(suite, &shard)?;
                fetched_shards.push(fetched);
            }
            Err(error) => {
                request.acquisition.failed(
                    suite,
                    &shard,
                    error.stage,
                    error.endpoint.as_deref(),
                    error.consumed_bytes,
                    &error.error,
                )?;
                fetch_errors.push((shard, error));
            }
        }
        request.acquisition.schedule_if_running(|| {
            let Some(shard) = remaining.next() else {
                return false;
            };
            in_flight.push(make_fetch(shard));
            true
        })?;
    }
    if !fetch_errors.is_empty() {
        fetch_errors.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.stage.order().cmp(&right.1.stage.order()))
        });
        return Err(fetch_errors.remove(0).1.error);
    }
    if remaining.len() > 0 {
        return Err(AppError::Unavailable(
            "shard acquisition stopped after another mandatory shard failed".to_owned(),
        ));
    }
    fetched_shards.sort_by(|left, right| left.name.cmp(&right.name));

    let failed_inventory = parse_failed_list(&failed_list)?;
    let failed_inventory_count = failed_inventory.len() as u64;
    let mut inventory_by_shard: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (shard, name) in &failed_inventory {
        if !inventory_by_shard
            .entry(shard.clone())
            .or_default()
            .insert(name.clone())
        {
            return Err(AppError::Integrity(format!(
                "failed-case inventory repeats {shard}/{name}"
            )));
        }
    }
    let mut shards = Vec::new();
    let mut counts = TestCounts::default();
    let mut junit_failures: BTreeMap<(String, String), JunitCase> = BTreeMap::new();
    let mut selected_build: Option<BuildProvenance> = None;
    let mut selected_testcases: Option<TestcaseProvenance> = None;
    let mut workflow_passed = 0_u64;
    let mut workflow_failed = 0_u64;
    let mut workflow_skipped = 0_u64;
    let mut binary_records = Vec::new();

    for fetched in fetched_shards {
        let FetchedShard {
            name: shard,
            root: shard_root,
            junit_files,
        } = fetched;
        let build_text = read_bounded_text(
            &provider_raw.join(format!("shards/{shard}/build.read")),
            MAX_INDEX_BYTES,
        )?;
        let testcase_text = read_bounded_text(
            &provider_raw.join(format!("shards/{shard}/tc.read")),
            MAX_INDEX_BYTES,
        )?;
        let build = parse_build_provenance(&build_text)?;
        if !build.sha.eq_ignore_ascii_case(request.commit) {
            return Err(AppError::Integrity(format!(
                "shard {shard} build SHA {} does not match selected commit {}",
                build.sha, request.commit
            )));
        }
        if build.run_attempt.is_none() {
            return Err(AppError::Integrity(format!(
                "shard {shard} build provenance has no producing attempt"
            )));
        }
        if selected_build
            .as_ref()
            .is_some_and(|selected| selected != &build)
        {
            return Err(AppError::Integrity(format!(
                "{} shards do not identify one reused build execution",
                request.suite
            )));
        }
        selected_build.get_or_insert_with(|| build.clone());
        let testcases = parse_testcase_provenance(&testcase_text)?;
        if selected_testcases
            .as_ref()
            .is_some_and(|selected| selected != &testcases)
        {
            return Err(AppError::Integrity(format!(
                "{} shards do not identify one testcase revision and branch",
                request.suite
            )));
        }
        selected_testcases.get_or_insert_with(|| testcases.clone());
        if testcases.sha != plan.testcase_sha || testcases.branch != plan.testcase_branch {
            return Err(AppError::Integrity(format!(
                "shard {shard} testcase provenance disagrees with the {} plan",
                request.suite
            )));
        }

        let workflow_result = read_bounded_text(
            &provider_raw.join(format!("shards/{shard}/{}", layout.result_file)),
            MAX_TEXT_BYTES,
        )?;
        let shard_done = read_bounded_text(
            &provider_raw.join(format!("shards/{shard}/shard.done")),
            MAX_INDEX_BYTES,
        )?;
        let shard_counts = match layout.result_format {
            ResultFormat::SummaryInfo => parse_summary_info(&workflow_result)?,
            ResultFormat::TestStatus => parse_test_status(&workflow_result)?,
        };
        let assigned =
            match validate_shard_done(&shard_done, &shard, layout.remote_name, shard_counts.failed)
            {
                Ok(assigned) => assigned,
                Err(AppError::JobLevel(message)) => {
                    return job_level_failure(message, &resolution.jobs, &provider_raw);
                }
                Err(error) => return Err(error),
            };
        let planned = plan_table
            .get(&shard)
            .copied()
            .ok_or_else(|| AppError::Integrity(format!("shard {shard} is absent from plan.tsv")))?;
        if assigned != planned {
            return Err(AppError::Integrity(format!(
                "shard {shard} completion says {assigned} assigned but plan.tsv says {planned}"
            )));
        }
        if shard_counts.passed + shard_counts.failed + shard_counts.skipped != planned {
            return job_level_failure(
                format!(
                    "shard {shard} handled {} cases but was assigned {planned}",
                    shard_counts.passed + shard_counts.failed + shard_counts.skipped
                ),
                &resolution.jobs,
                &provider_raw,
            );
        }
        workflow_passed += shard_counts.passed;
        workflow_failed += shard_counts.failed;
        workflow_skipped += shard_counts.skipped;

        if request.include_binaries && shard_counts.failed > 0 {
            collect_abnormal_shard_binaries(
                &client,
                &shard_root,
                &suite_dir,
                &shard,
                BinaryCollection {
                    per_file_limit: request.max_binary_bytes,
                    total_remaining: request.binary_bytes_remaining,
                    fetch_permits: request.fetch_permits,
                    suite: request.suite.job_name(),
                    consumption: request.binary_consumption,
                    records: &mut binary_records,
                },
            )
            .await?;
        }

        let _results_index = read_bounded_text(
            &provider_raw.join(format!("indexes/shard-{shard}-results.html")),
            MAX_INDEX_BYTES,
        )?;

        let mut junit_shard_counts = WorkflowCounts::default();
        for file in &junit_files {
            let xml = read_bounded_text(
                &provider_raw.join(format!("shards/{shard}/{file}")),
                MAX_JUNIT_BYTES,
            )?;
            for case in parse_junit(&xml)? {
                counts.tests += 1;
                if case.failure.is_some() {
                    counts.failures += 1;
                    junit_shard_counts.failed += 1;
                    junit_shard_counts.failed_names.insert(case.name.clone());
                    if junit_failures
                        .insert((shard.clone(), case.name.clone()), case)
                        .is_some()
                    {
                        return Err(AppError::Integrity(format!(
                            "JUnit repeats a failed testcase in shard {shard}"
                        )));
                    }
                } else if case.error.is_some() {
                    counts.errors += 1;
                    junit_shard_counts.failed += 1;
                    junit_shard_counts.failed_names.insert(case.name.clone());
                    if junit_failures
                        .insert((shard.clone(), case.name.clone()), case)
                        .is_some()
                    {
                        return Err(AppError::Integrity(format!(
                            "JUnit repeats an errored testcase in shard {shard}"
                        )));
                    }
                } else if case.skipped {
                    counts.skipped += 1;
                    junit_shard_counts.skipped += 1;
                } else {
                    junit_shard_counts.passed += 1;
                }
            }
        }
        if junit_shard_counts.passed != shard_counts.passed
            || junit_shard_counts.failed != shard_counts.failed
            || junit_shard_counts.skipped != shard_counts.skipped
        {
            return Err(AppError::Integrity(format!(
                "shard {shard} workflow result and JUnit counts disagree"
            )));
        }
        let inventory_names = inventory_by_shard.get(&shard).cloned().unwrap_or_default();
        if junit_shard_counts.failed_names != inventory_names {
            return Err(AppError::Integrity(format!(
                "shard {shard} failed-list and JUnit testcase identities disagree"
            )));
        }
        if matches!(layout.result_format, ResultFormat::SummaryInfo)
            && shard_counts.failed_names != inventory_names
        {
            return Err(AppError::Integrity(format!(
                "shard {shard} summary_info and failed-list testcase identities disagree"
            )));
        }
        shards.push(ShardSummary {
            index: shard,
            build,
            testcases,
            junit_files,
        });
    }

    let mut failures = Vec::new();
    for (shard, name) in failed_inventory {
        let case = junit_failures
            .remove(&(shard.clone(), name.clone()))
            .ok_or_else(|| {
                AppError::Integrity(format!(
                    "failed-case inventory entry {shard}/{name} has no matching JUnit failure"
                ))
            })?;
        let (result, message) = match (case.failure, case.error) {
            (Some(message), _) => ("failure", message),
            (_, Some(message)) => ("error", message),
            _ => unreachable!("failure map contains only failed or errored cases"),
        };
        let stable_id = stable_test_id(&name);
        let failure_dir = suite_dir.join("failures").join(&stable_id);
        let message_path = PathBuf::from(format!("failures/{stable_id}/message.txt"));
        write_string_immutable(&failure_dir.join("message.txt"), &message)?;
        let diff = extract_diff(&message);
        let diff_path = diff
            .as_ref()
            .map(|_| PathBuf::from(format!("failures/{stable_id}/diff.txt")));
        if let Some(diff) = &diff {
            write_string_immutable(&failure_dir.join("diff.txt"), &format!("{diff}\n"))?;
        }
        let record = FailureRecord {
            stable_id: stable_id.clone(),
            name,
            shard,
            class_name: case.class_name,
            duration_seconds: case.duration_seconds,
            result: result.to_owned(),
            diff_extraction: if diff.is_some() {
                "extracted".to_owned()
            } else {
                "not_available".to_owned()
            },
            message_path,
            diff_path,
        };
        write_json_immutable(
            &failure_dir.join("metadata.json"),
            &FailureMetadata {
                schema_version: 2,
                stable_id: &record.stable_id,
                name: &record.name,
                shard: &record.shard,
                class_name: &record.class_name,
                duration_seconds: record.duration_seconds,
                result: &record.result,
                diff_extraction: &record.diff_extraction,
                message_path: &record.message_path,
                diff_path: &record.diff_path,
            },
        )?;
        failures.push(record);
    }
    if !junit_failures.is_empty() {
        return Err(AppError::Integrity(
            "JUnit reports failures absent from collect/failed.list".to_owned(),
        ));
    }

    let junit_failure_count = counts.failures + counts.errors;
    if junit_failure_count != failed_inventory_count {
        return Err(AppError::Integrity(format!(
            "failed-case inventory has {failed_inventory_count} entries but JUnit has {junit_failure_count} failures"
        )));
    }
    counts.planned = plan.total;
    counts.run = workflow_passed + workflow_failed;
    counts.unrun = plan
        .total
        .saturating_sub(workflow_passed + workflow_failed + workflow_skipped);
    counts.passed = counts
        .tests
        .saturating_sub(counts.failures + counts.errors + counts.skipped);
    if workflow_passed != counts.passed
        || workflow_failed != junit_failure_count
        || workflow_skipped != counts.skipped
        || plan.total != workflow_passed + workflow_failed + workflow_skipped
    {
        return Err(AppError::Integrity(format!(
            "plan, workflow result, and JUnit counts disagree (planned {}, workflow {}/{}/{}, JUnit {}/{}/{})",
            plan.total,
            workflow_passed,
            workflow_failed,
            workflow_skipped,
            counts.passed,
            junit_failure_count,
            counts.skipped
        )));
    }
    let verdict = verdict.trim();
    let status_success = request.ci_state.eq_ignore_ascii_case("success");
    let status_failure = request.ci_state.eq_ignore_ascii_case("failure");
    // Explicit run selection has no attached suite status. The producer verdict
    // and independently reconciled testcase records establish the outcome.
    let unreported = request.ci_state.eq_ignore_ascii_case("unreported");
    let records_agree = (status_success && verdict == "pass" && junit_failure_count == 0)
        || (status_failure && verdict == "fail" && junit_failure_count > 0)
        || (unreported
            && ((verdict == "pass" && junit_failure_count == 0)
                || (verdict == "fail" && junit_failure_count > 0)));
    if !records_agree {
        return Err(AppError::Integrity(format!(
            "suite status {}, verdict {verdict}, and JUnit failure count {junit_failure_count} disagree",
            request.ci_state
        )));
    }

    let summary = SuiteSummary {
        schema_version: if source.is_some() { 3 } else { 2 },
        suite: request.suite.job_name().to_owned(),
        ci_state: if unreported {
            if verdict == "pass" {
                "success"
            } else {
                "failure"
            }
            .to_owned()
        } else {
            request.ci_state.to_ascii_lowercase()
        },
        collection_state: "complete".to_owned(),
        run_id: request.run_id,
        attempt: request.attempt,
        artifact_base: artifact_base.to_string(),
        verdict: verdict.to_owned(),
        counts,
        shards,
        failures,
        commit: source.as_ref().map(|_| request.commit.to_owned()),
        rerun: source.map(|(_, link)| link),
    };
    publish_summary(&request, &provider_raw, &suite_dir, summary, binary_records)
}

fn publish_summary(
    request: &SuiteRequest<'_>,
    provider_raw: &Path,
    suite_dir: &Path,
    summary: SuiteSummary,
    binary_records: Vec<BinaryRecord>,
) -> Result<SuiteSummary, AppError> {
    if !request.lineage.is_empty() {
        let current: ActionRun = serde_json::from_slice(&gh_output(&format!(
            "repos/CUBRID/cubrid/actions/runs/{}",
            request.run_id
        ))?)?;
        let pinned: ActionRun = serde_json::from_slice(request.run_json)?;
        if current.id != request.run_id
            || current.run_attempt != request.attempt
            || current.status.as_deref() != Some("completed")
            || current.updated_at != pinned.updated_at
        {
            return Err(AppError::Unavailable(
                "source execution changed while its evidence was being collected".to_owned(),
            ));
        }
    }
    let summary_bytes = serde_json::to_vec_pretty(&summary)?;
    validate_summary_schema(&summary_bytes)?;
    write_raw_index(
        provider_raw,
        request.run_id,
        request.attempt,
        request.suite.job_name(),
        &hex::encode(Sha256::digest(&summary_bytes)),
    )?;
    if request.include_binaries {
        write_json_immutable(
            &suite_dir.join("binary-inventory.json"),
            &BinaryInventory {
                schema_version: 2,
                files: binary_records,
            },
        )?;
    }
    write_json_immutable(&suite_dir.join("summary.json"), &summary)?;
    Ok(summary)
}

fn validate_summary_schema(bytes: &[u8]) -> Result<(), AppError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let (name, schema) = if value["schema_version"] == 3 {
        (
            "suite-summary-v3",
            include_str!("../schema/suite-summary-v3.schema.json"),
        )
    } else {
        (
            "suite-summary-v2",
            include_str!("../schema/suite-summary-v2.schema.json"),
        )
    };
    validate_json_schema(name, schema, bytes)
}

fn rerun_source_run(
    context: &RunContext,
    plan: &SqlPlan,
    raw: &Path,
) -> Result<Option<u64>, AppError> {
    let log = if let Some(log) = &context.collect_log {
        String::from_utf8_lossy(log).into_owned()
    } else if plan.rerun_since.is_some() {
        let job = context
            .jobs
            .iter()
            .find(|job| job.name == "collect")
            .ok_or_else(|| {
                AppError::Unavailable("rerun source requires a collect-job log".to_owned())
            })?;
        let retained = raw.join("github/collect.log");
        if retained.exists() {
            read_bounded_text(&retained, MAX_GITHUB_LOG_BYTES)?
        } else {
            let output = gh_job_log(&format!("repos/CUBRID/cubrid/actions/jobs/{}/logs", job.id))?;
            let log = sanitize_log(&String::from_utf8_lossy(&output));
            write_string_immutable(&retained, &log)?;
            log
        }
    } else {
        return Ok(None);
    };
    // Read the concrete step environment, not shell commands or rendered Markdown.
    let pattern =
        Regex::new(r"(?m)^(?:[0-9T:.\-]+Z[ \t]+)?[ \t]*RERUN_FROM:[ \t]*([0-9]*)[ \t]*\r?$")
            .expect("valid rerun source regex");
    let mut sources = BTreeSet::new();
    for capture in pattern.captures_iter(&log) {
        if !capture[1].is_empty() {
            let source: u64 = capture[1]
                .parse()
                .map_err(|_| AppError::Malformed("invalid RERUN_FROM run ID".to_owned()))?;
            if source == 0 {
                return Err(AppError::Malformed(
                    "RERUN_FROM must identify a positive run ID".to_owned(),
                ));
            }
            sources.insert(source);
        }
    }
    if sources.len() > 1 {
        return Err(AppError::Integrity(
            "collect-job log has conflicting RERUN_FROM values".to_owned(),
        ));
    }
    let source = sources.into_iter().next();
    if plan.rerun_since.is_some() && source.is_none() {
        return Err(AppError::Unavailable(
            "rerun plan has no concrete RERUN_FROM in the collect-job environment".to_owned(),
        ));
    }
    Ok(source)
}

async fn collect_rerun_source(
    request: &SuiteRequest<'_>,
    source_run: u64,
    raw: &Path,
) -> Result<(SuiteSummary, RerunSource), AppError> {
    if source_run == request.run_id || request.lineage.contains(&source_run) {
        return Err(AppError::Integrity(
            "rerun source lineage is cyclic".to_owned(),
        ));
    }
    let metadata = raw.join("github/rerun-source.json");
    let bytes = if metadata.exists() {
        fs::read(&metadata).map_err(|source| AppError::storage(&metadata, source))?
    } else {
        let bytes = gh_output(&format!("repos/CUBRID/cubrid/actions/runs/{source_run}"))?;
        write_string_immutable(&metadata, &String::from_utf8_lossy(&bytes))?;
        bytes
    };
    let source: ActionRun = serde_json::from_slice(&bytes)?;
    let current: ActionRun = serde_json::from_slice(request.run_json)?;
    if source.id != source_run || source.run_attempt == 0 {
        return Err(AppError::Integrity(
            "rerun source metadata has inconsistent execution identity".to_owned(),
        ));
    }
    match (source.updated_at, current.created_at) {
        (Some(finished), Some(started))
            if source.status.as_deref() == Some("completed") && finished <= started => {}
        _ => {
            return Err(AppError::Unavailable(format!(
                "source run {source_run} attempt {} is unfinished, was updated after rerun {} started, or lacks timestamps; its source attempt cannot be established",
                source.run_attempt, request.run_id
            )));
        }
    }
    let mut lineage = request.lineage.to_vec();
    lineage.push(request.run_id);
    let source_context = Mutex::new(None);
    let acquisition = AcquisitionLedger::default();
    let mut binary_bytes_remaining = 0;
    let mut binary_consumption = Vec::new();
    let summary = Box::pin(collect_suite(SuiteRequest {
        suite: request.suite,
        run_id: source_run,
        attempt: source.run_attempt,
        commit: request.commit,
        ci_state: "unreported",
        artifact_base: request.artifact_base,
        evidence_dir: request.evidence_dir,
        status_snapshot_json: request.status_snapshot_json,
        run_json: &bytes,
        run_context: &source_context,
        lineage: &lineage,
        fetch_permits: request.fetch_permits,
        fetch_concurrency: request.fetch_concurrency,
        acquisition: &acquisition,
        include_binaries: false,
        max_binary_bytes: request.max_binary_bytes,
        binary_bytes_remaining: &mut binary_bytes_remaining,
        binary_consumption: &mut binary_consumption,
    }))
    .await
    .map_err(|error| AppError::RerunSource {
        run_id: source_run,
        attempt: source.run_attempt,
        source: Box::new(error),
    })?;
    let relative = PathBuf::from(format!(
        "providers/github-actions/runs/{source_run}/attempts/{}/{}/summary.json",
        source.run_attempt,
        request.suite.job_name()
    ));
    let summary_bytes = fs::read(request.evidence_dir.join(&relative))
        .map_err(|source| AppError::storage(&relative, source))?;
    let plan = parse_split_meta(&read_bounded_text(
        &raw.join("plan/split.meta"),
        MAX_INDEX_BYTES,
    )?)?;
    let source_plan = parse_split_meta(&read_bounded_text(
        &request
            .evidence_dir
            .join(&relative)
            .parent()
            .unwrap()
            .join("raw/plan/split.meta"),
        MAX_INDEX_BYTES,
    )?)?;
    if plan.engine_sha.as_deref() != Some(request.commit)
        || plan.rerun_since.as_deref() != Some(source_plan.testcase_sha.as_str())
    {
        return Err(AppError::Integrity(
            "rerun plan Engine SHA or source testcase revision disagrees with the validated source"
                .to_owned(),
        ));
    }
    Ok((
        summary,
        RerunSource {
            mode: RerunMode::Partial,
            run_id: source_run,
            attempt: source.run_attempt,
            summary: relative,
            summary_sha256: hex::encode(Sha256::digest(&summary_bytes)),
        },
    ))
}

async fn validate_rerun_source(
    request: &SuiteRequest<'_>,
    source: &RerunSource,
) -> Result<(), AppError> {
    let raw = request.evidence_dir.join(format!(
        "providers/github-actions/runs/{}/attempts/{}/{}/raw",
        request.run_id,
        request.attempt,
        request.suite.job_name()
    ));
    let (_, expected) = collect_rerun_source(request, source.run_id, &raw).await?;
    if source.attempt != expected.attempt
        || source.summary != expected.summary
        || source.summary_sha256 != expected.summary_sha256
    {
        return Err(AppError::Integrity(
            "stored rerun source reference disagrees with its validated source summary".to_owned(),
        ));
    }
    Ok(())
}

async fn collect_abnormal_shard_binaries(
    client: &Client,
    shard_root: &Url,
    suite_dir: &Path,
    shard: &str,
    options: BinaryCollection<'_>,
) -> Result<(), AppError> {
    let BinaryCollection {
        per_file_limit,
        total_remaining,
        fetch_permits,
        suite,
        consumption,
        records,
    } = options;
    let artifacts_root = shard_root.join("artifacts/").map_err(|error| {
        AppError::Malformed(format!("construct abnormal-shard artifact URL: {error}"))
    })?;
    let index = match fetch_text(
        client,
        fetch_permits,
        artifacts_root.clone(),
        MAX_INDEX_BYTES,
        &suite_dir.join(format!("raw/indexes/shard-{shard}-artifacts.html")),
    )
    .await
    {
        Ok(index) => index,
        Err(error) => {
            records.push(BinaryRecord {
                shard: shard.to_owned(),
                name: "*".to_owned(),
                state: "excluded".to_owned(),
                reason: Some(error.error.diagnostic_kind().to_owned()),
                size_bytes: None,
                sha256: None,
            });
            return Ok(());
        }
    };
    for name in directory_entries(&index, false)? {
        let url = artifacts_root
            .join(&name)
            .map_err(|error| AppError::Malformed(format!("construct artifact URL: {error}")))?;
        let _permit = fetch_permits
            .acquire()
            .await
            .map_err(|_| AppError::Remote("fetch concurrency limiter closed".to_owned()))?;
        let response = match client.get(url.clone()).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                consumption.push(BinaryConsumption {
                    suite: suite.to_owned(),
                    shard: shard.to_owned(),
                    name: name.clone(),
                    consumed_bytes: 0,
                });
                records.push(BinaryRecord {
                    shard: shard.to_owned(),
                    name,
                    state: "excluded".to_owned(),
                    reason: Some(format!("http_{}", response.status().as_u16())),
                    size_bytes: response.content_length(),
                    sha256: None,
                });
                continue;
            }
            Err(_) => {
                consumption.push(BinaryConsumption {
                    suite: suite.to_owned(),
                    shard: shard.to_owned(),
                    name: name.clone(),
                    consumed_bytes: 0,
                });
                records.push(BinaryRecord {
                    shard: shard.to_owned(),
                    name,
                    state: "excluded".to_owned(),
                    reason: Some("remote".to_owned()),
                    size_bytes: None,
                    sha256: None,
                });
                continue;
            }
        };
        let declared = response.content_length();
        let total_is_limiting = *total_remaining <= per_file_limit;
        let allowed = per_file_limit.min(*total_remaining);
        if declared.is_some_and(|size| size > allowed) || allowed == 0 {
            consumption.push(BinaryConsumption {
                suite: suite.to_owned(),
                shard: shard.to_owned(),
                name: name.clone(),
                consumed_bytes: 0,
            });
            records.push(BinaryRecord {
                shard: shard.to_owned(),
                name,
                state: "excluded".to_owned(),
                reason: Some(if allowed == 0 || total_is_limiting {
                    "total_limit".to_owned()
                } else {
                    "per_file_limit".to_owned()
                }),
                size_bytes: declared,
                sha256: None,
            });
            continue;
        }
        let destination = suite_dir.join("binaries").join(shard).join(&name);
        let parent = destination.parent().expect("binary destination has parent");
        fs::create_dir_all(parent).map_err(|source| AppError::storage(parent, source))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|source| AppError::storage(parent, source))?;
        let stream = response.bytes_stream().map_err(std::io::Error::other);
        let mut reader = StreamReader::new(stream).take(allowed);
        let mut size = 0_u64;
        let mut hasher = Sha256::new();
        let mut excluded = None;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let consumed = match reader.read(&mut buffer).await {
                Ok(consumed) => consumed,
                Err(error) => {
                    consumption.push(BinaryConsumption {
                        suite: suite.to_owned(),
                        shard: shard.to_owned(),
                        name: name.clone(),
                        consumed_bytes: size,
                    });
                    return Err(AppError::Remote(format!("read {url}: {error}")));
                }
            };
            if consumed == 0 {
                break;
            }
            size += consumed as u64;
            *total_remaining -= consumed as u64;
            if let Err(source) = temporary.write_all(&buffer[..consumed]) {
                consumption.push(BinaryConsumption {
                    suite: suite.to_owned(),
                    shard: shard.to_owned(),
                    name: name.clone(),
                    consumed_bytes: size,
                });
                return Err(AppError::storage(temporary.path(), source));
            }
            hasher.update(&buffer[..consumed]);
            if declared.is_none() && size == allowed {
                excluded = Some(if total_is_limiting {
                    "total_limit"
                } else {
                    "per_file_limit"
                });
                break;
            }
        }
        consumption.push(BinaryConsumption {
            suite: suite.to_owned(),
            shard: shard.to_owned(),
            name: name.clone(),
            consumed_bytes: size,
        });
        if let Some(reason) = excluded {
            records.push(BinaryRecord {
                shard: shard.to_owned(),
                name,
                state: "excluded".to_owned(),
                reason: Some(reason.to_owned()),
                size_bytes: declared,
                sha256: None,
            });
            continue;
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| AppError::storage(temporary.path(), source))?;
        let digest = hex::encode(hasher.finalize());
        match temporary.persist_noclobber(&destination) {
            Ok(_) => sync_directory(parent)?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let (existing_size, existing_digest) = file_size_and_sha256(&destination)?;
                if existing_size != size || existing_digest != digest {
                    return Err(AppError::Integrity(format!(
                        "immutable binary already exists with different content: {}",
                        destination.display()
                    )));
                }
            }
            Err(error) => return Err(AppError::storage(&destination, error.error)),
        }
        records.push(BinaryRecord {
            shard: shard.to_owned(),
            name,
            state: "downloaded".to_owned(),
            reason: None,
            size_bytes: Some(size),
            sha256: Some(digest),
        });
    }
    Ok(())
}

struct BinaryCollection<'a> {
    per_file_limit: u64,
    total_remaining: &'a mut u64,
    fetch_permits: &'a Semaphore,
    suite: &'a str,
    consumption: &'a mut Vec<BinaryConsumption>,
    records: &'a mut Vec<BinaryRecord>,
}

struct EvidenceResolution {
    base: Url,
    jobs: Vec<ActionJob>,
}

pub(crate) struct RunContext {
    base: Url,
    jobs: Vec<ActionJob>,
    job_pages: Vec<(String, Vec<u8>)>,
    collect_log: Option<Vec<u8>>,
}

pub(crate) type RunContextCache = Mutex<Option<Result<Arc<RunContext>, Arc<AppError>>>>;

struct LoadedJobs {
    jobs: Vec<ActionJob>,
    pages: Vec<(String, Vec<u8>)>,
}

fn resolve_artifact_base(
    context: &RunContext,
    suite: &str,
    provider_raw: &Path,
) -> Result<EvidenceResolution, AppError> {
    retain_run_context(context, provider_raw)?;
    let diagnostic_jobs = context
        .jobs
        .iter()
        .filter(|job| job_is_relevant_to_suite(job, suite))
        .cloned()
        .collect::<Vec<_>>();
    Ok(EvidenceResolution {
        base: context.base.clone(),
        jobs: diagnostic_jobs,
    })
}

fn load_run_context(
    run_id: u64,
    attempt: u64,
    suite: &str,
    configured: Option<&Url>,
    provider_raw: &Path,
) -> Result<RunContext, AppError> {
    let LoadedJobs {
        jobs,
        pages: job_pages,
    } = load_jobs(run_id)?;
    retain_job_pages(&job_pages, provider_raw)?;
    let selected_jobs = jobs
        .into_iter()
        .filter(|job| job.run_attempt == attempt)
        .collect::<Vec<_>>();
    let diagnostic_jobs = selected_jobs
        .iter()
        .filter(|job| job_is_relevant_to_suite(job, suite))
        .cloned()
        .collect::<Vec<_>>();
    let collect_job = selected_jobs
        .iter()
        .find(|job| job.name == "collect")
        .map(Ok)
        .unwrap_or_else(|| {
            retain_abnormal_job_logs(&diagnostic_jobs, provider_raw)?;
            Err(AppError::JobLevel(format!(
                "Actions run {run_id} attempt {attempt} has no collect job"
            )))
        })?;
    if let Some(configured) = configured {
        return Ok(RunContext {
            base: configured.clone(),
            jobs: selected_jobs,
            job_pages,
            collect_log: None,
        });
    }
    let collect_job_id = collect_job.id;
    let logs_endpoint = format!("repos/CUBRID/cubrid/actions/jobs/{collect_job_id}/logs");
    let output = match gh_job_log(&logs_endpoint) {
        Ok(output) => output,
        Err(error) if job_is_abnormal(collect_job) => {
            write_string_immutable(
                &provider_raw.join(format!("github/job-{collect_job_id}-log-error.txt")),
                &error.to_string(),
            )?;
            return Err(AppError::JobLevel(format!(
                "collect job ended {} and its log is unavailable: {error}",
                collect_job.conclusion.as_deref().unwrap_or("abnormally")
            )));
        }
        Err(error) => return Err(error),
    };
    write_string_immutable(
        &provider_raw.join("github/collect.log"),
        &sanitize_log(&String::from_utf8_lossy(&output)),
    )?;
    let log = String::from_utf8(output.clone())
        .map_err(|_| AppError::Remote("collect-job log is not UTF-8".to_owned()))?;
    let capture = Regex::new(r"(?m)ARTIFACT_URL_BASE(?::|=)\s*(https?://[^\s]+)")
        .expect("valid artifact URL regex")
        .captures(&log)
        .and_then(|capture| capture.get(1));
    let Some(capture) = capture else {
        if job_is_abnormal(collect_job) {
            return Err(AppError::JobLevel(format!(
                "collect job ended {} without publishing an evidence-server URL",
                collect_job.conclusion.as_deref().unwrap_or("abnormally")
            )));
        }
        return Err(AppError::Unavailable(
            "collect-job log contains no evidence-server URL".to_owned(),
        ));
    };
    let value = capture.as_str().trim_end_matches([')', ']', ',', ';']);
    let mut url = Url::parse(value).map_err(|error| {
        AppError::Remote(format!("invalid evidence-server URL in log: {error}"))
    })?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(RunContext {
        base: url,
        jobs: selected_jobs,
        job_pages,
        collect_log: Some(output),
    })
}

fn retain_run_context(context: &RunContext, provider_raw: &Path) -> Result<(), AppError> {
    retain_job_pages(&context.job_pages, provider_raw)?;
    if let Some(log) = &context.collect_log {
        write_string_immutable(
            &provider_raw.join("github/collect.log"),
            &sanitize_log(&String::from_utf8_lossy(log)),
        )?;
    }
    Ok(())
}

fn retain_job_pages(job_pages: &[(String, Vec<u8>)], provider_raw: &Path) -> Result<(), AppError> {
    for (name, json) in job_pages {
        write_string_immutable(
            &provider_raw.join("github").join(name),
            &String::from_utf8_lossy(json),
        )?;
    }
    Ok(())
}

fn load_jobs(run_id: u64) -> Result<LoadedJobs, AppError> {
    let mut all_jobs = Vec::new();
    let mut pages = Vec::new();
    let mut page = 1_u64;
    loop {
        let endpoint = format!(
            "repos/CUBRID/cubrid/actions/runs/{run_id}/jobs?filter=all&per_page=100&page={page}"
        );
        let json = gh_output(&endpoint)?;
        let name = if page == 1 {
            "jobs.json".to_owned()
        } else {
            format!("jobs-page-{page}.json")
        };
        let response: JobsResponse =
            serde_json::from_slice(&json).map_err(|source| AppError::Json { endpoint, source })?;
        let page_count = response.jobs.len();
        all_jobs.extend(response.jobs);
        pages.push((name, json));
        let complete = response
            .total_count
            .is_none_or(|total| all_jobs.len() as u64 >= total);
        if complete || page_count == 0 {
            break;
        }
        page += 1;
        if page > 100 {
            return Err(AppError::Remote(
                "Actions job listing exceeds 10,000 records".to_owned(),
            ));
        }
    }
    Ok(LoadedJobs {
        jobs: all_jobs,
        pages,
    })
}

fn classify_pipeline_fetch(
    result: Result<String, FetchFailure>,
    jobs: &[ActionJob],
    provider_raw: &Path,
    stage: &str,
    terminal_required: bool,
) -> Result<String, FetchFailure> {
    match result {
        Ok(value) => Ok(value),
        Err(mut failure)
            if jobs.iter().any(job_is_abnormal)
                || (terminal_required && matches!(&failure.error, AppError::Unavailable(_))) =>
        {
            if let Err(error) = retain_abnormal_job_logs(jobs, provider_raw) {
                failure.error = error;
                return Err(failure);
            }
            let failed = jobs
                .iter()
                .filter(|job| job_is_abnormal(job))
                .map(|job| {
                    format!(
                        "{} ({})",
                        job.name,
                        job.conclusion.as_deref().unwrap_or("unknown")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let context = if failed.is_empty() {
                "the terminal suite published no trustworthy collector output".to_owned()
            } else {
                format!("abnormal Actions jobs: {failed}")
            };
            failure.error = AppError::JobLevel(format!(
                "{stage} is unavailable after {context}; {}",
                failure.error
            ));
            Err(failure)
        }
        Err(failure) => Err(failure),
    }
}

fn job_level_failure<T>(
    message: String,
    jobs: &[ActionJob],
    provider_raw: &Path,
) -> Result<T, AppError> {
    retain_abnormal_job_logs(jobs, provider_raw)?;
    Err(AppError::JobLevel(message))
}

fn job_is_abnormal(job: &ActionJob) -> bool {
    matches!(
        job.conclusion.as_deref(),
        Some("failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure")
    )
}

fn job_is_relevant_to_suite(job: &ActionJob, suite: &str) -> bool {
    let name = job.name.to_ascii_lowercase();
    let suite = suite.to_ascii_lowercase();
    if name.starts_with(&format!("shard {suite} ")) || name == format!("status {suite}") {
        return true;
    }
    if name == "gate"
        || name == "plan"
        || name.starts_with("plan ")
        || name == "build status"
        || name.starts_with("build (")
    {
        return true;
    }
    name == "collect" && job.conclusion.as_deref() != Some("failure")
}

fn retain_abnormal_job_logs(jobs: &[ActionJob], provider_raw: &Path) -> Result<(), AppError> {
    for job in jobs.iter().filter(|job| job_is_abnormal(job)) {
        let endpoint = format!("repos/CUBRID/cubrid/actions/jobs/{}/logs", job.id);
        match gh_job_log(&endpoint) {
            Ok(log) => write_string_immutable(
                &provider_raw.join(format!("github/job-{}.log", job.id)),
                &sanitize_log(&String::from_utf8_lossy(&log)),
            )?,
            Err(error) => write_string_immutable(
                &provider_raw.join(format!("github/job-{}-log-error.txt", job.id)),
                &error.to_string(),
            )?,
        }
    }
    Ok(())
}

fn sanitize_log(value: &str) -> String {
    let signed_url =
        Regex::new(r"(?i)(https?://[^\s?]+)\?[^\s]+").expect("valid signed URL redaction regex");
    let header = Regex::new(r"(?im)^(\s*(?:authorization|cookie|set-cookie)\s*:\s*).*$")
        .expect("valid credential header redaction regex");
    let assignment = Regex::new(r"(?im)^(\s*[A-Za-z0-9_]*(?:token|secret)[A-Za-z0-9_]*\s*=\s*).*$")
        .expect("valid credential assignment redaction regex");
    let value = signed_url.replace_all(value, "$1?[REDACTED]");
    let value = header.replace_all(&value, "$1[REDACTED]");
    assignment.replace_all(&value, "$1[REDACTED]").into_owned()
}

fn file_size_and_sha256(path: &Path) -> Result<(u64, String), AppError> {
    let mut file = fs::File::open(path).map_err(|source| AppError::storage(path, source))?;
    let mut size = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AppError::storage(path, source))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((size, hex::encode(hasher.finalize())))
}

fn sync_directory(path: &Path) -> Result<(), AppError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| AppError::storage(path, source))
}

fn validate_artifact_base(url: &Url) -> Result<(), AppError> {
    if !matches!(url.scheme(), "http" | "https")
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::Integrity(
            "evidence-server URL is not a credential-free HTTP base URL".to_owned(),
        ));
    }
    Ok(())
}

fn gh_output(endpoint: &str) -> Result<Vec<u8>, AppError> {
    bounded_gh_output(&["api", endpoint], endpoint, MAX_GITHUB_JSON_BYTES)
}

fn gh_job_log(endpoint: &str) -> Result<Vec<u8>, AppError> {
    bounded_gh_output(
        &["api", "--allow-escape-sequences", endpoint],
        endpoint,
        MAX_GITHUB_LOG_BYTES,
    )
}

fn bounded_gh_output(args: &[&str], endpoint: &str, limit: usize) -> Result<Vec<u8>, AppError> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| AppError::Remote(format!("failed to execute gh: {error}")))?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::Remote(format!("read gh output for {endpoint}: {error}")))?;
    if bytes.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        return Err(AppError::Oversized(format!(
            "gh output for {endpoint} exceeds the {limit}-byte limit"
        )));
    }
    let status = child
        .wait()
        .map_err(|error| AppError::Remote(format!("wait for gh: {error}")))?;
    if !status.success() {
        return Err(AppError::Remote(format!(
            "gh api failed for {endpoint} with {status}"
        )));
    }
    Ok(bytes)
}

async fn fetch_text(
    client: &Client,
    fetch_permits: &Semaphore,
    url: Url,
    limit: usize,
    destination: &Path,
) -> Result<String, FetchFailure> {
    if destination.exists() {
        return read_verified_text(destination, limit)
            .map_err(|error| FetchFailure::new(&url, 0, error));
    }
    let _permit = fetch_permits.acquire().await.map_err(|_| {
        FetchFailure::new(
            &url,
            0,
            AppError::Remote("fetch concurrency limiter closed".to_owned()),
        )
    })?;
    let response = client.get(url.clone()).send().await.map_err(|error| {
        FetchFailure::new(&url, 0, AppError::Remote(format!("GET {url}: {error}")))
    })?;
    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(FetchFailure::new(
                &url,
                0,
                AppError::Unavailable(format!("GET {url} returned 404")),
            ));
        }
        if response.status() == reqwest::StatusCode::GONE {
            return Err(FetchFailure::new(
                &url,
                0,
                AppError::Expired(format!("GET {url} returned 410")),
            ));
        }
        return Err(FetchFailure::new(
            &url,
            0,
            AppError::Remote(format!("GET {url} returned {}", response.status())),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(FetchFailure::new(
            &url,
            0,
            AppError::Oversized(format!("GET {url} exceeds the {limit}-byte text limit")),
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        FetchFailure::new(
            &url,
            0,
            AppError::Input(format!(
                "output path has no parent: {}",
                destination.display()
            )),
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| FetchFailure::new(&url, 0, AppError::storage(parent, source)))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| FetchFailure::new(&url, 0, AppError::storage(parent, source)))?;
    let mut size = 0_usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            FetchFailure::new(
                &url,
                size as u64,
                AppError::Remote(format!("read {url}: {error}")),
            )
        })?;
        let consumed = size.saturating_add(chunk.len());
        if size.saturating_add(chunk.len()) > limit {
            return Err(FetchFailure::new(
                &url,
                consumed as u64,
                AppError::Oversized(format!("GET {url} exceeds the {limit}-byte text limit")),
            ));
        }
        temporary.write_all(&chunk).map_err(|source| {
            FetchFailure::new(
                &url,
                consumed as u64,
                AppError::storage(temporary.path(), source),
            )
        })?;
        size = consumed;
    }
    temporary.as_file().sync_all().map_err(|source| {
        FetchFailure::new(
            &url,
            size as u64,
            AppError::storage(temporary.path(), source),
        )
    })?;
    let bytes = fs::read(temporary.path()).map_err(|source| {
        FetchFailure::new(
            &url,
            size as u64,
            AppError::storage(temporary.path(), source),
        )
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        FetchFailure::new(
            &url,
            size as u64,
            AppError::Malformed(format!("GET {url} is not UTF-8 text")),
        )
    })?;
    write_text_receipt(destination, text.as_bytes())
        .map_err(|error| FetchFailure::new(&url, size as u64, error))?;
    match temporary.persist_noclobber(destination) {
        Ok(_) => {
            sync_directory(parent).map_err(|error| FetchFailure::new(&url, size as u64, error))?
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_bounded_text(destination, limit)
                .map_err(|error| FetchFailure::new(&url, size as u64, error))?;
            if existing != text {
                return Err(FetchFailure::new(
                    &url,
                    size as u64,
                    AppError::Integrity(format!(
                        "immutable evidence already exists with different content: {}",
                        destination.display()
                    )),
                ));
            }
        }
        Err(error) => {
            return Err(FetchFailure::new(
                &url,
                size as u64,
                AppError::storage(destination, error.error),
            ));
        }
    }
    Ok(text)
}

fn read_bounded_text(path: &Path, limit: usize) -> Result<String, AppError> {
    let bytes = fs::read(path).map_err(|source| AppError::storage(path, source))?;
    if bytes.len() > limit {
        return Err(AppError::Integrity(format!(
            "retained evidence exceeds its {limit}-byte limit: {}",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        AppError::Integrity(format!(
            "retained evidence is not UTF-8 text: {}",
            path.display()
        ))
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextEvidenceReceipt {
    schema_version: u32,
    file_name: String,
    size_bytes: u64,
    sha256: String,
}

fn text_receipt_path(destination: &Path) -> Result<PathBuf, AppError> {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::Input(format!(
                "raw evidence path has no UTF-8 file name: {}",
                destination.display()
            ))
        })?;
    let receipt_name = format!("{}.json", hex::encode(Sha256::digest(file_name.as_bytes())));
    Ok(destination
        .parent()
        .expect("raw evidence destination has a parent")
        .join(".cubrid-ci-receipts")
        .join(receipt_name))
}

fn write_text_receipt(destination: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::Input(format!(
                "raw evidence path has no UTF-8 file name: {}",
                destination.display()
            ))
        })?;
    write_json_immutable(
        &text_receipt_path(destination)?,
        &TextEvidenceReceipt {
            schema_version: 1,
            file_name: file_name.to_owned(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(bytes)),
        },
    )
}

fn read_verified_text(path: &Path, limit: usize) -> Result<String, AppError> {
    let text = read_bounded_text(path, limit)?;
    let receipt_path = text_receipt_path(path)?;
    let receipt_bytes = fs::read(&receipt_path).map_err(|error| {
        AppError::Integrity(format!(
            "read retained raw evidence receipt {}: {error}",
            receipt_path.display()
        ))
    })?;
    let receipt: TextEvidenceReceipt = serde_json::from_slice(&receipt_bytes).map_err(|error| {
        AppError::Integrity(format!(
            "parse retained raw evidence receipt {}: {error}",
            receipt_path.display()
        ))
    })?;
    let file_name = path.file_name().and_then(|name| name.to_str());
    if receipt.schema_version != 1
        || Some(receipt.file_name.as_str()) != file_name
        || receipt.size_bytes != text.len() as u64
        || receipt.sha256 != hex::encode(Sha256::digest(text.as_bytes()))
    {
        return Err(AppError::Integrity(format!(
            "retained raw evidence disagrees with its receipt: {}",
            path.display()
        )));
    }
    Ok(text)
}

fn directory_entries(html: &str, directories: bool) -> Result<Vec<String>, AppError> {
    let href = Regex::new(r#"(?i)href\s*=\s*["']([^"']+)["']"#).expect("valid href regex");
    let mut entries = Vec::new();
    for value in href
        .captures_iter(html)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str()))
    {
        if matches!(value, "../" | ".." | "./" | ".") {
            continue;
        }
        let value = value.trim_end_matches('/');
        if value.is_empty() {
            continue;
        }
        if value.contains(['/', '\\', '%', '?', '#']) {
            return Err(AppError::Malformed(format!(
                "directory index contains unsafe entry {value:?}"
            )));
        }
        if directories && !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(AppError::Malformed(format!(
                "directory index contains non-shard entry {value:?}"
            )));
        }
        if !directories
            && !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(AppError::Malformed(format!(
                "directory index contains unsafe filename {value:?}"
            )));
        }
        entries.push(value.to_owned());
    }
    entries.sort();
    entries.dedup();
    if entries.len() > 10_000 {
        return Err(AppError::Malformed(
            "evidence-server directory index has too many entries".to_owned(),
        ));
    }
    Ok(entries)
}

fn parse_key_values(value: &str) -> Result<BTreeMap<&str, &str>, AppError> {
    let mut fields = BTreeMap::new();
    for (index, line) in value.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (name, field_value) = line
            .split_once('=')
            .ok_or_else(|| AppError::Malformed(format!("malformed key-value row {}", index + 1)))?;
        if name.is_empty() || fields.insert(name, field_value).is_some() {
            return Err(AppError::Malformed(format!(
                "duplicate or empty key on row {}",
                index + 1
            )));
        }
    }
    Ok(fields)
}

fn parse_build_provenance(value: &str) -> Result<BuildProvenance, AppError> {
    let fields = parse_key_values(value)?;
    let required = |name| {
        fields.get(name).copied().ok_or_else(|| {
            AppError::Malformed(format!("build.read is missing required field {name}"))
        })
    };
    Ok(BuildProvenance {
        sha: required("sha")?.to_owned(),
        mode: fields.get("mode").map(|value| (*value).to_owned()),
        namespace: fields.get("ns").map(|value| (*value).to_owned()),
        run_id: required("run_id")?
            .parse()
            .map_err(|_| AppError::Malformed("build.read has invalid run_id".to_owned()))?,
        run_attempt: fields
            .get("run_attempt")
            .map(|value| value.parse())
            .transpose()
            .map_err(|_| AppError::Malformed("build.read has invalid run_attempt".to_owned()))?,
    })
}

fn parse_testcase_provenance(value: &str) -> Result<TestcaseProvenance, AppError> {
    let fields = parse_key_values(value)?;
    let sha = fields
        .get("tc_sha")
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Malformed("tc.read has no valid tc_sha".to_owned()))?;
    let branch = fields
        .get("tc_branch")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Malformed("tc.read has no tc_branch".to_owned()))?;
    Ok(TestcaseProvenance {
        sha: (*sha).to_owned(),
        branch: (*branch).to_owned(),
    })
}

fn parse_failed_list(value: &str) -> Result<Vec<(String, String)>, AppError> {
    value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (shard, name) = line.split_once('\t').ok_or_else(|| {
                AppError::Malformed("collect/failed.list has a malformed row".to_owned())
            })?;
            if shard.bytes().all(|byte| byte.is_ascii_digit()) && !name.is_empty() {
                Ok((shard.to_owned(), name.to_owned()))
            } else {
                Err(AppError::Malformed(
                    "collect/failed.list has an invalid shard or testcase".to_owned(),
                ))
            }
        })
        .collect()
}

#[derive(Debug)]
struct SqlPlan {
    total: u64,
    parallelism: u64,
    testcase_sha: String,
    testcase_branch: String,
    engine_sha: Option<String>,
    rerun_since: Option<String>,
    carry_forward: bool,
}

fn parse_split_meta(value: &str) -> Result<SqlPlan, AppError> {
    let fields = parse_key_values(value)?;
    let field = |name| {
        fields
            .get(name)
            .map(|value| value.trim_matches('\''))
            .ok_or_else(|| AppError::Malformed(format!("split.meta is missing {name}")))
    };
    let total = field("total")?
        .parse()
        .map_err(|_| AppError::Malformed("split.meta has invalid total".to_owned()))?;
    let parallelism = field("par")?
        .parse()
        .map_err(|_| AppError::Malformed("split.meta has invalid par".to_owned()))?;
    let testcase_sha = field("tc_sha")?;
    if testcase_sha.len() != 40 || !testcase_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::Malformed(
            "split.meta has invalid tc_sha".to_owned(),
        ));
    }
    let testcase_branch = field("tc_branch")?;
    if testcase_branch.is_empty() {
        return Err(AppError::Malformed(
            "split.meta has empty tc_branch".to_owned(),
        ));
    }
    Ok(SqlPlan {
        total,
        parallelism,
        testcase_sha: testcase_sha.to_owned(),
        testcase_branch: testcase_branch.to_owned(),
        engine_sha: fields
            .get("sha")
            .map(|value| value.trim_matches('\'').to_owned()),
        rerun_since: fields
            .get("rerun_since")
            .map(|value| value.trim_matches('\''))
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        carry_forward: [
            "rerun_unrun",
            "rerun_short",
            "rerun_gone",
            "rerun_failed",
            "rerun_changed",
        ]
        .iter()
        .all(|name| fields.get(name).is_some_and(|value| *value == "0")),
    })
}

#[derive(Debug, Default)]
struct WorkflowCounts {
    passed: u64,
    failed: u64,
    skipped: u64,
    failed_names: BTreeSet<String>,
}

fn parse_plan_table(value: &str) -> Result<BTreeMap<String, u64>, AppError> {
    let mut plan = BTreeMap::new();
    for (index, line) in value.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 2
            || fields[0].is_empty()
            || !fields[0].bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(AppError::Malformed(format!(
                "plan.tsv has a malformed row {}",
                index + 1
            )));
        }
        let count = fields[1].parse().map_err(|_| {
            AppError::Malformed(format!(
                "plan.tsv has an invalid count on row {}",
                index + 1
            ))
        })?;
        if plan.insert(fields[0].to_owned(), count).is_some() {
            return Err(AppError::Malformed(format!(
                "plan.tsv repeats shard {}",
                fields[0]
            )));
        }
    }
    if plan.is_empty() {
        return Err(AppError::Malformed("plan.tsv is empty".to_owned()));
    }
    Ok(plan)
}

fn parse_summary_info(value: &str) -> Result<WorkflowCounts, AppError> {
    let verdict = Regex::new(r":(ok|nok)?[[:space:]]+[0-9]+ms$").expect("valid verdict regex");
    let mut counts = WorkflowCounts::default();
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        let Some(capture) = verdict.captures(line) else {
            continue;
        };
        match capture.get(1).map(|value| value.as_str()) {
            Some("ok") => counts.passed += 1,
            Some("nok") => {
                counts.failed += 1;
                let raw_name = &line[..capture.get(0).expect("whole regex capture").start()];
                let name = raw_name
                    .split_once("/cubrid-testcases/")
                    .map_or(raw_name, |(_, relative)| relative);
                if !counts.failed_names.insert(name.to_owned()) {
                    return Err(AppError::Integrity(format!(
                        "summary_info repeats failed testcase {name}"
                    )));
                }
            }
            None => counts.skipped += 1,
            _ => unreachable!("verdict regex has only ok and nok alternatives"),
        }
    }
    if counts.passed + counts.failed + counts.skipped == 0 {
        return Err(AppError::Malformed(
            "summary_info contains no testcase verdict rows".to_owned(),
        ));
    }
    Ok(counts)
}

fn parse_test_status(value: &str) -> Result<WorkflowCounts, AppError> {
    let properties = value
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with('#') && !line.starts_with('!')
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fields = parse_key_values(&properties)?;
    let count = |name| {
        fields
            .get(name)
            .ok_or_else(|| AppError::Malformed(format!("test_status.data is missing {name}")))?
            .parse::<u64>()
            .map_err(|_| AppError::Malformed(format!("test_status.data has invalid {name}")))
    };
    let executed = count("total_executed_case_count")?;
    let counts = WorkflowCounts {
        passed: count("total_success_case_count")?,
        failed: count("total_fail_case_count")?,
        skipped: count("total_skip_case_count")?,
        failed_names: BTreeSet::new(),
    };
    if executed != counts.passed + counts.failed {
        return Err(AppError::Integrity(
            "test_status.data executed count disagrees with success and failure counts".to_owned(),
        ));
    }
    Ok(counts)
}

fn validate_shard_done(
    value: &str,
    expected_index: &str,
    expected_suite: &str,
    failed_cases: u64,
) -> Result<u64, AppError> {
    let fields = parse_key_values(value)?;
    if fields.get("suite") != Some(&expected_suite) || fields.get("idx") != Some(&expected_index) {
        return Err(AppError::Integrity(format!(
            "shard {expected_index} completion record has inconsistent identity"
        )));
    }
    let (status_field, status) = match (fields.get("test_rc"), fields.get("ctp_rc")) {
        (Some(test_rc), Some(ctp_rc)) if test_rc != ctp_rc => {
            return Err(AppError::Integrity(format!(
                "shard {expected_index} has conflicting test_rc and ctp_rc"
            )));
        }
        (Some(status), _) => ("test_rc", *status),
        (None, Some(status)) => ("ctp_rc", *status),
        (None, None) => {
            return Err(AppError::JobLevel(format!(
                "shard {expected_index} has no test_rc or ctp_rc"
            )));
        }
    };
    let exit_code = status.parse::<u8>().map_err(|_| {
        AppError::Malformed(format!("shard {expected_index} has invalid {status_field}"))
    })?;
    // The workflow records the test command's exit, including 1 for failed cases.
    // Counts, JUnit, and the failed-case inventory must still reconcile below.
    if exit_code > 1 || (exit_code == 1 && failed_cases == 0) {
        return Err(AppError::JobLevel(format!(
            "shard {expected_index} exited abnormally with {status_field}={status}"
        )));
    }
    fields
        .get("assigned")
        .ok_or_else(|| AppError::Malformed("shard.done is missing assigned".to_owned()))?
        .parse()
        .map_err(|_| AppError::Malformed("shard.done has invalid assigned".to_owned()))
}

#[derive(Debug, Serialize, Deserialize)]
struct RawEvidenceIndex {
    schema_version: u32,
    run_id: u64,
    attempt: u64,
    suite: String,
    summary_sha256: String,
    files: Vec<RawFileRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawFileRecord {
    path: PathBuf,
    size_bytes: u64,
    sha256: String,
}

fn write_raw_index(
    raw_dir: &Path,
    run_id: u64,
    attempt: u64,
    suite: &str,
    summary_sha256: &str,
) -> Result<(), AppError> {
    let mut files = Vec::new();
    for entry in WalkDir::new(raw_dir) {
        let entry = entry.map_err(|error| {
            AppError::Remote(format!(
                "enumerate raw evidence {}: {error}",
                raw_dir.display()
            ))
        })?;
        if !entry.file_type().is_file() || entry.path() == raw_dir.join("index.json") {
            continue;
        }
        let bytes =
            fs::read(entry.path()).map_err(|source| AppError::storage(entry.path(), source))?;
        let path = entry
            .path()
            .strip_prefix(raw_dir)
            .map_err(|error| AppError::Integrity(format!("derive raw evidence path: {error}")))?
            .to_owned();
        files.push(RawFileRecord {
            path,
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    write_json_immutable(
        &raw_dir.join("index.json"),
        &RawEvidenceIndex {
            schema_version: 2,
            run_id,
            attempt,
            suite: suite.to_owned(),
            summary_sha256: summary_sha256.to_owned(),
            files,
        },
    )
}

fn validate_raw_index(
    raw_dir: &Path,
    run_id: u64,
    attempt: u64,
    suite: &str,
) -> Result<String, AppError> {
    let index_path = raw_dir.join("index.json");
    let bytes = fs::read(&index_path).map_err(|error| {
        AppError::Integrity(format!("read retained raw evidence index: {error}"))
    })?;
    validate_json_schema(
        "raw-evidence-index-v2",
        include_str!("../schema/raw-evidence-index-v2.schema.json"),
        &bytes,
    )?;
    let index: RawEvidenceIndex = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Integrity(format!("parse retained raw evidence index: {error}"))
    })?;
    if index.schema_version != 2
        || index.run_id != run_id
        || index.attempt != attempt
        || index.suite != suite
    {
        return Err(AppError::Integrity(
            "retained raw evidence index has inconsistent identity".to_owned(),
        ));
    }
    let mut seen = BTreeSet::new();
    for file in index.files {
        if file.path.is_absolute()
            || file
                .path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || !seen.insert(file.path.clone())
        {
            return Err(AppError::Integrity(format!(
                "retained raw evidence index has an unsafe or repeated path: {}",
                file.path.display()
            )));
        }
        let path = raw_dir.join(&file.path);
        let retained = fs::read(&path).map_err(|error| {
            AppError::Integrity(format!(
                "read retained raw evidence {}: {error}",
                file.path.display()
            ))
        })?;
        if retained.len() as u64 != file.size_bytes
            || hex::encode(Sha256::digest(&retained)) != file.sha256
        {
            return Err(AppError::Integrity(format!(
                "retained raw evidence disagrees with its index: {}",
                file.path.display()
            )));
        }
    }
    Ok(index.summary_sha256)
}

fn validate_binary_inventory(suite_dir: &Path) -> Result<(), AppError> {
    let path = suite_dir.join("binary-inventory.json");
    let bytes = fs::read(&path).map_err(|source| AppError::storage(&path, source))?;
    validate_json_schema(
        "binary-inventory-v2",
        include_str!("../schema/binary-inventory-v2.schema.json"),
        &bytes,
    )?;
    let inventory: BinaryInventory = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Integrity(format!("parse retained binary inventory: {error}"))
    })?;
    let mut seen = BTreeSet::new();
    for file in inventory.files {
        let safe_name = Path::new(&file.name)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
            && Path::new(&file.name).components().count() == 1;
        if !safe_name || !seen.insert((file.shard.clone(), file.name.clone())) {
            return Err(AppError::Integrity(format!(
                "retained binary inventory has an unsafe or repeated path: {}/{}",
                file.shard, file.name
            )));
        }
        let destination = suite_dir
            .join("binaries")
            .join(&file.shard)
            .join(&file.name);
        if file.state == "downloaded" {
            let (Some(size), Some(digest)) = (file.size_bytes, file.sha256.as_deref()) else {
                return Err(AppError::Integrity(format!(
                    "downloaded binary lacks size or digest: {}/{}",
                    file.shard, file.name
                )));
            };
            if file.reason.is_some() {
                return Err(AppError::Integrity(format!(
                    "downloaded binary has an exclusion reason: {}/{}",
                    file.shard, file.name
                )));
            }
            let (actual_size, actual_digest) = file_size_and_sha256(&destination)?;
            if actual_size != size || actual_digest != digest {
                return Err(AppError::Integrity(format!(
                    "retained binary disagrees with its inventory: {}/{}",
                    file.shard, file.name
                )));
            }
        } else if file.sha256.is_some() {
            return Err(AppError::Integrity(format!(
                "excluded binary unexpectedly has a digest: {}/{}",
                file.shard, file.name
            )));
        }
    }
    Ok(())
}

fn validate_json_schema(label: &str, schema: &str, bytes: &[u8]) -> Result<(), AppError> {
    let schema: serde_json::Value = serde_json::from_str(schema)
        .map_err(|error| AppError::Integrity(format!("parse embedded {label} schema: {error}")))?;
    let instance: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        AppError::Integrity(format!("parse retained {label} document: {error}"))
    })?;
    let validator = jsonschema::validator_for(&schema).map_err(|error| {
        AppError::Integrity(format!("compile embedded {label} schema: {error}"))
    })?;
    if let Err(error) = validator.validate(&instance) {
        return Err(AppError::Integrity(format!(
            "retained document violates {label} schema: {error}"
        )));
    }
    Ok(())
}

fn parse_junit(xml: &str) -> Result<Vec<JunitCase>, AppError> {
    let document = roxmltree::Document::parse(xml)
        .map_err(|error| AppError::Malformed(format!("invalid JUnit XML: {error}")))?;
    document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
        .map(|node| {
            let name = node
                .attribute("name")
                .filter(|name| !name.is_empty())
                .ok_or_else(|| AppError::Malformed("JUnit testcase has no name".to_owned()))?;
            let failure = node
                .children()
                .find(|child| child.has_tag_name("failure"))
                .map(node_text);
            let error = node
                .children()
                .find(|child| child.has_tag_name("error"))
                .map(node_text);
            Ok(JunitCase {
                name: name.to_owned(),
                class_name: node.attribute("classname").map(ToOwned::to_owned),
                duration_seconds: node.attribute("time").and_then(|value| value.parse().ok()),
                failure,
                error,
                skipped: node.children().any(|child| child.has_tag_name("skipped")),
            })
        })
        .collect()
}

fn node_text(node: roxmltree::Node<'_, '_>) -> String {
    node.descendants()
        .filter(|child| child.is_text())
        .filter_map(|child| child.text())
        .collect::<String>()
}

#[derive(Debug, Deserialize)]
struct JobsResponse {
    #[serde(default)]
    total_count: Option<u64>,
    jobs: Vec<ActionJob>,
}

#[derive(Debug, Clone, Deserialize)]
struct ActionJob {
    id: u64,
    name: String,
    run_attempt: u64,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActionRun {
    id: u64,
    run_attempt: u64,
    #[serde(default)]
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_retained_workflow_test_rc_completion() {
        let marker = include_str!("../tests/fixtures/pr7925-medium-shard.done");
        assert_eq!(validate_shard_done(marker, "00", "medium", 0).unwrap(), 975);
        let marker = include_str!("../tests/fixtures/pr7925-shell-shard.done");
        assert_eq!(validate_shard_done(marker, "30", "shell", 1).unwrap(), 66);
    }

    #[test]
    fn completion_status_aliases_require_consistent_testcase_evidence() {
        for field in ["test_rc", "ctp_rc"] {
            for (status, failed, accepted) in [
                (0, 0, true),
                (0, 1, true),
                (1, 1, true),
                (1, 0, false),
                (2, 1, false),
                (137, 1, false),
            ] {
                let marker = format!("suite=sql\nidx=00\nassigned=1\n{field}={status}\n");
                let result = validate_shard_done(&marker, "00", "sql", failed);
                if accepted {
                    assert_eq!(result.unwrap(), 1);
                } else {
                    assert!(matches!(result, Err(AppError::JobLevel(_))));
                }
            }
        }
        for status in ["", "invalid", "-1", "256"] {
            for field in ["test_rc", "ctp_rc"] {
                let marker = format!("suite=sql\nidx=00\nassigned=1\n{field}={status}\n");
                assert!(matches!(
                    validate_shard_done(&marker, "00", "sql", 1),
                    Err(AppError::Malformed(_))
                ));
            }
        }
    }

    #[test]
    fn completion_markers_reject_missing_or_ambiguous_statuses() {
        let marker = "suite=sql\nidx=00\nassigned=1\n";
        assert!(matches!(
            validate_shard_done(marker, "00", "sql", 1),
            Err(AppError::JobLevel(_))
        ));
        for status in ["test_rc=0\nctp_rc=1\n", "test_rc=1\nctp_rc=0\n"] {
            assert!(matches!(
                validate_shard_done(&format!("{marker}{status}"), "00", "sql", 1),
                Err(AppError::Integrity(_))
            ));
        }
        for field in ["test_rc", "ctp_rc"] {
            let duplicate = format!("{marker}{field}=0\n{field}=0\n");
            assert!(matches!(
                validate_shard_done(&duplicate, "00", "sql", 0),
                Err(AppError::Malformed(_))
            ));
        }
        assert_eq!(
            validate_shard_done(&format!("{marker}test_rc=0\nctp_rc=0\n"), "00", "sql", 0).unwrap(),
            1
        );
        assert!(matches!(
            validate_shard_done(&format!("{marker}test_rc=0\n"), "01", "sql", 0),
            Err(AppError::Integrity(_))
        ));
    }

    #[test]
    fn parses_junit_failure_body_without_losing_diff() {
        let cases = parse_junit(
            r#"<testsuite><testcase name="a"><failure><![CDATA[x
[Diff]
-a
+b]]></failure></testcase></testsuite>"#,
        )
        .unwrap();
        assert_eq!(cases[0].failure.as_deref(), Some("x\n[Diff]\n-a\n+b"));
    }

    #[test]
    fn rejects_ambiguous_duplicate_provenance_fields() {
        let error = parse_build_provenance(
            "sha=1111111111111111111111111111111111111111\nsha=2222222222222222222222222222222222222222\nrun_id=1\n",
        )
        .unwrap_err();
        assert_eq!(error.kind(), crate::error::ExitKind::Integrity);
    }

    #[test]
    fn rejects_traversal_and_redacts_durable_logs() {
        let error = directory_entries(r#"<a href="%2e%2e/secret.xml">x</a>"#, false).unwrap_err();
        assert!(matches!(error, AppError::Malformed(_)));
        let sanitized = sanitize_log(
            "Authorization: Bearer actual-secret\nurl=https://example.test/file?X-Amz-Signature=secret",
        );
        assert!(!sanitized.contains("Bearer"));
        assert!(!sanitized.contains("actual-secret"));
        assert!(!sanitized.contains("Signature=secret"));
        assert!(sanitized.contains("[REDACTED]"));
    }
}
