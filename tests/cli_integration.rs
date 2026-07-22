use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

fn run_cli(server: &MockServer, data_dir: &Path, extra: &[&str]) -> std::process::Output {
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
        .arg(data_dir)
        .arg("--artifact-mode")
        .arg("manifest")
        .args(extra);
    command.output().unwrap()
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
