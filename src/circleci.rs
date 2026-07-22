use regex::Regex;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::AppError;
use crate::http::send_with_retry;
use crate::model::{
    CircleArtifact, CircleJob, CircleTestsResponse, RepositoryRef, Suite, TestCase,
};

#[derive(Debug, Clone)]
pub struct Fetched<T> {
    pub value: T,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct CircleCiClient {
    client: Client,
    api_base: String,
    repo: RepositoryRef,
}

impl CircleCiClient {
    pub fn new(client: Client, api_base: impl Into<String>, repo: RepositoryRef) -> Self {
        Self {
            client,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            repo,
        }
    }

    pub fn job_api(&self, build_number: u64) -> String {
        format!(
            "{}/project/github/{}/{}/{}",
            self.api_base, self.repo.owner, self.repo.name, build_number
        )
    }

    pub fn tests_api(&self, build_number: u64) -> String {
        format!("{}/tests", self.job_api(build_number))
    }

    pub fn artifacts_api(&self, build_number: u64) -> String {
        format!("{}/artifacts", self.job_api(build_number))
    }

    pub async fn job(&self, build_number: u64) -> Result<Fetched<CircleJob>, AppError> {
        self.get_json(&self.job_api(build_number)).await
    }

    pub async fn tests(&self, build_number: u64) -> Result<Fetched<CircleTestsResponse>, AppError> {
        self.get_json(&self.tests_api(build_number)).await
    }

    pub async fn artifacts(
        &self,
        build_number: u64,
    ) -> Result<Fetched<Vec<CircleArtifact>>, AppError> {
        self.get_json(&self.artifacts_api(build_number)).await
    }

    pub fn validate_job(
        job: &CircleJob,
        build_number: u64,
        commit: &str,
        suite: Suite,
    ) -> Result<(), AppError> {
        if !job.vcs_revision.eq_ignore_ascii_case(commit) {
            return Err(AppError::Integrity(format!(
                "requested commit is {commit}, but CircleCI job {build_number} tested {}",
                job.vcs_revision
            )));
        }
        if job.workflows.job_name != suite.job_name() {
            return Err(AppError::Integrity(format!(
                "requested {}, but CircleCI job {build_number} metadata says {}",
                suite.job_name(),
                job.workflows.job_name
            )));
        }
        if job.build_num != build_number {
            return Err(AppError::Integrity(format!(
                "status URL identifies CircleCI job {build_number}, but metadata says {}",
                job.build_num
            )));
        }
        Ok(())
    }

    pub fn failed_node_indexes(job: &CircleJob) -> Vec<u32> {
        let mut nodes: Vec<_> = job
            .steps
            .iter()
            .flat_map(|step| &step.actions)
            .filter(|action| {
                action.failed == Some(true)
                    || action
                        .status
                        .as_deref()
                        .is_some_and(|status| status.eq_ignore_ascii_case("failed"))
            })
            .filter_map(|action| action.index)
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    pub fn testcase_revision(tests: &[TestCase]) -> Option<String> {
        let pattern = Regex::new(r"/blob/([0-9a-fA-F]{40})/").expect("valid revision regex");
        tests
            .iter()
            .filter_map(|test| test.message.as_deref())
            .find_map(|message| {
                pattern
                    .captures(message)
                    .and_then(|capture| capture.get(1))
                    .map(|value| value.as_str().to_ascii_lowercase())
            })
    }

    pub fn http_client(&self) -> &Client {
        &self.client
    }

    async fn get_json<T: DeserializeOwned>(&self, endpoint: &str) -> Result<Fetched<T>, AppError> {
        let response = send_with_retry(self.client.get(endpoint), "CircleCI", endpoint).await?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| AppError::Remote(format!("read {endpoint}: {error}")))?;
        if !status.is_success() {
            return Err(remote_status_error(endpoint, status, &bytes));
        }
        let raw: Value = serde_json::from_slice(&bytes).map_err(|source| AppError::Json {
            endpoint: endpoint.to_owned(),
            source,
        })?;
        let value = serde_json::from_value(raw.clone()).map_err(|source| AppError::Json {
            endpoint: endpoint.to_owned(),
            source,
        })?;
        Ok(Fetched { value, raw })
    }
}

fn remote_status_error(endpoint: &str, status: StatusCode, body: &[u8]) -> AppError {
    let detail = String::from_utf8_lossy(&body[..body.len().min(1_000)]);
    AppError::Remote(format!(
        "CircleCI GET {endpoint} returned {status}: {}",
        detail.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CircleWorkflow;

    fn job() -> CircleJob {
        CircleJob {
            build_num: 42,
            status: "failed".to_owned(),
            vcs_revision: "a".repeat(40),
            queued_at: None,
            start_time: None,
            stop_time: None,
            build_time_millis: None,
            parallel: Some(10),
            workflows: CircleWorkflow {
                job_name: "test_sql".to_owned(),
                workflow_id: None,
                workflow_name: None,
            },
            steps: vec![],
        }
    }

    #[test]
    fn validates_all_job_identity_fields() {
        CircleCiClient::validate_job(&job(), 42, &"a".repeat(40), Suite::Sql).unwrap();
        assert!(CircleCiClient::validate_job(&job(), 43, &"a".repeat(40), Suite::Sql).is_err());
        assert!(CircleCiClient::validate_job(&job(), 42, &"b".repeat(40), Suite::Sql).is_err());
        assert!(CircleCiClient::validate_job(&job(), 42, &"a".repeat(40), Suite::Shell).is_err());
    }

    #[test]
    fn finds_testcase_revision_in_failure_messages() {
        let test = TestCase {
            name: "x".to_owned(),
            file: None,
            class_name: None,
            source: None,
            result: "failure".to_owned(),
            message: Some(format!(
                "https://github.com/CUBRID/cubrid-testcases/blob/{}/sql/a.sql",
                "c".repeat(40)
            )),
            run_time: None,
            extra: Default::default(),
        };
        assert_eq!(
            CircleCiClient::testcase_revision(&[test]).as_deref(),
            Some("cccccccccccccccccccccccccccccccccccccccc")
        );
    }
}
