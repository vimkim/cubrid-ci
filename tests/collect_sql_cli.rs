use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "1111111111111111111111111111111111111111";

#[tokio::test(flavor = "multi_thread")]
async fn terminal_red_sql_suite_is_a_successful_evidence_collection() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    validate_schema(
        include_str!("../schema/command-result-v2.schema.json"),
        &output.stdout,
    );
    assert_eq!(result["schema_version"], 2);
    assert_eq!(result["ok"], true);
    assert_eq!(result["suites"]["test_sql"]["state"], "completed");
    assert_eq!(result["suites"]["test_sql"]["status"]["state"], "FAILURE");
    assert_eq!(result["suites"]["test_sql"]["execution"]["run_id"], 123);
    assert_eq!(result["suites"]["test_sql"]["execution"]["attempt"], 2);

    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    let manifest_bytes = fs::read(output_dir.join("manifest.json")).unwrap();
    validate_schema(
        include_str!("../schema/manifest-v2.schema.json"),
        &manifest_bytes,
    );
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest, result);

    let summary_path = result["suites"]["test_sql"]["summary"].as_str().unwrap();
    let summary_bytes = fs::read(output_dir.join(summary_path)).unwrap();
    validate_schema(
        include_str!("../schema/suite-summary-v2.schema.json"),
        &summary_bytes,
    );
    let summary: Value = serde_json::from_slice(&summary_bytes).unwrap();
    assert_eq!(summary["schema_version"], 2);
    assert_eq!(summary["ci_state"], "failure");
    assert_eq!(summary["counts"]["tests"], 1);
    assert_eq!(summary["counts"]["planned"], 1);
    assert_eq!(summary["counts"]["run"], 1);
    assert_eq!(summary["counts"]["unrun"], 0);
    assert_eq!(summary["counts"]["failures"], 1);
    assert_eq!(summary["verdict"], "fail");
    assert_eq!(summary["shards"][0]["build"]["sha"], SHA);
    assert_eq!(
        summary["shards"][0]["testcases"]["sha"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );

    let suite_dir = output_dir.join(Path::new(summary_path).parent().unwrap());
    let failures = suite_dir.join("failures");
    let failure_dir = fs::read_dir(&failures)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let metadata_bytes = fs::read(failure_dir.join("metadata.json")).unwrap();
    validate_schema(
        include_str!("../schema/failure-v2.schema.json"),
        &metadata_bytes,
    );
    let metadata: Value = serde_json::from_slice(&metadata_bytes).unwrap();
    assert_eq!(metadata["name"], "sql/bugs/cases/bug_123.sql");
    assert_eq!(metadata["shard"], "00");
    assert_eq!(metadata["result"], "failure");
    assert_eq!(metadata["diff_extraction"], "extracted");
    let message = fs::read_to_string(failure_dir.join("message.txt")).unwrap();
    assert!(message.contains("unexpected rows"));
    assert!(message.contains("[Diff]"));
    let diff = fs::read_to_string(failure_dir.join("diff.txt")).unwrap();
    assert_eq!(diff, "[Diff]\n--- answer\n+++ actual\n-old\n+new\n");

    let raw = suite_dir.join("raw");
    assert_eq!(
        fs::read_to_string(raw.join("collect/failed.list")).unwrap(),
        "00\tsql/bugs/cases/bug_123.sql\n"
    );
    assert!(
        fs::read_to_string(raw.join("shards/00/results.xml"))
            .unwrap()
            .contains("<failure>")
    );
    let raw_index_bytes = fs::read(raw.join("index.json")).unwrap();
    validate_schema(
        include_str!("../schema/raw-evidence-index-v2.schema.json"),
        &raw_index_bytes,
    );
    let raw_index: Value = serde_json::from_slice(&raw_index_bytes).unwrap();
    assert_eq!(raw_index["schema_version"], 2);
    assert!(
        raw_index["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "github/collect.log")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn contradictory_workflow_counts_are_an_integrity_failure() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:ok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["suites"]["test_sql"]["state"], "collection_failed");
    assert_eq!(result["errors"][0]["kind"], "integrity");
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/untrusted.json")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mismatched_shard_build_is_retained_as_untrusted_integrity_evidence() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        "2222222222222222222222222222222222222222",
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    let untrusted: Value = serde_json::from_slice(
        &fs::read(
            output_dir.join("providers/github-actions/runs/123/attempts/2/test_sql/untrusted.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(untrusted["trusted"], false);
    assert_eq!(untrusted["kind"], "integrity");
}

#[tokio::test(flavor = "multi_thread")]
async fn shard_assignment_mismatch_is_an_integrity_failure() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        2,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("assigned")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn swapped_testcase_identity_is_an_integrity_failure() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/other.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("testcase identities")
    );
}

fn write_ci_commands(commands: &Path, server: &MockServer) {
    write_command(
        commands,
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot()).unwrap()
        ),
    );
    write_command(
        commands,
        "gh",
        &format!(
            r#"case "$*" in
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":900,"name":"collect","run_attempt":2}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            server.uri()
        ),
    );
}

fn command(commands: &Path, data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
    command
        .env("PATH", commands)
        .env("CUBRID_CI_CONFIG", missing_config_path())
        .env_remove("CUBRID_CI_DATA_DIR")
        .env_remove("CUBRID_CI_ARTIFACT_BASE")
        .args([
            "collect",
            "7990",
            "--commit",
            SHA,
            "--suite",
            "test_sql",
            "--data-dir",
        ])
        .arg(data)
        .arg("--json");
    command
}

fn status_snapshot() -> Value {
    json!({
        "schema_version": 1,
        "repository": "CUBRID/cubrid",
        "fetched_at": "2026-09-21T18:19:03Z",
        "complete": true,
        "pr": {
            "number": 7990,
            "title": "[CBRD-26357] Add out-of-row overflow storage",
            "url": "https://github.com/CUBRID/cubrid/pull/7990",
            "head_sha": SHA,
            "state": "OPEN"
        },
        "checks": [{
            "name": "gha-ci: test_sql",
            "provider": "GitHub Actions",
            "expected": true,
            "freshness": "current",
            "current": {
                "state": "FAILURE",
                "reported_for_sha": SHA,
                "detail_url": "https://github.com/CUBRID/cubrid/actions/runs/123",
                "reported_at": "2026-09-21T18:10:04Z"
            },
            "previous": null
        }],
        "history": { "status": "disabled", "limit": 0, "searched": 0 },
        "errors": []
    })
}

async fn mount_evidence(server: &MockServer, summary_info: &str, build_sha: &str, assigned: u64) {
    let build_read =
        format!("sha={build_sha}\nmode=debug\nns=develop\nrun_id=123\nrun_attempt=2\n");
    let shard_done = format!("suite=sql\nidx=00\nassigned={assigned}\nctp_rc=0\n");
    for (url_path, body) in [
        (
            "/runs/123/sql/plan/split.meta",
            concat!(
                "total=1\nunits=1\npar=1\ntimed=0\n",
                "tc_sha='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n",
                "tc_branch='develop'\nrerun_unrun=0\nrerun_short=0\nrerun_gone=0\n"
            ),
        ),
        (
            "/runs/123/sql/plan/shards/",
            "<a href=\"00.list\">00.list</a>",
        ),
        ("/runs/123/sql/plan/plan.tsv", "00\t1\t1\t1.0\n"),
        ("/runs/123/sql/shard/", "<a href=\"00/\">00/</a>"),
        (
            "/runs/123/sql/shard/00/test-results/",
            "<a href=\"results.xml\">results.xml</a>",
        ),
        (
            "/runs/123/sql/collect/failed.list",
            "00\tsql/bugs/cases/bug_123.sql\n",
        ),
        ("/runs/123/sql/collect/verdict", "fail\n"),
        ("/runs/123/sql/shard/00/build.read", build_read.as_str()),
        (
            "/runs/123/sql/shard/00/tc.read",
            concat!(
                "tc_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
                "tc_branch=develop\n"
            ),
        ),
        ("/runs/123/sql/shard/00/summary_info", summary_info),
        ("/runs/123/sql/shard/00/shard.done", shard_done.as_str()),
        (
            "/runs/123/sql/shard/00/test-results/results.xml",
            concat!(
                "<testsuite tests=\"1\" failures=\"1\">",
                "<testcase classname=\"sql.bugs\" name=\"sql/bugs/cases/bug_123.sql\" time=\"1.2\">",
                "<failure><![CDATA[unexpected rows\n[Diff]\n--- answer\n+++ actual\n-old\n+new]]></failure>",
                "</testcase></testsuite>"
            ),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(url_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }
}

fn missing_config_path() -> PathBuf {
    std::env::temp_dir().join("cubrid-ci-sql-no-config.toml")
}

fn write_command(directory: &Path, name: &str, behavior: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{behavior}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn validate_schema(schema: &str, instance: &[u8]) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let instance: Value = serde_json::from_slice(instance).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    if let Err(error) = validator.validate(&instance) {
        panic!("schema validation failed: {error}");
    }
}
