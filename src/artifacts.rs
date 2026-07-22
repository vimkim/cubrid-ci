use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use futures_util::TryStreamExt;
use futures_util::stream::{self, StreamExt};
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::cli::ArtifactMode;
use crate::error::AppError;
use crate::http::send_with_retry;
use crate::model::{ArtifactRecord, CircleArtifact, CircleJob, TestCase};
use crate::storage::{safe_relative_path, write_json_atomic, write_string_atomic};

#[derive(Debug, Clone)]
pub struct ArtifactDownloader {
    client: Client,
    mode: ArtifactMode,
    max_bytes: u64,
    concurrency: usize,
    github_token: Option<String>,
}

impl ArtifactDownloader {
    pub fn new(
        client: Client,
        mode: ArtifactMode,
        max_bytes: u64,
        concurrency: usize,
        github_token: Option<String>,
    ) -> Self {
        Self {
            client,
            mode,
            max_bytes,
            concurrency,
            github_token,
        }
    }

    pub async fn download_artifacts(
        &self,
        artifacts: &[CircleArtifact],
        suite_root: &Path,
    ) -> Vec<ArtifactRecord> {
        let root = suite_root.to_owned();
        let records: Vec<_> = stream::iter(artifacts.iter().cloned().map(|artifact| {
            let downloader = self.clone();
            let root = root.clone();
            async move { downloader.download_artifact(artifact, &root).await }
        }))
        .buffer_unordered(self.concurrency)
        .collect()
        .await;

        let mut records = records;
        records.sort_by(|a, b| {
            a.node_index
                .cmp(&b.node_index)
                .then_with(|| a.path.cmp(&b.path))
        });
        records
    }

