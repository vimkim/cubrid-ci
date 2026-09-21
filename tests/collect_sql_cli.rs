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

#[tokio::test(flavor = "multi_thread")]
async fn wait_refreshes_status_then_pins_the_terminal_execution() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    let counter = commands.path().join("status-count");
    write_status_sequence(
        commands.path(),
        &counter,
        &status_snapshot_with("PENDING", SHA, 123),
        &status_snapshot_with("FAILURE", SHA, 123),
    );
    write_gh_command(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args(["--wait", "--timeout", "1s", "--poll-interval", "1ms"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(counter)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
            >= 3
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["execution"]["run_id"], 123);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_same_commit_run_does_not_change_the_pinned_download() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    let counter = commands.path().join("status-count");
    write_status_sequence(
        commands.path(),
        &counter,
        &status_snapshot_with("FAILURE", SHA, 123),
        &status_snapshot_with("FAILURE", SHA, 999),
    );
    write_gh_command(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["execution"]["run_id"], 123);
}

#[tokio::test(flavor = "multi_thread")]
async fn moved_pr_head_rejects_publication_but_keeps_downloaded_evidence() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    let counter = commands.path().join("status-count");
    write_status_sequence(
        commands.path(),
        &counter,
        &status_snapshot_with("FAILURE", SHA, 123),
        &status_snapshot_with("FAILURE", "2222222222222222222222222222222222222222", 999),
    );
    write_gh_command(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["kind"], "integrity");
    let root = data
        .path()
        .join(format!("github-actions/CUBRID-cubrid/pr-7990/{SHA}"));
    assert!(!root.join("manifest.json").exists());
    assert!(
        root.join("providers/github-actions/runs/123/attempts/2/test_sql/summary.json")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_plan_job_is_retained_as_job_level_failure() {
    let commands = tempfile::tempdir().unwrap();
    write_command(
        commands.path(),
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot()).unwrap()
        ),
    );
    write_command(
        commands.path(),
        "gh",
        r#"case "$*" in
  *actions/runs/123/jobs*) printf '%s\n' '{"jobs":[{"id":901,"name":"plan / test_sql","run_attempt":2,"conclusion":"failure"}]}' ;;
  *actions/jobs/901/logs*) printf '%s\n' 'planner failed before publishing evidence' ;;
  *actions/runs/123*) printf '%s\n' '{"id":123,"run_attempt":2}' ;;
  *) exit 64 ;;
esac"#,
    );
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["state"], "job_level_failure");
    assert_eq!(result["errors"][0]["kind"], "unavailable");
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/raw/github/job-901.log")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unrelated_attempt_and_suite_failures_do_not_reclassify_a_transport_error() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/collect/verdict"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_command(
        commands.path(),
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot()).unwrap()
        ),
    );
    write_command(
        commands.path(),
        "gh",
        &format!(
            r#"case "$*" in
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":901,"name":"plan / test_sql","run_attempt":1,"conclusion":"failure"}},{{"id":902,"name":"shard shell 09","run_attempt":2,"conclusion":"failure"}},{{"id":900,"name":"collect","run_attempt":2,"conclusion":"failure"}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/jobs/901/logs*|*actions/jobs/902/logs*) exit 73 ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            server.uri()
        ),
    );
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["state"], "collection_failed");
    assert_eq!(result["errors"][0]["kind"], "remote");
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        !output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/raw/github/job-901-log-error.txt")
            .exists()
    );
    assert!(
        !output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/raw/github/job-902-log-error.txt")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn collect_job_can_be_discovered_on_a_later_jobs_page() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let commands = tempfile::tempdir().unwrap();
    write_command(
        commands.path(),
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot()).unwrap()
        ),
    );
    write_command(
        commands.path(),
        "gh",
        &format!(
            r#"case "$*" in
  *actions/runs/123/jobs*page=2*) printf '%s\n' '{{"total_count":2,"jobs":[{{"id":900,"name":"collect","run_attempt":2,"conclusion":"success"}}]}}' ;;
  *actions/runs/123/jobs*page=1*) printf '%s\n' '{{"total_count":2,"jobs":[{{"id":899,"name":"plan / test_sql","run_attempt":2,"conclusion":"success"}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            server.uri()
        ),
    );
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        output_dir
            .join(
                "providers/github-actions/runs/123/attempts/2/test_sql/raw/github/jobs-page-2.json"
            )
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_collect_job_with_unavailable_log_is_job_level_failure() {
    let commands = tempfile::tempdir().unwrap();
    write_command(
        commands.path(),
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot()).unwrap()
        ),
    );
    write_command(
        commands.path(),
        "gh",
        r#"case "$*" in
  *actions/runs/123/jobs*) printf '%s\n' '{"jobs":[{"id":900,"name":"collect","run_attempt":2,"conclusion":"cancelled"}]}' ;;
  *actions/jobs/900/logs*) exit 73 ;;
  *actions/runs/123*) printf '%s\n' '{"id":123,"run_attempt":2}' ;;
  *) exit 64 ;;
