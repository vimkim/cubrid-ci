use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn version_reports_package_and_build_commit() {
    let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "cubrid-ci {}\n",
            cubrid_circleci_analyzer::build_info::VERSION
        )
    );
    assert!(cubrid_circleci_analyzer::build_info::VERSION.ends_with(", debug)"));
}

#[test]
fn json_mode_reports_configuration_errors_as_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
        .arg("--json")
        .arg("test-sql")
        .arg("https://github.com/CUBRID/cubrid/pull/6864")
        .arg("--download-concurrency")
        .arg("0")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["kind"], "input");
    assert_eq!(error["exit_code"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_suite_is_collected_as_successful_evidence() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    mount_statuses(
        &server,
        vec![
            status("build", "success", 40),
            status("build_debug", "success", 41),
            status("test_sql", "failure", 42),
        ],
    )
    .await;
    mount_job(&server, SHA, "test_sql", 42).await;
    Mock::given(method("GET"))
        .and(path("/project/github/CUBRID/cubrid/42/tests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tests": [
                {"name": "sql/a.sql", "file": "sql/a.sql", "result": "success", "run_time": 0.5},
                {
                    "name": "sql/b.sql",
                    "file": "sql/b.sql",
                    "result": "failure",
                    "run_time": 1.5,
                    "message": "unexpected result\n[Diff]\n--- answer\n+++ actual\n-old\n+new"
                }
            ]
        })))
        .mount(&server)
        .await;
    mount_artifacts(&server, 42).await;

    let output_root = tempfile::tempdir().unwrap();
    let output = run_cli(&server, output_root.path(), &[]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["failure_count"], 1);
    assert_eq!(result["ci_status"], "failure");

    let suite = output_root.path().join("CBRD-26357/aaaaaaa/test_sql");
    assert_eq!(
        std::fs::read_to_string(suite.join("failed-tc.txt")).unwrap(),
        "sql/b.sql\n"
    );
    assert!(suite.join("summary.json").exists());
    assert!(suite.join("attempts/42/raw/tests.json").exists());
    let failure_dir = std::fs::read_dir(suite.join("failures"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        std::fs::read_to_string(failure_dir.join("diff.txt"))
            .unwrap()
            .contains("--- answer")
    );
    validate_schema(
        include_str!("../schema/suite-summary-v1.schema.json"),
        &std::fs::read(suite.join("summary.json")).unwrap(),
    );
    validate_schema(
        include_str!("../schema/manifest-v1.schema.json"),
        &std::fs::read(output_root.path().join("CBRD-26357/aaaaaaa/manifest.json")).unwrap(),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn default_mode_keeps_failed_tests_and_action_logs_without_artifact_payloads() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    mount_statuses(
        &server,
        vec![
            status("build", "success", 40),
            status("build_debug", "success", 41),
            status("test_sql", "failure", 42),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/project/github/CUBRID/cubrid/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "build_num": 42,
            "status": "failed",
            "vcs_revision": SHA,
            "queued_at": "2026-07-22T00:00:00Z",
            "start_time": "2026-07-22T00:00:02Z",
            "stop_time": "2026-07-22T00:00:12Z",
            "build_time_millis": 10000,
            "parallel": 1,
            "workflows": {
                "job_name": "test_sql",
                "workflow_id": "wf",
                "workflow_name": "build_test"
            },
            "steps": [{
                "name": "Test",
                "actions": [{
                    "index": 0,
                    "status": "failed",
                    "failed": true,
                    "output_url": format!("{}/step-output", server.uri())
                }]
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/project/github/CUBRID/cubrid/42/tests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tests": [{
                "name": "sql/b.sql",
                "file": "sql/b.sql",
                "result": "failure",
                "message": "unexpected result"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/project/github/CUBRID/cubrid/42/artifacts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "path": "tmp/logs/test.log",
            "url": format!("{}/artifact-payload", server.uri()),
            "node_index": 0
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/step-output"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"message": "the assertion failed\n"}])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/artifact-payload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("large artifact"))
        .mount(&server)
        .await;

    let output_root = tempfile::tempdir().unwrap();
    let output = run_cli(&server, output_root.path(), &[]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["failure_count"], 1);
    let stderr = String::from_utf8(output.stderr).unwrap();
    for status_line in [
        "cubrid-ci: resolving CUBRID/cubrid#6864 for test_sql",
        "cubrid-ci: pinned commit aaaaaaa",
        "cubrid-ci: checking GitHub status for test_sql",
        "cubrid-ci: collecting CircleCI test_sql job 42",
        "cubrid-ci: fetching job metadata, tests, and artifact manifest",
        "cubrid-ci: tests: total=1, failed=1; artifacts: listed=1",
        "cubrid-ci: downloading failed action logs",
        "cubrid-ci: failed action logs: captured=1, unavailable=0",
        "cubrid-ci: artifact payloads: skipped (manifest mode, listed=1)",
        "cubrid-ci: publishing evidence",
        "cubrid-ci: published evidence:",
    ] {
        assert!(
            stderr.contains(status_line),
            "missing status line {status_line:?} in stderr:\n{stderr}"
        );
    }

    let suite = output_root.path().join("CBRD-26357/aaaaaaa/test_sql");
    assert_eq!(
        std::fs::read_to_string(suite.join("failed-tc.txt")).unwrap(),
        "sql/b.sql\n"
    );
    assert_eq!(
        std::fs::read_to_string(suite.join("logs/steps/000-Test/node-0.log")).unwrap(),
        "the assertion failed\n"
    );
    let artifacts: Value =
        serde_json::from_slice(&std::fs::read(suite.join("artifacts.json")).unwrap()).unwrap();
    assert_eq!(artifacts[0]["downloaded"], false);
    assert!(artifacts[0].get("local_path").is_none());

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.url.path() == "/step-output")
    );
    assert!(
        requests
            .iter()
            .all(|request| request.url.path() != "/artifact-payload")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_suite_exits_unavailable_and_leaves_empty_directory() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    mount_statuses(
        &server,
        vec![
            status("build", "success", 40),
            status("build_debug", "pending", 41),
            status("test_sql", "pending", 42),
        ],
    )
    .await;
    let output_root = tempfile::tempdir().unwrap();
    let output = run_cli(&server, output_root.path(), &[]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        std::fs::read_dir(output_root.path().join("CBRD-26357/aaaaaaa/test_sql"))
            .unwrap()
            .count(),
        0
    );
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(output_root.path().join("CBRD-26357/aaaaaaa/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["suites"]["test_sql"]["state"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn mismatched_circleci_revision_is_rejected_without_suite_output() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    mount_statuses(
        &server,
        vec![
            status("build", "success", 40),
            status("build_debug", "success", 41),
            status("test_sql", "failure", 42),
        ],
    )
    .await;
    mount_job(
        &server,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "test_sql",
        42,
    )
    .await;

    let output_root = tempfile::tempdir().unwrap();
    let output = run_cli(&server, output_root.path(), &[]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        std::fs::read_dir(output_root.path().join("CBRD-26357/aaaaaaa/test_sql"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_polls_pending_status_until_terminal_result() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path(format!("/repos/CUBRID/cubrid/commits/{SHA}/statuses")))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .respond_with(SequencedStatuses {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    mount_job(&server, SHA, "test_sql", 42).await;
    Mock::given(method("GET"))
        .and(path("/project/github/CUBRID/cubrid/42/tests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tests": []})))
        .mount(&server)
        .await;
    mount_artifacts(&server, 42).await;

    let output_root = tempfile::tempdir().unwrap();
    let output = run_cli(
        &server,
        output_root.path(),
        &["--wait", "--poll-interval", "10ms", "--timeout", "1s"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_build_is_reported_without_publishing_suite_evidence() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    mount_statuses(
        &server,
        vec![
            status("build", "failure", 40),
            status("build_debug", "success", 41),
        ],
    )
    .await;
    let output_root = tempfile::tempdir().unwrap();
    let output = run_cli(&server, output_root.path(), &[]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        std::fs::read_dir(output_root.path().join("CBRD-26357/aaaaaaa/test_sql"))
            .unwrap()
            .count(),
        0
    );
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(output_root.path().join("CBRD-26357/aaaaaaa/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["suites"]["test_sql"]["state"], "build_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn abbreviated_commit_not_associated_with_pr_is_rejected() {
    let server = MockServer::start().await;
    mount_pull(&server).await;
    let unrelated = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    Mock::given(method("GET"))
        .and(path("/repos/CUBRID/cubrid/commits/bbbbbbb"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": unrelated})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/CUBRID/cubrid/commits/{unrelated}/pulls"
        )))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/CUBRID/cubrid/pulls/6864/commits"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let output_root = tempfile::tempdir().unwrap();
    let mut command = base_command(&server, output_root.path());
    let output = command
        .arg("bbbbbbb")
        .arg("--artifact-mode")
        .arg("manifest")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!output_root.path().join("CBRD-26357").exists());
}

struct SequencedStatuses {
    calls: Arc<AtomicUsize>,
}

impl Respond for SequencedStatuses {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let state = if call == 0 { "pending" } else { "failure" };
        ResponseTemplate::new(200).set_body_json(vec![
            status("build", "success", 40),
            status("build_debug", "success", 41),
            status("test_sql", state, 42),
        ])
    }
}

fn run_cli(server: &MockServer, data_dir: &Path, extra: &[&str]) -> std::process::Output {
    let mut command = base_command(server, data_dir);
    command.args(extra);
    command.output().unwrap()
}

fn base_command(server: &MockServer, data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
    command
        .arg("--json")
        .arg("--github-api")
        .arg(server.uri())
        .arg("--circleci-api")
        .arg(server.uri())
        .arg("test-sql")
        .arg("https://github.com/CUBRID/cubrid/pull/6864")
        .arg("--data-dir")
        .arg(data_dir);
    command
}

async fn mount_pull(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/CUBRID/cubrid/pulls/6864"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 6864,
            "title": "[CBRD-26357] tracking PR",
            "body": null,
            "html_url": "https://github.com/CUBRID/cubrid/pull/6864",
            "state": "open",
            "head": {"ref": "feat/oos", "sha": SHA},
            "base": {"ref": "develop", "sha": "dddddddddddddddddddddddddddddddddddddddd"}
        })))
        .mount(server)
        .await;
}

async fn mount_statuses(server: &MockServer, statuses: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/CUBRID/cubrid/commits/{SHA}/statuses")))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(statuses))
        .mount(server)
        .await;
}

fn status(job: &str, state: &str, build_number: u64) -> Value {
    json!({
        "context": format!("ci/circleci: {job}"),
        "state": state,
        "target_url": format!("https://circleci.com/gh/CUBRID/cubrid/{build_number}"),
        "description": null,
        "created_at": "2026-07-22T00:00:00Z",
        "updated_at": "2026-07-22T00:00:01Z"
    })
}

async fn mount_job(server: &MockServer, revision: &str, job_name: &str, build_number: u64) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/project/github/CUBRID/cubrid/{build_number}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "build_num": build_number,
            "status": "failed",
            "vcs_revision": revision,
            "queued_at": "2026-07-22T00:00:00Z",
            "start_time": "2026-07-22T00:00:02Z",
            "stop_time": "2026-07-22T00:00:12Z",
            "build_time_millis": 10000,
            "parallel": 10,
            "workflows": {"job_name": job_name, "workflow_id": "wf", "workflow_name": "build_test"},
            "steps": []
        })))
        .mount(server)
        .await;
}

async fn mount_artifacts(server: &MockServer, build_number: u64) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/project/github/CUBRID/cubrid/{build_number}/artifacts"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
}

fn validate_schema(schema: &str, instance: &[u8]) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let instance: Value = serde_json::from_slice(instance).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    if let Err(error) = validator.validate(&instance) {
        panic!("schema validation failed: {error}");
    }
}
