use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use reqwest::Client;
use tracing::{debug, info};

use crate::artifacts::ArtifactDownloader;
use crate::circleci::CircleCiClient;
use crate::cli::{ArtifactMode, Cli};
use crate::diff::{extract_diff, node_index, stable_test_id};
use crate::error::AppError;
use crate::github::{GitHubClient, directory_identity, parse_circleci_job_number, parse_pr_url};
use crate::model::{
    CommandResult, CommitManifest, CommitStatus, FailureMetadata, PullRequest, RepositoryRef,
    StatusMap, Suite, SuiteManifest, SuiteState, SuiteSummary, TestCase,
};
use crate::stats::{duration_stats, known_counts, result_counts};
use crate::storage::{Storage, write_json_atomic, write_string_atomic};

#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub suite: Suite,
    pub pr_url: String,
    pub requested_commit: Option<String>,
    pub data_dir: PathBuf,
    pub wait: bool,
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub attempt: Option<u64>,
    pub artifact_mode: ArtifactMode,
    pub max_artifact_bytes: u64,
    pub download_concurrency: usize,
    pub include_test_sources: bool,
    pub github_api: String,
    pub circleci_api: String,
    pub github_token: Option<String>,
}

impl CollectorConfig {
    pub fn from_cli(cli: &Cli) -> Result<Self, AppError> {
        let (suite, args) = cli.suite_and_args();
        if args.timeout.is_zero() {
            return Err(AppError::Input(
                "--timeout must be greater than zero".to_owned(),
            ));
        }
        if args.poll_interval.is_zero() {
            return Err(AppError::Input(
                "--poll-interval must be greater than zero".to_owned(),
            ));
        }
        if args.max_artifact_bytes == 0 {
            return Err(AppError::Input(
                "--max-artifact-bytes must be greater than zero".to_owned(),
            ));
        }
        if !(1..=32).contains(&args.download_concurrency) {
            return Err(AppError::Input(
                "--download-concurrency must be between 1 and 32".to_owned(),
            ));
        }
        Ok(Self {
            suite,
            pr_url: args.github_pr_url.clone(),
            requested_commit: args.commit.clone(),
            data_dir: args.data_dir.clone(),
            wait: args.wait,
            timeout: args.timeout,
            poll_interval: args.poll_interval,
            attempt: args.attempt,
            artifact_mode: args.artifact_mode,
            max_artifact_bytes: args.max_artifact_bytes,
            download_concurrency: args.download_concurrency,
            include_test_sources: args.include_test_sources,
            github_api: cli.github_api.clone(),
            circleci_api: cli.circleci_api.clone(),
            github_token: env::var("GH_TOKEN")
                .ok()
                .or_else(|| env::var("GITHUB_TOKEN").ok())
                .filter(|value| !value.trim().is_empty()),
        })
    }
}

pub struct Collector {
    config: CollectorConfig,
    http: Client,
    github: GitHubClient,
}