esac"#,
    );
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["state"], "job_level_failure");
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/raw/github/job-900-log-error.txt")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn optional_binaries_are_streamed_only_from_abnormal_shards_and_inventoried() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    for (url_path, body) in [
        (
            "/runs/123/sql/shard/00/artifacts/",
            "<a href=\"small.bin\">small.bin</a><a href=\"large.bin\">large.bin</a>",
        ),
        ("/runs/123/sql/shard/00/artifacts/small.bin", "abcd"),
        ("/runs/123/sql/shard/00/artifacts/large.bin", "0123456789"),
    ] {
        Mock::given(method("GET"))
            .and(path(url_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args([
            "--include-binaries",
            "--max-binary-bytes",
            "4",
            "--max-binary-total-bytes",
            "100",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let suite_dir = PathBuf::from(result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/123/attempts/2/test_sql");
    assert_eq!(
        fs::read(suite_dir.join("binaries/00/small.bin")).unwrap(),
        b"abcd"
    );
    assert!(!suite_dir.join("binaries/00/large.bin").exists());
    let inventory_bytes = fs::read(suite_dir.join("binary-inventory.json")).unwrap();
    validate_schema(
        include_str!("../schema/binary-inventory-v2.schema.json"),
        &inventory_bytes,
    );
    let inventory: Value = serde_json::from_slice(&inventory_bytes).unwrap();
    let files = inventory["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().any(|file| {
        file["name"] == "small.bin"
            && file["state"] == "downloaded"
            && file["size_bytes"] == 4
            && file["sha256"].as_str().unwrap().len() == 64
    }));
    assert!(files.iter().any(|file| {
        file["name"] == "large.bin"
            && file["state"] == "excluded"
            && file["reason"] == "per_file_limit"
    }));

    fs::remove_file(suite_dir.join("summary.json")).unwrap();
    fs::remove_file(suite_dir.join("binary-inventory.json")).unwrap();
    let replay = command(commands.path(), data.path())
        .args([
            "--include-binaries",
            "--max-binary-bytes",
            "4",
            "--max-binary-total-bytes",
            "100",
        ])
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        fs::read(suite_dir.join("binaries/00/small.bin")).unwrap(),
        b"abcd"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_directory_entry_is_a_malformed_diagnostic() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/plan/shards/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<a href=\"%2e%2e.list\">bad</a>"))
        .with_priority(1)
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["errors"][0]["diagnostic"], "malformed");
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_and_oversized_inputs_have_distinct_diagnostics() {
    for (status, expected_exit, diagnostic) in [(410, 3, "expired"), (200, 5, "oversized")] {
        let server = MockServer::start().await;
        mount_evidence(
            &server,
            "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
            SHA,
            1,
        )
        .await;
        let response = if status == 410 {
            ResponseTemplate::new(410)
        } else {
            ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1024 * 1024 + 1])
        };
        Mock::given(method("GET"))
            .and(path("/runs/123/sql/plan/split.meta"))
            .respond_with(response)
            .with_priority(1)
            .mount(&server)
            .await;
        let commands = tempfile::tempdir().unwrap();
        write_ci_commands(commands.path(), &server);
        let data = tempfile::tempdir().unwrap();

        let output = command(commands.path(), data.path()).output().unwrap();
        assert_eq!(output.status.code(), Some(expected_exit));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["errors"][0]["diagnostic"], diagnostic);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn evidence_redirect_is_not_followed_outside_the_pinned_path() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/plan/split.meta"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/outside", server.uri())),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/outside"))
        .respond_with(ResponseTemplate::new(200).set_body_string("escaped"))
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/outside")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_provenance_has_a_malformed_diagnostic() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/build.read"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "sha={SHA}\nmode=debug\nns=develop\nrun_id=not-a-number\nrun_attempt=2\n"
        )))
        .with_priority(1)
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["errors"][0]["diagnostic"], "malformed");
}

#[tokio::test(flavor = "multi_thread")]
async fn recollection_reuses_immutable_attempt_evidence_and_only_refreshes_manifest() {
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

    let first = command(commands.path(), data.path()).output().unwrap();
    assert!(first.status.success());
    let first_result: Value = serde_json::from_slice(&first.stdout).unwrap();
    let suite_dir = PathBuf::from(first_result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/123/attempts/2/test_sql");
    let first_summary = fs::read(suite_dir.join("summary.json")).unwrap();
    let request_count = server.received_requests().await.unwrap().len();

    let second = command(commands.path(), data.path()).output().unwrap();
    assert!(second.status.success());
    let second_result: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        fs::read(suite_dir.join("summary.json")).unwrap(),
        first_summary
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        request_count
    );
    assert_ne!(first_result["collected_at"], second_result["collected_at"]);
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
    write_gh_command(commands, server);
}

fn write_gh_command(commands: &Path, server: &MockServer) {
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

fn write_status_sequence(commands: &Path, counter: &Path, first: &Value, later: &Value) {
    let behavior = format!(
        "count=0\n\
         if [ -f '{counter}' ]; then count=$(/bin/cat '{counter}'); fi\n\
         count=$((count + 1))\n\
         printf '%s\\n' \"$count\" > '{counter}'\n\
         if [ \"$count\" -eq 1 ]; then\n\
           printf '%s\\n' '{first}'\n\
         else\n\
           printf '%s\\n' '{later}'\n\
         fi",
        counter = counter.display(),
        first = serde_json::to_string(first).unwrap(),
        later = serde_json::to_string(later).unwrap(),
    );
    write_command(commands, "cubrid-pr-status", &behavior);
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
    status_snapshot_with("FAILURE", SHA, 123)
}

fn status_snapshot_with(state: &str, head_sha: &str, run_id: u64) -> Value {
    json!({
        "schema_version": 1,
        "repository": "CUBRID/cubrid",
        "fetched_at": "2026-09-21T18:19:03Z",
        "complete": true,
        "pr": {
            "number": 7990,
            "title": "[CBRD-26357] Add out-of-row overflow storage",
            "url": "https://github.com/CUBRID/cubrid/pull/7990",
            "head_sha": head_sha,
            "state": "OPEN"
        },
        "checks": [{
            "name": "gha-ci: test_sql",
            "provider": "GitHub Actions",
            "expected": true,
            "freshness": "current",
            "current": {
                "state": state,
                "reported_for_sha": head_sha,
                "detail_url": format!("https://github.com/CUBRID/cubrid/actions/runs/{run_id}"),
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
