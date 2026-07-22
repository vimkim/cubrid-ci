use std::collections::HashMap;

use regex::Regex;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::error::AppError;
use crate::http::send_with_retry;
use crate::model::{CommitStatus, GitCommit, PullRequest, RepositoryRef, StatusMap, Suite};

const EXPECTED_OWNER: &str = "CUBRID";
const EXPECTED_REPOSITORY: &str = "cubrid";

#[derive(Debug, Clone)]
pub struct GitHubClient {
    client: Client,
    api_base: String,
    token: Option<String>,
}

impl GitHubClient {
    pub fn new(client: Client, api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            client,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            token,
        }
    }

    pub async fn pull_request(&self, repo: &RepositoryRef) -> Result<PullRequest, AppError> {
        self.get_json(&format!(
            "/repos/{}/{}/pulls/{}",
            repo.owner, repo.name, repo.pr_number
        ))
        .await
    }

    pub async fn resolve_commit(
        &self,
        repo: &RepositoryRef,
        pull: &PullRequest,
        requested: Option<&str>,
    ) -> Result<String, AppError> {
        let Some(requested) = requested else {
            validate_full_sha(&pull.head.sha)?;
            return Ok(pull.head.sha.to_ascii_lowercase());
        };

        if !Regex::new(r"(?i)^[0-9a-f]{7,40}$")
            .expect("valid sha regex")
            .is_match(requested)
        {
            return Err(AppError::Input(format!(
                "commit must be 7 to 40 hexadecimal characters, got '{requested}'"
            )));
        }

        let commit: GitCommit = self
            .get_json(&format!(
                "/repos/{}/{}/commits/{}",
                repo.owner, repo.name, requested
            ))
            .await?;
        validate_full_sha(&commit.sha)?;
        let sha = commit.sha.to_ascii_lowercase();

        if sha.eq_ignore_ascii_case(&pull.head.sha)
            || self.commit_is_in_pull(repo, pull.number, &sha).await?
        {
            Ok(sha)
        } else {
            Err(AppError::Input(format!(
                "commit {sha} is not associated with PR #{}",
                pull.number
            )))
        }
    }

    pub async fn statuses(
        &self,
        repo: &RepositoryRef,
        sha: &str,
    ) -> Result<Vec<CommitStatus>, AppError> {
        let mut statuses = Vec::new();
        for page in 1..=20 {
            let batch: Vec<CommitStatus> = self
                .get_json(&format!(
                    "/repos/{}/{}/commits/{sha}/statuses?per_page=100&page={page}",
                    repo.owner, repo.name
                ))
                .await?;
            let count = batch.len();
            statuses.extend(batch);
            if count < 100 {
                return Ok(statuses);
            }
        }
        Err(AppError::Remote(
            "GitHub returned more than 2,000 commit statuses; refusing incomplete discovery"
                .to_owned(),
        ))
    }

    pub fn latest_statuses(statuses: &[CommitStatus]) -> StatusMap {
        let mut latest = HashMap::new();
        for status in statuses {
            let replace = latest
                .get(&status.context)
                .is_none_or(|current: &CommitStatus| status.updated_at > current.updated_at);
            if replace {
                latest.insert(status.context.clone(), status.clone());
            }
        }
        latest
    }

    pub fn select_suite_status(
        statuses: &[CommitStatus],
        suite: Suite,
        attempt: Option<u64>,
    ) -> Result<Option<CommitStatus>, AppError> {
        let context = suite.status_context();
        let mut candidates: Vec<_> = statuses
            .iter()
            .filter(|status| status.context == context)
            .cloned()
            .collect();
        candidates.sort_by_key(|status| status.updated_at);

        if let Some(attempt) = attempt {
            let selected = candidates.into_iter().find(|status| {
                status
                    .target_url
                    .as_deref()
                    .and_then(parse_circleci_job_number)
                    == Some(attempt)
            });
            return selected.map(Some).ok_or_else(|| {
                AppError::Unavailable(format!(
                    "no GitHub status for {} attempt {attempt} on the requested commit",
                    suite.job_name()
                ))
            });
        }

        Ok(candidates.pop())
    }

    async fn commit_is_in_pull(
        &self,
        repo: &RepositoryRef,
        pr_number: u64,
        sha: &str,
    ) -> Result<bool, AppError> {
        let associated: Vec<PullAssociation> = self
            .get_json(&format!(
                "/repos/{}/{}/commits/{sha}/pulls?per_page=100",
                repo.owner, repo.name
            ))
            .await?;
        if associated.iter().any(|pull| pull.number == pr_number) {
            return Ok(true);
        }

        for page in 1..=100 {
            let commits: Vec<GitCommit> = self
                .get_json(&format!(
                    "/repos/{}/{}/pulls/{pr_number}/commits?per_page=100&page={page}",
                    repo.owner, repo.name
                ))
                .await?;
            if commits
                .iter()
                .any(|commit| commit.sha.eq_ignore_ascii_case(sha))
            {
                return Ok(true);
            }
            if commits.len() < 100 {
                return Ok(false);
            }
        }
        Err(AppError::Remote(format!(
            "PR #{pr_number} has more than 10,000 commits; association check is incomplete"
        )))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, AppError> {
        let endpoint = format!("{}{}", self.api_base, path);
        let mut request = self
            .client
            .get(&endpoint)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = send_with_retry(request, "GitHub", &endpoint).await?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| AppError::Remote(format!("read {endpoint}: {error}")))?;
        if !status.is_success() {
            return Err(remote_status_error("GitHub", &endpoint, status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|source| AppError::Json { endpoint, source })
    }
}

#[derive(Debug, Deserialize)]
struct PullAssociation {
    number: u64,
}

pub fn parse_pr_url(input: &str) -> Result<RepositoryRef, AppError> {
    let url = Url::parse(input)
        .map_err(|error| AppError::Input(format!("invalid GitHub PR URL: {error}")))?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return Err(AppError::Input(
            "PR URL must use https://github.com".to_owned(),
        ));
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or_else(|| AppError::Input("PR URL has no path".to_owned()))?
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() != 4 || segments[2] != "pull" {
        return Err(AppError::Input(
            "expected URL form https://github.com/CUBRID/cubrid/pull/<number>".to_owned(),
        ));
    }
    if !segments[0].eq_ignore_ascii_case(EXPECTED_OWNER)
        || !segments[1].eq_ignore_ascii_case(EXPECTED_REPOSITORY)
    {
        return Err(AppError::Input(format!(
            "expected a {EXPECTED_OWNER}/{EXPECTED_REPOSITORY} pull request"
        )));
    }
    let pr_number = segments[3].parse::<u64>().map_err(|_| {
        AppError::Input(format!(
            "invalid pull-request number '{}': expected a positive integer",
            segments[3]
        ))
    })?;
    if pr_number == 0 {
        return Err(AppError::Input(
            "pull-request number must be positive".to_owned(),
        ));
    }
    Ok(RepositoryRef {
        owner: EXPECTED_OWNER.to_owned(),
        name: EXPECTED_REPOSITORY.to_owned(),
        pr_number,
    })
}

pub fn parse_circleci_job_number(target_url: &str) -> Option<u64> {
    let url = Url::parse(target_url).ok()?;
    let segments: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let candidate = if segments.last() == Some(&"tests") {
        segments.get(segments.len().checked_sub(2)?)?
    } else {
        segments.last()?
    };
    candidate.parse().ok()
}

pub fn extract_ticket(pull: &PullRequest) -> Option<String> {
    let pattern = Regex::new(r"(?i)CBRD-[0-9]+").expect("valid ticket regex");
    [
        &pull.title,
        pull.body.as_deref().unwrap_or(""),
        &pull.head.ref_name,
    ]
    .into_iter()
    .find_map(|text| pattern.find(text).map(|m| m.as_str().to_ascii_uppercase()))
}

pub fn directory_identity(pull: &PullRequest) -> String {
    extract_ticket(pull).unwrap_or_else(|| format!("PR-{}", pull.number))
}

fn validate_full_sha(sha: &str) -> Result<(), AppError> {
    if Regex::new(r"(?i)^[0-9a-f]{40}$")
        .expect("valid sha regex")
        .is_match(sha)
    {
        Ok(())
    } else {
        Err(AppError::Integrity(format!(
            "GitHub resolved an invalid full commit SHA '{sha}'"
        )))
    }
}

fn remote_status_error(service: &str, endpoint: &str, status: StatusCode, body: &[u8]) -> AppError {
    let detail = String::from_utf8_lossy(&body[..body.len().min(1_000)]);
    AppError::Remote(format!(
        "{service} GET {endpoint} returned {status}: {}",
        detail.trim()
    ))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn parses_cubrid_pr_url() {
        let parsed = parse_pr_url("https://github.com/CUBRID/cubrid/pull/6864/").unwrap();
        assert_eq!(parsed.slug(), "CUBRID/cubrid");
        assert_eq!(parsed.pr_number, 6864);
    }

    #[test]
    fn rejects_other_repositories_and_shapes() {
        assert!(parse_pr_url("https://github.com/example/cubrid/pull/1").is_err());
        assert!(parse_pr_url("https://github.com/CUBRID/cubrid/issues/1").is_err());
    }

    #[test]
    fn parses_old_and_app_circleci_urls() {
        assert_eq!(
            parse_circleci_job_number("https://circleci.com/gh/CUBRID/cubrid/139543"),
            Some(139543)
        );
        assert_eq!(
            parse_circleci_job_number(
                "https://app.circleci.com/pipelines/github/CUBRID/cubrid/1/workflows/x/jobs/139543/tests"
            ),
            Some(139543)
        );
    }

    #[test]
    fn selects_latest_status() {
        let make = |state: &str, minute| CommitStatus {
            context: "ci/circleci: test_sql".to_owned(),
            state: state.to_owned(),
            target_url: Some(format!("https://circleci.com/gh/CUBRID/cubrid/{minute}")),
            description: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, minute, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, minute, 0).unwrap(),
        };
        let selected = GitHubClient::select_suite_status(
            &[make("success", 1), make("pending", 2)],
            Suite::Sql,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.state, "pending");
    }

    #[test]
    fn extracts_ticket_in_priority_order() {
        let pull = PullRequest {
            number: 1,
            title: "[cbrd-123] title".to_owned(),
            body: Some("CBRD-456".to_owned()),
            html_url: String::new(),
            state: "open".to_owned(),
            head: crate::model::PullRef {
                ref_name: "CBRD-789".to_owned(),
                sha: "a".repeat(40),
            },
            base: crate::model::PullRef {
                ref_name: "develop".to_owned(),
                sha: "b".repeat(40),
            },
        };
        assert_eq!(extract_ticket(&pull).as_deref(), Some("CBRD-123"));
    }

    #[test]
    fn falls_back_to_pr_number_without_ticket() {
        let pull = PullRequest {
            number: 6864,
            title: "tracking PR".to_owned(),
            body: None,
            html_url: String::new(),
            state: "open".to_owned(),
            head: crate::model::PullRef {
                ref_name: "feature".to_owned(),
                sha: "a".repeat(40),
            },
            base: crate::model::PullRef {
                ref_name: "develop".to_owned(),
                sha: "b".repeat(40),
            },
        };
        assert_eq!(directory_identity(&pull), "PR-6864");
    }
}
