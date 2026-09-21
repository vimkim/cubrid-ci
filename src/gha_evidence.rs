use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use walkdir::WalkDir;

use crate::diff::{extract_diff, stable_test_id};
use crate::error::AppError;
use crate::model::Suite;
use crate::storage::{write_json_atomic, write_string_atomic};

const MAX_INDEX_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
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

pub struct SuiteRequest<'a> {
    pub suite: Suite,
    pub run_id: u64,
    pub attempt: u64,
    pub commit: &'a str,
    pub ci_state: &'a str,
    pub artifact_base: Option<&'a Url>,
    pub evidence_dir: &'a Path,
    pub status_snapshot_json: &'a [u8],
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

pub async fn collect_suite(request: SuiteRequest<'_>) -> Result<SuiteSummary, AppError> {
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
    write_string_atomic(
        &provider_raw.join("github/status-snapshot.json"),
        &String::from_utf8_lossy(request.status_snapshot_json),
    )?;
    let run_endpoint = format!("repos/CUBRID/cubrid/actions/runs/{}", request.run_id);
    let run_json = gh_output(&run_endpoint)?;
    let run: ActionRun = serde_json::from_slice(&run_json).map_err(|source| AppError::Json {
        endpoint: run_endpoint,
        source,
    })?;
    if run.id != request.run_id || run.run_attempt != request.attempt {
        return Err(AppError::Integrity(
            "Actions run metadata changed after suite selection".to_owned(),
        ));
    }
    write_string_atomic(
        &provider_raw.join("github/run.json"),
        &String::from_utf8_lossy(&run_json),
    )?;
    let resolution = resolve_artifact_base(
        request.run_id,
        request.attempt,
        layout.remote_name,
        request.artifact_base,
        &provider_raw,
    )?;
    let artifact_base = resolution.base;
    validate_artifact_base(&artifact_base)?;
    let client = Client::builder()
        .build()
        .map_err(|error| AppError::Remote(format!("create evidence-server client: {error}")))?;
    let run_root = artifact_base
        .join(&format!("runs/{}/{}/", request.run_id, layout.remote_name))
        .map_err(|error| AppError::Remote(format!("construct evidence URL: {error}")))?;

    let split_meta = classify_pipeline_fetch(
        fetch_text(
            &client,
            run_root.join("plan/split.meta").unwrap(),
            MAX_INDEX_BYTES,
        )
        .await,
        &resolution.jobs,
        &provider_raw,
        "plan metadata",
        false,
    )?;
    let plan = parse_split_meta(&split_meta)?;
    write_string_atomic(&provider_raw.join("plan/split.meta"), &split_meta)?;
    let planned_index = classify_pipeline_fetch(
        fetch_text(
            &client,
            run_root.join("plan/shards/").unwrap(),
            MAX_INDEX_BYTES,
        )
        .await,
        &resolution.jobs,
        &provider_raw,
        "planned-shard index",
        false,
    )?;
    write_string_atomic(
        &provider_raw.join("indexes/planned-shards.html"),
        &planned_index,
    )?;
    let planned_shards = directory_entries(&planned_index, false)?
        .into_iter()
        .filter_map(|name| name.strip_suffix(".list").map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    let plan_table_text = classify_pipeline_fetch(
        fetch_text(
            &client,
            run_root.join("plan/plan.tsv").unwrap(),
            MAX_TEXT_BYTES,
        )
        .await,
        &resolution.jobs,
        &provider_raw,
        "plan table",
        false,
    )?;
    write_string_atomic(&provider_raw.join("plan/plan.tsv"), &plan_table_text)?;
    let plan_table = parse_plan_table(&plan_table_text)?;
    if plan_table.values().sum::<u64>() != plan.total {
        return Err(AppError::Integrity(
            "plan.tsv assigned counts do not add up to split.meta total".to_owned(),
        ));
    }

    let failed_list = classify_pipeline_fetch(
        fetch_text(
            &client,
            run_root.join("collect/failed.list").unwrap(),
            MAX_TEXT_BYTES,
        )
        .await,
        &resolution.jobs,
        &provider_raw,
        "collector failure inventory",
        true,
    )?;
    let verdict = classify_pipeline_fetch(
        fetch_text(
            &client,
            run_root.join("collect/verdict").unwrap(),
            MAX_INDEX_BYTES,
        )
        .await,
        &resolution.jobs,
        &provider_raw,
        "collector verdict",
        true,
    )?;
    write_string_atomic(&provider_raw.join("collect/failed.list"), &failed_list)?;
    write_string_atomic(&provider_raw.join("collect/verdict"), &verdict)?;

    let shard_index = classify_pipeline_fetch(
        fetch_text(&client, run_root.join("shard/").unwrap(), MAX_INDEX_BYTES).await,
        &resolution.jobs,
        &provider_raw,
        "published-shard index",
        true,
    )?;
    write_string_atomic(&provider_raw.join("indexes/shards.html"), &shard_index)?;
    let shard_names = directory_entries(&shard_index, true)?;
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

    for shard in shard_names {
        let shard_root = run_root.join(&format!("shard/{shard}/")).unwrap();
        let build_text = classify_pipeline_fetch(
            fetch_text(
                &client,
                shard_root.join("build.read").unwrap(),
                MAX_INDEX_BYTES,
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            &format!("shard {shard} build provenance"),
            false,
        )?;
        let testcase_text = classify_pipeline_fetch(
            fetch_text(
                &client,
                shard_root.join("tc.read").unwrap(),
                MAX_INDEX_BYTES,
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            &format!("shard {shard} testcase provenance"),
            false,
        )?;
        write_string_atomic(
            &provider_raw.join(format!("shards/{shard}/build.read")),
            &build_text,
        )?;
        write_string_atomic(
            &provider_raw.join(format!("shards/{shard}/tc.read")),
            &testcase_text,
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

        let workflow_result = classify_pipeline_fetch(
            fetch_text(
                &client,
                shard_root.join(layout.result_file).unwrap(),
                MAX_TEXT_BYTES,
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            &format!("shard {shard} workflow result"),
            true,
        )?;
        let shard_done = classify_pipeline_fetch(
            fetch_text(
                &client,
                shard_root.join("shard.done").unwrap(),
                MAX_INDEX_BYTES,
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            &format!("shard {shard} completion record"),
            true,
        )?;
        write_string_atomic(
            &provider_raw.join(format!("shards/{shard}/{}", layout.result_file)),
            &workflow_result,
        )?;
        write_string_atomic(
            &provider_raw.join(format!("shards/{shard}/shard.done")),
            &shard_done,
        )?;
        let assigned = match validate_shard_done(&shard_done, &shard, layout.remote_name) {
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
        let shard_counts = match layout.result_format {
            ResultFormat::SummaryInfo => parse_summary_info(&workflow_result)?,
            ResultFormat::TestStatus => parse_test_status(&workflow_result)?,
        };
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

        let results_index = classify_pipeline_fetch(
            fetch_text(
                &client,
                shard_root.join("test-results/").unwrap(),
                MAX_INDEX_BYTES,
            )
            .await,
            &resolution.jobs,
            &provider_raw,
            &format!("shard {shard} test-result index"),
            true,
        )?;
        write_string_atomic(
            &provider_raw.join(format!("indexes/shard-{shard}-results.html")),
            &results_index,
        )?;
        let junit_files = directory_entries(&results_index, false)?
            .into_iter()
            .filter(|name| name.ends_with(".xml"))
            .collect::<Vec<_>>();
        if junit_files.is_empty() {
            return job_level_failure(
                format!("shard {shard} has no {} JUnit XML", request.suite),
                &resolution.jobs,
                &provider_raw,
            );
        }

        let mut junit_shard_counts = WorkflowCounts::default();
        for file in &junit_files {
            let xml = fetch_text(
                &client,
                shard_root.join(&format!("test-results/{file}")).unwrap(),
                MAX_TEXT_BYTES,
            )
            .await?;
            write_string_atomic(&provider_raw.join(format!("shards/{shard}/{file}")), &xml)?;
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
        write_string_atomic(&failure_dir.join("message.txt"), &message)?;
        let diff = extract_diff(&message);
        let diff_path = diff
            .as_ref()
            .map(|_| PathBuf::from(format!("failures/{stable_id}/diff.txt")));
        if let Some(diff) = &diff {
            write_string_atomic(&failure_dir.join("diff.txt"), &format!("{diff}\n"))?;
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
        write_json_atomic(
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
    let records_agree = (status_success && verdict == "pass" && junit_failure_count == 0)
        || (status_failure && verdict == "fail" && junit_failure_count > 0);
    if !records_agree {
        return Err(AppError::Integrity(format!(
            "suite status {}, verdict {verdict}, and JUnit failure count {junit_failure_count} disagree",
            request.ci_state
        )));
    }

    write_raw_index(
        &provider_raw,
        request.run_id,
        request.attempt,
        request.suite.job_name(),
    )?;

    let summary = SuiteSummary {
        schema_version: 2,
        suite: request.suite.job_name().to_owned(),
        ci_state: request.ci_state.to_ascii_lowercase(),
        collection_state: "complete".to_owned(),
        run_id: request.run_id,
        attempt: request.attempt,
        artifact_base: artifact_base.to_string(),
        verdict: verdict.to_owned(),
        counts,
        shards,
        failures,
    };
    write_json_atomic(&suite_dir.join("summary.json"), &summary)?;
    Ok(summary)
}

struct EvidenceResolution {
    base: Url,
    jobs: Vec<ActionJob>,
}

fn resolve_artifact_base(
    run_id: u64,
    attempt: u64,
    suite: &str,
    configured: Option<&Url>,
    provider_raw: &Path,
) -> Result<EvidenceResolution, AppError> {
    let jobs = load_jobs(run_id, provider_raw)?;
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
        return Ok(EvidenceResolution {
            base: configured.clone(),
            jobs: diagnostic_jobs,
        });
    }
    let collect_job_id = collect_job.id;
    let logs_endpoint = format!("repos/CUBRID/cubrid/actions/jobs/{collect_job_id}/logs");
    let output = match gh_job_log(&logs_endpoint) {
        Ok(output) => output,
        Err(error) if job_is_abnormal(collect_job) => {
            write_string_atomic(
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
    write_string_atomic(
        &provider_raw.join("github/collect.log"),
        &String::from_utf8_lossy(&output),
    )?;
    let log = String::from_utf8(output)
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
    Ok(EvidenceResolution {
        base: url,
        jobs: diagnostic_jobs,
    })
}

fn load_jobs(run_id: u64, provider_raw: &Path) -> Result<Vec<ActionJob>, AppError> {
    let mut all_jobs = Vec::new();
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
        write_string_atomic(
            &provider_raw.join("github").join(name),
            &String::from_utf8_lossy(&json),
        )?;
        let response: JobsResponse =
            serde_json::from_slice(&json).map_err(|source| AppError::Json { endpoint, source })?;
        let page_count = response.jobs.len();
        all_jobs.extend(response.jobs);
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
    Ok(all_jobs)
}

fn classify_pipeline_fetch(
    result: Result<String, AppError>,
    jobs: &[ActionJob],
    provider_raw: &Path,
    stage: &str,
    terminal_required: bool,
) -> Result<String, AppError> {
    match result {
        Ok(value) => Ok(value),
        Err(error)
            if jobs.iter().any(job_is_abnormal)
                || (terminal_required && matches!(&error, AppError::Unavailable(_))) =>
        {
            retain_abnormal_job_logs(jobs, provider_raw)?;
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
            Err(AppError::JobLevel(format!(
                "{stage} is unavailable after {context}; {error}"
            )))
        }
        Err(error) => Err(error),
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
            Ok(log) => write_string_atomic(
                &provider_raw.join(format!("github/job-{}.log", job.id)),
                &String::from_utf8_lossy(&log),
            )?,
            Err(error) => write_string_atomic(
                &provider_raw.join(format!("github/job-{}-log-error.txt", job.id)),
                &error.to_string(),
            )?,
        }
    }
    Ok(())
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
        return Err(AppError::Remote(format!(
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

async fn fetch_text(client: &Client, url: Url, limit: usize) -> Result<String, AppError> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| AppError::Remote(format!("GET {url}: {error}")))?;
    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(AppError::Unavailable(format!("GET {url} returned 404")));
        }
        return Err(AppError::Remote(format!(
            "GET {url} returned {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AppError::Remote(format!(
            "GET {url} exceeds the {limit}-byte text limit"
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::Remote(format!("read {url}: {error}")))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(AppError::Remote(format!(
                "GET {url} exceeds the {limit}-byte text limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| AppError::Remote(format!("GET {url} is not UTF-8 text")))
}

fn directory_entries(html: &str, directories: bool) -> Result<Vec<String>, AppError> {
    let href = Regex::new(r#"(?i)href\s*=\s*["']([^"']+)["']"#).expect("valid href regex");
    let mut entries = Vec::new();
    for value in href
        .captures_iter(html)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str()))
    {
        let value = value.trim_end_matches('/');
        if value.is_empty() || value.contains('/') || matches!(value, "." | "..") {
            continue;
        }
        if directories && !value.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if !directories
            && !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            continue;
        }
        entries.push(value.to_owned());
    }
    entries.sort();
    entries.dedup();
    if entries.len() > 10_000 {
        return Err(AppError::Integrity(
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
            .ok_or_else(|| AppError::Integrity(format!("malformed key-value row {}", index + 1)))?;
        if name.is_empty() || fields.insert(name, field_value).is_some() {
            return Err(AppError::Integrity(format!(
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
            AppError::Integrity(format!("build.read is missing required field {name}"))
        })
    };
    Ok(BuildProvenance {
        sha: required("sha")?.to_owned(),
        mode: fields.get("mode").map(|value| (*value).to_owned()),
        namespace: fields.get("ns").map(|value| (*value).to_owned()),
        run_id: required("run_id")?
            .parse()
            .map_err(|_| AppError::Integrity("build.read has invalid run_id".to_owned()))?,
        run_attempt: fields
            .get("run_attempt")
            .map(|value| value.parse())
            .transpose()
            .map_err(|_| AppError::Integrity("build.read has invalid run_attempt".to_owned()))?,
    })
}

fn parse_testcase_provenance(value: &str) -> Result<TestcaseProvenance, AppError> {
    let fields = parse_key_values(value)?;
    let sha = fields
        .get("tc_sha")
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Integrity("tc.read has no valid tc_sha".to_owned()))?;
    let branch = fields
        .get("tc_branch")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Integrity("tc.read has no tc_branch".to_owned()))?;
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
                AppError::Integrity("collect/failed.list has a malformed row".to_owned())
            })?;
            if shard.bytes().all(|byte| byte.is_ascii_digit()) && !name.is_empty() {
                Ok((shard.to_owned(), name.to_owned()))
            } else {
                Err(AppError::Integrity(
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
}

fn parse_split_meta(value: &str) -> Result<SqlPlan, AppError> {
    let fields = parse_key_values(value)?;
    let field = |name| {
        fields
            .get(name)
            .map(|value| value.trim_matches('\''))
            .ok_or_else(|| AppError::Integrity(format!("split.meta is missing {name}")))
    };
    let total = field("total")?
        .parse()
        .map_err(|_| AppError::Integrity("split.meta has invalid total".to_owned()))?;
    let parallelism = field("par")?
        .parse()
        .map_err(|_| AppError::Integrity("split.meta has invalid par".to_owned()))?;
    let testcase_sha = field("tc_sha")?;
    if testcase_sha.len() != 40 || !testcase_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::Integrity(
            "split.meta has invalid tc_sha".to_owned(),
        ));
    }
    let testcase_branch = field("tc_branch")?;
    if testcase_branch.is_empty() {
        return Err(AppError::Integrity(
            "split.meta has empty tc_branch".to_owned(),
        ));
    }
    Ok(SqlPlan {
        total,
        parallelism,
        testcase_sha: testcase_sha.to_owned(),
        testcase_branch: testcase_branch.to_owned(),
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
            return Err(AppError::Integrity(format!(
                "plan.tsv has a malformed row {}",
                index + 1
            )));
        }
        let count = fields[1].parse().map_err(|_| {
            AppError::Integrity(format!(
                "plan.tsv has an invalid count on row {}",
                index + 1
            ))
        })?;
        if plan.insert(fields[0].to_owned(), count).is_some() {
            return Err(AppError::Integrity(format!(
                "plan.tsv repeats shard {}",
                fields[0]
            )));
        }
    }
    if plan.is_empty() {
        return Err(AppError::Integrity("plan.tsv is empty".to_owned()));
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
        return Err(AppError::Integrity(
            "summary_info contains no testcase verdict rows".to_owned(),
        ));
    }
    Ok(counts)
}

fn parse_test_status(value: &str) -> Result<WorkflowCounts, AppError> {
    let fields = parse_key_values(value)?;
    let count = |name| {
        fields
            .get(name)
            .ok_or_else(|| AppError::Integrity(format!("test_status.data is missing {name}")))?
            .parse::<u64>()
            .map_err(|_| AppError::Integrity(format!("test_status.data has invalid {name}")))
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
) -> Result<u64, AppError> {
    let fields = parse_key_values(value)?;
    if fields.get("suite") != Some(&expected_suite) || fields.get("idx") != Some(&expected_index) {
        return Err(AppError::Integrity(format!(
            "shard {expected_index} completion record has inconsistent identity"
        )));
    }
    let ctp_rc = fields
        .get("ctp_rc")
        .ok_or_else(|| AppError::JobLevel(format!("shard {expected_index} has no ctp_rc")))?;
    if *ctp_rc != "0" {
        return Err(AppError::JobLevel(format!(
            "shard {expected_index} exited abnormally with ctp_rc={ctp_rc}"
        )));
    }
    fields
        .get("assigned")
        .ok_or_else(|| AppError::Integrity("shard.done is missing assigned".to_owned()))?
        .parse()
        .map_err(|_| AppError::Integrity("shard.done has invalid assigned".to_owned()))
}

#[derive(Debug, Serialize)]
struct RawEvidenceIndex {
    schema_version: u32,
    run_id: u64,
    attempt: u64,
    suite: String,
    files: Vec<RawFileRecord>,
}

#[derive(Debug, Serialize)]
struct RawFileRecord {
    path: PathBuf,
    size_bytes: u64,
    sha256: String,
}

fn write_raw_index(raw_dir: &Path, run_id: u64, attempt: u64, suite: &str) -> Result<(), AppError> {
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
    write_json_atomic(
        &raw_dir.join("index.json"),
        &RawEvidenceIndex {
            schema_version: 2,
            run_id,
            attempt,
            suite: suite.to_owned(),
            files,
        },
    )
}

fn parse_junit(xml: &str) -> Result<Vec<JunitCase>, AppError> {
    let document = roxmltree::Document::parse(xml)
        .map_err(|error| AppError::Integrity(format!("invalid JUnit XML: {error}")))?;
    document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
        .map(|node| {
            let name = node
                .attribute("name")
                .filter(|name| !name.is_empty())
                .ok_or_else(|| AppError::Integrity("JUnit testcase has no name".to_owned()))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