    pub async fn download_failed_action_logs(
        &self,
        job: &CircleJob,
        suite_root: &Path,
    ) -> Result<Vec<ActionLogRecord>, AppError> {
        let mut candidates = Vec::new();
        for (step_index, step) in job.steps.iter().enumerate() {
            for action in &step.actions {
                let failed = action.failed == Some(true)
                    || action
                        .status
                        .as_deref()
                        .is_some_and(|status| status.eq_ignore_ascii_case("failed"));
                if failed && let Some(url) = &action.output_url {
                    candidates.push((step_index, step.name.clone(), action.index, url.clone()));
                }
            }
        }

        let client = self.client.clone();
        let max_bytes = self.max_bytes;
        let logs_root = suite_root.join("logs/steps");
        let results: Vec<_> = stream::iter(candidates.into_iter().map(
            |(step_index, step_name, node_index, url)| {
                let client = client.clone();
                let logs_root = logs_root.clone();
                async move {
                    let slug = safe_component(&step_name);
                    let node = node_index
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_owned());
                    let relative =
                        PathBuf::from(format!("logs/steps/{step_index:03}-{slug}/node-{node}.raw"));
                    let destination = logs_root
                        .join(format!("{step_index:03}-{slug}"))
                        .join(format!("node-{node}.raw"));
                    match download_url(&client, &url, &destination, max_bytes, None).await {
                        Ok(downloaded) => {
                            let text_path = destination.with_extension("log");
                            if let Ok(bytes) = tokio::fs::read(&destination).await {
                                let text = decode_circle_output(&bytes);
                                if !text.is_empty() {
                                    let _ = write_string_atomic(&text_path, &text);
                                }
                            }
                            ActionLogRecord {
                                step_index,
                                step_name,
                                node_index,
                                downloaded: true,
                                local_path: Some(relative),
                                size_bytes: Some(downloaded.size_bytes),
                                sha256: Some(downloaded.sha256),
                                diagnostic: None,
                            }
                        }
                        Err(error) => ActionLogRecord {
                            step_index,
                            step_name,
                            node_index,
                            downloaded: false,
                            local_path: None,
                            size_bytes: None,
                            sha256: None,
                            diagnostic: Some(error.to_string()),
                        },
                    }
                }
            },
        ))
        .buffer_unordered(self.concurrency)
        .collect()
        .await;
        let mut results = results;
        results.sort_by_key(|record| (record.step_index, record.node_index));
        write_json_atomic(&suite_root.join("logs/index.json"), &results)?;
        Ok(results)
    }

    pub async fn download_test_sources(
        &self,
        failures: &[TestCase],
        suite_root: &Path,
    ) -> Result<Vec<SourceRecord>, AppError> {
        let pattern = regex::Regex::new(
            r"https://github\.com/([^/\s]+)/([^/\s]+)/blob/([0-9a-fA-F]{40})/([^\s]+)",
        )
        .expect("valid GitHub blob URL regex");
        let mut links = BTreeSet::new();
        for message in failures.iter().filter_map(|test| test.message.as_deref()) {
            for capture in pattern.captures_iter(message) {
                let Some(whole) = capture.get(0) else {
                    continue;
                };
                let owner = capture[1].to_owned();
                let repo = capture[2].to_owned();
                let revision = capture[3].to_ascii_lowercase();
                let path = capture[4]
                    .trim_end_matches(|character: char| {
                        matches!(character, ')' | ']' | '}' | ',' | ';')
                    })
                    .to_owned();
                links.insert((whole.as_str().to_owned(), owner, repo, revision, path));
            }
        }

        let client = self.client.clone();
        let max_bytes = self.max_bytes;
        let github_token = self.github_token.clone();
        let root = suite_root.to_owned();
        let results: Vec<_> = stream::iter(links.into_iter().map(
            |(source_url, owner, repo, revision, path)| {
                let client = client.clone();
                let root = root.clone();
                let github_token = github_token.clone();
                async move {
                    let raw_url = format!(
                        "https://raw.githubusercontent.com/{owner}/{repo}/{revision}/{path}"
                    );
                    let relative = PathBuf::from("sources")
                        .join(format!(
                            "{}-{}",
                            safe_component(&owner),
                            safe_component(&repo)
                        ))
                        .join(&revision)
                        .join(safe_relative_path(&path));
                    let destination = root.join(&relative);
                    match download_url(
                        &client,
                        &raw_url,
                        &destination,
                        max_bytes,
                        github_token.as_deref(),
                    )
                    .await
                    {
                        Ok(downloaded) => SourceRecord {
                            source_url,
                            raw_url,
                            revision,
                            path,
                            downloaded: true,
                            local_path: Some(relative),
                            size_bytes: Some(downloaded.size_bytes),
                            sha256: Some(downloaded.sha256),
                            diagnostic: None,
                        },
                        Err(error) => SourceRecord {
                            source_url,
                            raw_url,
                            revision,
                            path,
                            downloaded: false,
                            local_path: None,
                            size_bytes: None,
                            sha256: None,
                            diagnostic: Some(error.to_string()),
                        },
                    }
                }
            },
        ))
        .buffer_unordered(self.concurrency)
        .collect()
        .await;
        let mut results = results;
        results.sort_by(|a, b| a.source_url.cmp(&b.source_url));
        write_json_atomic(&suite_root.join("sources/index.json"), &results)?;
        Ok(results)
    }

    async fn download_artifact(
        &self,
        artifact: CircleArtifact,
        suite_root: &Path,
    ) -> ArtifactRecord {
        let mut record = ArtifactRecord {
            path: artifact.path.clone(),
            url: artifact.url.clone(),
            node_index: artifact.node_index,
            downloaded: false,
            local_path: None,
            size_bytes: None,
            sha256: None,
            diagnostic: None,
        };
        if !self.should_download(&artifact.path) {
            return record;
        }

        let node = artifact
            .node_index
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let relative = PathBuf::from("artifacts")
            .join(format!("node-{node}"))
            .join(safe_relative_path(&artifact.path));
        let destination = suite_root.join(&relative);
        match download_url(
            &self.client,
            &artifact.url,
            &destination,
            self.max_bytes,
            None,
        )
        .await
        {
            Ok(downloaded) => {
                record.downloaded = true;
                record.local_path = Some(relative);
                record.size_bytes = Some(downloaded.size_bytes);
                record.sha256 = Some(downloaded.sha256);
            }
            Err(error) => record.diagnostic = Some(error.to_string()),
        }
        record
    }

    fn should_download(&self, path: &str) -> bool {
        match self.mode {
            ArtifactMode::Manifest => false,
            ArtifactMode::All => true,
            ArtifactMode::Text => is_text_artifact(path),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionLogRecord {
    pub step_index: usize,
    pub step_name: String,
    pub node_index: Option<u32>,
    pub downloaded: bool,
    pub local_path: Option<PathBuf>,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRecord {
    pub source_url: String,
    pub raw_url: String,
    pub revision: String,
    pub path: String,
    pub downloaded: bool,
    pub local_path: Option<PathBuf>,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub diagnostic: Option<String>,
}

struct DownloadedFile {
    size_bytes: u64,
    sha256: String,
}

async fn download_url(
    client: &Client,
    url: &str,
    destination: &Path,
    max_bytes: u64,
    bearer_token: Option<&str>,
) -> Result<DownloadedFile, AppError> {
    let mut request = client.get(url);
    if let Some(token) = bearer_token {
        request = request.bearer_auth(token);
    }
    let display_url = redact_url(url);
    let response = send_with_retry(request, "artifact", &display_url).await?;
    if !response.status().is_success() {
        return Err(AppError::Remote(format!(
            "GET {display_url} returned {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(AppError::Remote(format!(
            "download exceeds configured limit of {max_bytes} bytes: {display_url}"
        )));
    }

    let parent = destination.parent().ok_or_else(|| {
        AppError::Input(format!(
            "download path has no parent: {}",
            destination.display()
        ))
    })?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| AppError::storage(parent, source))?;
    let temporary = destination.with_extension(format!(
        "{}.part",
        destination
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
    ));
    let mut file = tokio::fs::File::create(&temporary)
        .await
        .map_err(|source| AppError::storage(&temporary, source))?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        let next = match stream.try_next().await {
            Ok(next) => next,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(AppError::Remote(format!("read {display_url}: {error}")));
            }
        };
        let Some(chunk) = next else {
            break;
        };
        size += chunk.len() as u64;
        if size > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(AppError::Remote(format!(
                "download exceeded configured limit of {max_bytes} bytes: {display_url}"
            )));
        }
        hasher.update(&chunk);
        if let Err(source) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(AppError::storage(&temporary, source));
        }
    }
    if let Err(source) = file.sync_all().await {
        drop(file);
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(AppError::storage(&temporary, source));
    }
    drop(file);
    if let Err(source) = tokio::fs::rename(&temporary, destination).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(AppError::storage(destination, source));
    }
    Ok(DownloadedFile {
        size_bytes: size,
        sha256: hex::encode(hasher.finalize()),
    })
}