impl Collector {
    pub fn new(config: CollectorConfig) -> Result<Self, AppError> {
        let http = Client::builder()
            .user_agent(format!(
                "cubrid-circleci-analyzer/{}",
                crate::build_info::VERSION
            ))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|error| AppError::Remote(format!("build HTTP client: {error}")))?;
        let github = GitHubClient::new(
            http.clone(),
            config.github_api.clone(),
            config.github_token.clone(),
        );
        Ok(Self {
            config,
            http,
            github,
        })
    }

    pub async fn run(&self) -> Result<CommandResult, AppError> {
        let repo = parse_pr_url(&self.config.pr_url)?;
        eprintln!(
            "cubrid-ci: resolving {}#{} for {}",
            repo.slug(),
            repo.pr_number,
            self.config.suite
        );
        info!(repository = %repo.slug(), pr = repo.pr_number, suite = %self.config.suite, "resolving pull request");
        let pull = self.github.pull_request(&repo).await?;
        if pull.number != repo.pr_number {
            return Err(AppError::Integrity(format!(
                "requested PR #{}, but GitHub returned PR #{}",
                repo.pr_number, pull.number
            )));
        }
        let commit = self
            .github
            .resolve_commit(&repo, &pull, self.config.requested_commit.as_deref())
            .await?;
        let short_sha = commit[..7].to_owned();
        eprintln!("cubrid-ci: pinned commit {short_sha}");
        let directory_identity = directory_identity(&pull);
        let storage = Storage::new(
            &self.config.data_dir,
            directory_identity.clone(),
            short_sha.clone(),
        );
        storage.initialize()?;

        let started = Instant::now();
        let (status, latest, mut manifest) = loop {
            eprintln!(
                "cubrid-ci: checking GitHub status for {}",
                self.config.suite
            );
            let statuses = self.github.statuses(&repo, &commit).await?;
            let latest = GitHubClient::latest_statuses(&statuses);
            let selected = GitHubClient::select_suite_status(
                &statuses,
                self.config.suite,
                self.config.attempt,
            );
            let mut manifest = self.build_manifest(
                &repo,
                &pull,
                &commit,
                &short_sha,
                &directory_identity,
                &latest,
            );

            let selected = match selected {
                Ok(value) => value,
                Err(error) if self.config.wait && started.elapsed() < self.config.timeout => {
                    debug!(error = %error, "requested attempt is not attached yet");
                    storage.write_manifest(&manifest)?;
                    eprintln!(
                        "cubrid-ci: requested attempt is not attached; retrying in {}",
                        humantime::format_duration(self.config.poll_interval)
                    );
                    tokio::time::sleep(self.config.poll_interval).await;
                    continue;
                }
                Err(error) => {
                    mark_suite(
                        &mut manifest,
                        self.config.suite,
                        if started.elapsed() >= self.config.timeout {
                            SuiteState::TimedOut
                        } else {
                            SuiteState::Missing
                        },
                        Some(error.to_string()),
                    );
                    storage.write_manifest(&manifest)?;
                    return Err(error);
                }
            };

            if let Some(failure) = failed_prerequisite(self.config.suite, &latest) {
                let diagnostic = format!(
                    "prerequisite '{}' ended in state '{}'",
                    failure.context, failure.state
                );
                mark_suite(
                    &mut manifest,
                    self.config.suite,
                    SuiteState::BuildFailed,
                    Some(diagnostic.clone()),
                );
                storage.write_manifest(&manifest)?;
                return Err(AppError::Unavailable(diagnostic));
            }

            if let Some(status) = selected {
                update_suite_status(&mut manifest, self.config.suite, &status);
                storage.write_manifest(&manifest)?;
                if status.is_terminal() {
                    break (status, latest, manifest);
                }
            } else {
                storage.write_manifest(&manifest)?;
            }

            if !self.config.wait {
                return Err(AppError::Unavailable(format!(
                    "{} status for commit {short_sha} is missing or pending",
                    self.config.suite
                )));
            }
            if started.elapsed() >= self.config.timeout {
                mark_suite(
                    &mut manifest,
                    self.config.suite,
                    SuiteState::TimedOut,
                    Some(format!("wait timed out after {:?}", self.config.timeout)),
                );
                storage.write_manifest(&manifest)?;
                return Err(AppError::Unavailable(format!(
                    "timed out waiting for {} at {short_sha}",
                    self.config.suite
                )));
            }
            eprintln!(
                "cubrid-ci: {} is pending; retrying in {}",
                self.config.suite,
                humantime::format_duration(self.config.poll_interval)
            );
            tokio::time::sleep(self.config.poll_interval).await;
        };

        let result = self
            .collect_terminal(
                &repo, &pull, &commit, &short_sha, &status, &latest, &storage,
            )
            .await;
        match result {
            Ok(result) => {
                mark_suite(
                    &mut manifest,
                    self.config.suite,
                    SuiteState::Completed,
                    None,
                );
                if let Some(suite) = manifest.suites.get_mut(self.config.suite.job_name()) {
                    suite.circleci_job_number = Some(result.circleci_job_number);
                    suite.status_state = Some(status.state.clone());
                    suite.status_url = status.target_url.clone();
                }
                manifest.collected_at = Utc::now();
                storage.write_manifest(&manifest)?;
                Ok(result)
            }
            Err(error) => {
                mark_suite(
                    &mut manifest,
                    self.config.suite,
                    SuiteState::CollectionFailed,
                    Some(error.to_string()),
                );
                manifest.collected_at = Utc::now();
                let _ = storage.write_manifest(&manifest);
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn collect_terminal(
        &self,
        repo: &RepositoryRef,
        pull: &PullRequest,
        commit: &str,
        short_sha: &str,
        status: &CommitStatus,
        latest: &StatusMap,
        storage: &Storage,
    ) -> Result<CommandResult, AppError> {
        let target_url = status.target_url.as_deref().ok_or_else(|| {
            AppError::Integrity(format!(
                "terminal {} status has no CircleCI target URL",
                self.config.suite
            ))
        })?;
        let build_number = parse_circleci_job_number(target_url).ok_or_else(|| {
            AppError::Integrity(format!(
                "cannot extract CircleCI job number from '{target_url}'"
            ))
        })?;
        if self
            .config
            .attempt
            .is_some_and(|attempt| attempt != build_number)
        {
            return Err(AppError::Integrity(format!(
                "selected status points to job {build_number}, not requested attempt {}",
                self.config.attempt.unwrap_or_default()
            )));
        }

        eprintln!(
            "cubrid-ci: collecting CircleCI {} job {build_number}",
            self.config.suite
        );
        let circle = CircleCiClient::new(
            self.http.clone(),
            self.config.circleci_api.clone(),
            repo.clone(),
        );
        eprintln!("cubrid-ci: fetching job metadata, tests, and artifact manifest");
        let fetched_job = circle.job(build_number).await?;
        CircleCiClient::validate_job(&fetched_job.value, build_number, commit, self.config.suite)?;
        let (fetched_tests, fetched_artifacts) =
            tokio::try_join!(circle.tests(build_number), circle.artifacts(build_number))?;

        let staging = storage.staging(self.config.suite)?;
        storage.seed_attempt_history(self.config.suite, staging.path())?;
        let attempt_raw = staging
            .path()
            .join("attempts")
            .join(build_number.to_string())
            .join("raw");
        write_json_atomic(&attempt_raw.join("github-status.json"), status)?;
        write_json_atomic(&attempt_raw.join("job.json"), &fetched_job.raw)?;
        write_json_atomic(&attempt_raw.join("tests.json"), &fetched_tests.raw)?;
        write_json_atomic(&attempt_raw.join("artifacts.json"), &fetched_artifacts.raw)?;

        let failures: Vec<_> = fetched_tests
            .value
            .tests
            .iter()
            .filter(|test| test.result.eq_ignore_ascii_case("failure"))
            .cloned()
            .collect();
        eprintln!(
            "cubrid-ci: tests: total={}, failed={}; artifacts: listed={}",
            fetched_tests.value.tests.len(),
            failures.len(),
            fetched_artifacts.value.len()
        );
        write_json_atomic(&staging.path().join("failed-tests.json"), &failures)?;
        let failed_names = if failures.is_empty() {
            String::new()
        } else {
            format!(
                "{}\n",
                failures
                    .iter()
                    .map(|test| test.name.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        write_string_atomic(&staging.path().join("failed-tc.txt"), &failed_names)?;
        write_failures(staging.path(), &failures)?;

        let downloader = ArtifactDownloader::new(
            self.http.clone(),
            self.config.artifact_mode,
            self.config.max_artifact_bytes,
            self.config.download_concurrency,
            self.config.github_token.clone(),
        );
        eprintln!("cubrid-ci: downloading failed action logs");
        let logs = downloader
            .download_failed_action_logs(&fetched_job.value, staging.path())
            .await?;
        let downloaded_log_count = logs.iter().filter(|log| log.downloaded).count();
        eprintln!(
            "cubrid-ci: failed action logs: captured={}, unavailable={}",
            downloaded_log_count,
            logs.len() - downloaded_log_count
        );
        if self.config.artifact_mode == ArtifactMode::Manifest {
            eprintln!(
                "cubrid-ci: artifact payloads: skipped (manifest mode, listed={})",
                fetched_artifacts.value.len()
            );
        } else {
            let mode = match self.config.artifact_mode {
                ArtifactMode::Text => "text",
                ArtifactMode::All => "all",
                ArtifactMode::Manifest => unreachable!("manifest mode handled above"),
            };
            eprintln!("cubrid-ci: downloading artifact payloads in {mode} mode");
        }
        let artifact_records = downloader
            .download_artifacts(&fetched_artifacts.value, staging.path())
            .await;
        write_json_atomic(&staging.path().join("artifacts.json"), &artifact_records)?;

        if self.config.artifact_mode != ArtifactMode::Manifest {
            let downloaded_count = artifact_records
                .iter()
                .filter(|artifact| artifact.downloaded)
                .count();
            let downloaded_bytes: u64 = artifact_records
                .iter()
                .filter_map(|artifact| artifact.size_bytes)
                .sum();
            eprintln!(
                "cubrid-ci: artifact payloads: downloaded={downloaded_count}, bytes={downloaded_bytes}"
            );
        }

        if self.config.include_test_sources {
            eprintln!("cubrid-ci: downloading referenced testcase sources");
            let source_records = downloader
                .download_test_sources(&failures, staging.path())
                .await?;
            let downloaded_source_count = source_records
                .iter()
                .filter(|source| source.downloaded)
                .count();
            eprintln!(
                "cubrid-ci: testcase sources: downloaded={}, unavailable={}",
                downloaded_source_count,
                source_records.len() - downloaded_source_count
            );
        }

        let counts = result_counts(&fetched_tests.value.tests);
        let (success, failure, skipped, error, unknown) = known_counts(&counts);
        let downloaded_artifact_count = artifact_records
            .iter()
            .filter(|artifact| artifact.downloaded)
            .count();
        let downloaded_artifact_bytes = artifact_records
            .iter()
            .filter_map(|artifact| artifact.size_bytes)
            .sum();
        let job = &fetched_job.value;
        let prerequisites = prerequisite_snapshot(self.config.suite, latest);
        let queue_duration_seconds = match (job.queued_at, job.start_time) {
            (Some(queued), Some(started)) => {
                Some((started - queued).num_milliseconds() as f64 / 1_000.0)
            }
            _ => None,
        };
        let wall_duration_seconds = match (job.start_time, job.stop_time) {
            (Some(started), Some(stopped)) => {
                Some((stopped - started).num_milliseconds() as f64 / 1_000.0)
            }
            _ => job
                .build_time_millis
                .map(|milliseconds| milliseconds as f64 / 1_000.0),
        };
        let summary = SuiteSummary {
            schema_version: 1,
            suite: self.config.suite,
            repository: repo.slug(),
            pr_number: repo.pr_number,
            pr_url: pull.html_url.clone(),
            commit: commit.to_owned(),
            short_sha: short_sha.to_owned(),
            github_status_context: status.context.clone(),
            github_status_state: status.state.clone(),
            github_status_url: target_url.to_owned(),
            circleci_job_number: build_number,
            circleci_job_status: job.status.clone(),
            circleci_job_url: target_url.to_owned(),
            circleci_job_api: circle.job_api(build_number),
            circleci_tests_api: circle.tests_api(build_number),
            queued_at: job.queued_at,
            started_at: job.start_time,
            stopped_at: job.stop_time,
            queue_duration_seconds,
            wall_duration_seconds,
            parallelism: job.parallel,
            failed_node_indexes: CircleCiClient::failed_node_indexes(job),
            test_count: fetched_tests.value.tests.len(),
            result_counts: counts,
            success_count: success,
            failure_count: failure,
            skipped_count: skipped,
            error_count: error,
            unknown_count: unknown,
            test_durations: duration_stats(&fetched_tests.value.tests),
            artifact_count: fetched_artifacts.value.len(),
            downloaded_artifact_count,
            downloaded_artifact_bytes,
            prerequisites,
            testcase_revision: CircleCiClient::testcase_revision(&fetched_tests.value.tests),
        };
        write_json_atomic(&staging.path().join("summary.json"), &summary)?;
        eprintln!("cubrid-ci: publishing evidence");
        let output_dir = storage.publish_suite(self.config.suite, staging)?;
        eprintln!("cubrid-ci: published evidence: {}", output_dir.display());
        info!(job = build_number, output = %output_dir.display(), "published validated suite evidence");
        Ok(CommandResult {
            ok: true,
            suite: self.config.suite,
            repository: repo.slug(),
            pr_number: repo.pr_number,
            commit: commit.to_owned(),
            circleci_job_number: build_number,
            ci_status: status.state.clone(),
            test_count: summary.test_count,
            failure_count: summary.failure_count,
            output_dir,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_manifest(
        &self,
        repo: &RepositoryRef,
        pull: &PullRequest,
        commit: &str,
        short_sha: &str,
        directory_identity: &str,
        latest: &StatusMap,
    ) -> CommitManifest {
        let mut suites = BTreeMap::new();
        for suite in Suite::ALL {
            let status = latest.get(&suite.status_context());
            let state = match status {
                None => SuiteState::Missing,
                Some(status) if status.is_pending() => SuiteState::Pending,
                Some(_) => SuiteState::Completed,
            };
            suites.insert(
                suite.job_name().to_owned(),
                SuiteManifest {
                    state,
                    circleci_job_number: status
                        .and_then(|status| status.target_url.as_deref())
                        .and_then(parse_circleci_job_number),
                    status_state: status.map(|status| status.state.clone()),
                    status_url: status.and_then(|status| status.target_url.clone()),
                    diagnostic: None,
                },
            );
        }
        CommitManifest {
            schema_version: 1,
            tool_version: crate::build_info::VERSION.to_owned(),
            repository: repo.slug(),
            pr_number: repo.pr_number,
            pr_url: pull.html_url.clone(),
            pr_title: pull.title.clone(),
            head_branch: pull.head.ref_name.clone(),
            base_branch: pull.base.ref_name.clone(),
            requested_commit: self.config.requested_commit.clone(),
            resolved_commit: commit.to_owned(),
            short_sha: short_sha.to_owned(),
            directory_identity: directory_identity.to_owned(),
            collected_at: Utc::now(),
            prerequisites: all_prerequisite_snapshot(latest),
            suites,
        }
    }
}

fn write_failures(root: &Path, failures: &[TestCase]) -> Result<(), AppError> {
    for test in failures {
        let stable_id = stable_test_id(&test.name);
        let failure_dir = root.join("failures").join(&stable_id);
        let message = test.message.as_deref().unwrap_or("");
        let extracted = extract_diff(message);
        let metadata = FailureMetadata {
            stable_id,
            name: test.name.clone(),
            file: test.file.clone(),
            class_name: test.class_name.clone(),
            source: test.source.clone(),
            result: test.result.clone(),
            run_time: test.run_time,
            node_index: node_index(message),
            diff_extraction: if extracted.is_some() {
                "extracted".to_owned()
            } else {
                "unavailable".to_owned()
            },
        };
        write_json_atomic(&failure_dir.join("metadata.json"), &metadata)?;
        write_string_atomic(&failure_dir.join("message.txt"), message)?;
        write_string_atomic(
            &failure_dir.join("diff.txt"),
            extracted.as_deref().unwrap_or(""),
        )?;
    }
    Ok(())
}

fn failed_prerequisite(suite: Suite, latest: &StatusMap) -> Option<&CommitStatus> {
    suite.prerequisites().iter().find_map(|job| {
        latest
            .get(&format!("ci/circleci: {job}"))
            .filter(|status| status.is_terminal() && !status.is_success())
    })
}

fn prerequisite_snapshot(
    suite: Suite,
    latest: &StatusMap,
) -> BTreeMap<String, Option<CommitStatus>> {
    suite
        .prerequisites()
        .iter()
        .map(|job| {
            let context = format!("ci/circleci: {job}");
            (context.clone(), latest.get(&context).cloned())
        })
        .collect()
}

fn all_prerequisite_snapshot(latest: &StatusMap) -> BTreeMap<String, Option<CommitStatus>> {
    ["build", "build_debug", "download-build"]
        .into_iter()
        .map(|job| {
            let context = format!("ci/circleci: {job}");
            (context.clone(), latest.get(&context).cloned())
        })
        .collect()
}

fn update_suite_status(manifest: &mut CommitManifest, suite: Suite, status: &CommitStatus) {
    if let Some(entry) = manifest.suites.get_mut(suite.job_name()) {
        entry.state = if status.is_pending() {
            SuiteState::Pending
        } else {
            SuiteState::Completed
        };
        entry.circleci_job_number = status
            .target_url
            .as_deref()
            .and_then(parse_circleci_job_number);
        entry.status_state = Some(status.state.clone());
        entry.status_url = status.target_url.clone();
    }
}

fn mark_suite(
    manifest: &mut CommitManifest,
    suite: Suite,
    state: SuiteState,
    diagnostic: Option<String>,
) {
    if let Some(entry) = manifest.suites.get_mut(suite.job_name()) {
        entry.state = state;
        entry.diagnostic = diagnostic;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_complete_failure_evidence_without_fabricating_diff() {
        let root = tempfile::tempdir().unwrap();
        let failures = vec![TestCase {
            name: "shell/x/cases/x.sh".to_owned(),
            file: Some("x.sh".to_owned()),
            class_name: None,
            source: None,
            result: "failure".to_owned(),
            message: Some("Test failed".to_owned()),
            run_time: Some(1.0),
            extra: Default::default(),
        }];
        write_failures(root.path(), &failures).unwrap();
        let failure = std::fs::read_dir(root.path().join("failures"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(failure.join("message.txt")).unwrap(),
            "Test failed"
        );
        assert_eq!(
            std::fs::read_to_string(failure.join("diff.txt")).unwrap(),
            ""
        );
    }
}