fn redact_url(input: &str) -> String {
    match url::Url::parse(input) {
        Ok(mut url) => {
            if url.query().is_some() {
                url.set_query(Some("redacted"));
            }
            url.to_string()
        }
        Err(_) => "<invalid-url>".to_owned(),
    }
}

fn is_text_artifact(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    const EXTENSIONS: &[&str] = &[
        ".txt",
        ".log",
        ".xml",
        ".json",
        ".properties",
        ".data",
        ".list",
        ".out",
        ".err",
        ".sql",
        ".answer",
        ".result",
        ".conf",
        ".ini",
    ];
    EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
        || lower.ends_with("current_task_id")
}

fn safe_component(value: &str) -> String {
    let result: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    result.trim_matches('_').to_owned()
}

fn decode_circle_output(bytes: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Some(items) = value.as_array()
    {
        let messages: Vec<_> = items
            .iter()
            .filter_map(|item| item.get("message").and_then(|message| message.as_str()))
            .collect();
        if !messages.is_empty() {
            return messages.join("");
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn text_mode_excludes_core_dumps() {
        assert!(is_text_artifact("tmp/logs/test-shell.xml"));
        assert!(is_text_artifact("tmp/logs/test_status.data"));
        assert!(!is_text_artifact("tmp/cub_server.coredump"));
    }

    #[test]
    fn decodes_circle_step_output_messages() {
        let input = br#"[{"message":"one\n"},{"message":"two\n"}]"#;
        assert_eq!(decode_circle_output(input), "one\ntwo\n");
    }

    #[test]
    fn redacts_signed_query_parameters_from_diagnostics() {
        assert_eq!(
            redact_url("https://example.test/log?token=secret&other=value"),
            "https://example.test/log?redacted"
        );
    }

    #[tokio::test]
    async fn streams_text_artifacts_to_safe_node_scoped_paths() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes("useful log"))
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let downloader = ArtifactDownloader::new(Client::new(), ArtifactMode::Text, 1_024, 2, None);
        let records = downloader
            .download_artifacts(
                &[CircleArtifact {
                    path: "../../tmp/logs/test.log".to_owned(),
                    url: format!("{}/artifact", server.uri()),
                    node_index: Some(3),
                    pretty_path: None,
                }],
                root.path(),
            )
            .await;
        assert!(records[0].downloaded);
        let relative = records[0].local_path.as_ref().unwrap();
        assert_eq!(
            relative,
            &PathBuf::from("artifacts/node-3/tmp/logs/test.log")
        );
        assert_eq!(
            tokio::fs::read_to_string(root.path().join(relative))
                .await
                .unwrap(),
            "useful log"
        );
    }

    #[tokio::test]
    async fn enforces_streamed_download_size_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/large"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes("too large"))
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let downloader = ArtifactDownloader::new(Client::new(), ArtifactMode::All, 4, 1, None);
        let records = downloader
            .download_artifacts(
                &[CircleArtifact {
                    path: "core.coredump".to_owned(),
                    url: format!("{}/large", server.uri()),
                    node_index: Some(0),
                    pretty_path: None,
                }],
                root.path(),
            )
            .await;
        assert!(!records[0].downloaded);
        assert!(records[0].diagnostic.as_deref().unwrap().contains("limit"));
    }
}
