mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::HttpProxy;

const SHA: &str = "1111111111111111111111111111111111111111";

#[tokio::test(flavor = "multi_thread")]
async fn test_rc_one_with_reconciled_testcase_failure_is_complete_and_reusable() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let marker = "suite=sql\nidx=00\nassigned=1\ntest_rc=1\n";
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/shard.done"))
        .respond_with(ResponseTemplate::new(200).set_body_string(marker))
        .with_priority(1)
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let first = command(commands.path(), data.path()).output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let result: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["state"], "completed");
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let summary = root.join(result["suites"]["test_sql"]["summary"].as_str().unwrap());
    let summary_bytes = fs::read(&summary).unwrap();
    validate_schema(
        include_str!("../schema/suite-summary-v2.schema.json"),
        &summary_bytes,
    );
    let parsed: Value = serde_json::from_slice(&summary_bytes).unwrap();
    assert_eq!(parsed["counts"]["failures"], 1);
    assert_eq!(parsed["verdict"], "fail");
    let raw_marker = summary.parent().unwrap().join("raw/shards/00/shard.done");
    assert_eq!(fs::read_to_string(&raw_marker).unwrap(), marker);
    let requests = server.received_requests().await.unwrap().len();

    let replay = command(commands.path(), data.path()).output().unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stdout)
    );
    assert_eq!(fs::read(&summary).unwrap(), summary_bytes);
    assert_eq!(fs::read_to_string(&raw_marker).unwrap(), marker);
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert_eq!(fs::read_dir(root.join("observations")).unwrap().count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_rc_one_without_testcase_failures_is_job_level_failure() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:ok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/shard.done"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("suite=sql\nidx=00\nassigned=1\ntest_rc=1\n"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_sql"]["state"], "job_level_failure");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("test_rc=1")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_rc_one_does_not_waive_junit_reconciliation() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    for (endpoint, body) in [
        ("shard.done", "suite=sql\nidx=00\nassigned=1\ntest_rc=1\n"),
        (
            "test-results/results.xml",
            "<testsuite><testcase name=\"sql/bugs/cases/bug_123.sql\"/></testsuite>",
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/runs/123/sql/shard/00/{endpoint}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .with_priority(1)
            .mount(&server)
            .await;
    }
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("workflow result and JUnit counts disagree")
    );
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        !root
            .join("providers/github-actions/runs/123/attempts/2/test_sql/summary.json")
            .exists()
    );
}

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
async fn recollection_reuses_complete_raw_responses_without_network_fetches() {
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
    let output_dir = PathBuf::from(first_result["output_dir"].as_str().unwrap());
    let suite_dir = output_dir.join("providers/github-actions/runs/123/attempts/2/test_sql");
    fs::remove_file(suite_dir.join("summary.json")).unwrap();
    let requests_before = server.received_requests().await.unwrap().len();

    let replay = command(commands.path(), data.path()).output().unwrap();

    assert!(
        replay.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests_before
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn validated_warm_suite_records_its_shards_as_retained() {
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

    assert!(
        command(commands.path(), data.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let replay = command(commands.path(), data.path()).output().unwrap();

    assert!(replay.status.success());
    let result: Value = serde_json::from_slice(&replay.stdout).unwrap();
    let mut observations =
        fs::read_dir(PathBuf::from(result["output_dir"].as_str().unwrap()).join("observations"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
    observations.sort_by_key(|entry| entry.file_name());
    let observation_result: Value = serde_json::from_slice(
        &fs::read(observations.last().unwrap().path().join("result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        observation_result["acquisition"]["test_sql"]["shards"]["00"]["state"],
        "retained"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recollection_rejects_raw_evidence_that_disagrees_with_its_index() {
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
    let raw = PathBuf::from(first_result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/123/attempts/2/test_sql/raw");
    fs::write(raw.join("collect/verdict"), "pass\n").unwrap();

    let replay = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(replay.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert_eq!(result["suites"]["test_sql"]["state"], "collection_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn recollection_rejects_a_stored_summary_that_violates_its_schema() {
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
    let summary_path = PathBuf::from(first_result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/123/attempts/2/test_sql/summary.json");
    let mut summary: Value = serde_json::from_slice(&fs::read(&summary_path).unwrap()).unwrap();
    summary["unexpected"] = Value::Bool(true);
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary).unwrap()).unwrap();

    let replay = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(replay.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("suite-summary-v2")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recollection_rejects_schema_valid_summary_tampering() {
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
    let summary_path = PathBuf::from(first_result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/123/attempts/2/test_sql/summary.json");
    let mut summary: Value = serde_json::from_slice(&fs::read(&summary_path).unwrap()).unwrap();
    summary["ci_state"] = json!("success");
    summary["verdict"] = json!("pass");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary).unwrap()).unwrap();

    let replay = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(replay.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("summary digest")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn suite_level_failure_records_known_planned_shards_as_not_attempted() {
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
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let observation =
        fs::read_dir(PathBuf::from(result["output_dir"].as_str().unwrap()).join("observations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let observation_result: Value =
        serde_json::from_slice(&fs::read(observation.join("result.json")).unwrap()).unwrap();
    assert_eq!(
        observation_result["acquisition"]["test_sql"]["shards"]["00"]["state"],
        "not_attempted"
    );
    let acquisition_error = &observation_result["acquisition"]["test_sql"]["errors"][0];
    assert_eq!(acquisition_error["stage"], "verdict");
    assert_eq!(acquisition_error["consumed_bytes"], 0);
    assert!(
        acquisition_error["endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/runs/123/sql/collect/verdict")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_mandatory_response_records_endpoint_stage_and_consumed_bytes() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    let response_path = "/runs/123/sql/shard/00/build.read";
    let proxy = HttpProxy::start_truncated(*server.address(), response_path, b"sha=1".to_vec());
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    assert!(
        !output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/raw/shards/00/build.read")
            .exists()
    );
    let observation = fs::read_dir(output_dir.join("observations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let observation_bytes = fs::read(observation.join("result.json")).unwrap();
    validate_schema(
        include_str!("../schema/collection-observation-result-v1.schema.json"),
        &observation_bytes,
    );
    let observation_result: Value = serde_json::from_slice(&observation_bytes).unwrap();
    let shard = &observation_result["acquisition"]["test_sql"]["shards"]["00"];
    assert_eq!(shard["state"], "failed");
    assert_eq!(shard["stage"], "build_provenance");
    assert_eq!(shard["consumed_bytes"], 5);
    assert!(shard["endpoint"].as_str().unwrap().ends_with(response_path));
    assert_eq!(shard["kind"], "remote");
    assert_eq!(shard["diagnostic"], "remote");
    assert!(shard["message"].as_str().unwrap().contains("read http"));
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
        !output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/untrusted.json")
            .exists()
    );
    assert!(output_dir.join("observations").is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn mismatched_shard_build_does_not_create_an_untrusted_marker() {
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
    assert!(
        !output_dir
            .join("providers/github-actions/runs/123/attempts/2/test_sql/untrusted.json")
            .exists()
    );
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
            >= 2
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
        &status_snapshot_with("FAILURE", SHA, 123),
    );
    write_command(
        commands.path(),
        "gh",
        &format!(
            r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"2222222222222222222222222222222222222222"}}}}' ;;
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":900,"name":"collect","run_attempt":2}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            server.uri()
        ),
    );
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
    let observation = fs::read_dir(root.join("observations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(observation.join("request.json").is_file());
    let observation_result: Value =
        serde_json::from_slice(&fs::read(observation.join("result.json")).unwrap()).unwrap();
    assert_eq!(observation_result["outcome"], "failed");
    assert_eq!(observation_result["error"]["kind"], "integrity");
    assert_eq!(
        observation_result["error"]["diagnostic"],
        "identity_or_integrity"
    );
    assert_eq!(fs::read_to_string(counter).unwrap().trim(), "1");
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
  *pulls/7990*) printf '%s\n' '{"number":7990,"head":{"sha":"1111111111111111111111111111111111111111"}}' ;;
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
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
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
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
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
  *pulls/7990*) printf '%s\n' '{"number":7990,"head":{"sha":"1111111111111111111111111111111111111111"}}' ;;
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
    let first_observation =
        fs::read_dir(PathBuf::from(result["output_dir"].as_str().unwrap()).join("observations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let first_observation_result: Value =
        serde_json::from_slice(&fs::read(first_observation.join("result.json")).unwrap()).unwrap();
    assert_eq!(
        first_observation_result["binary_budget"]["consumed_bytes"],
        4
    );
    assert!(
        first_observation_result["binary_budget"]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| { artifact["name"] == "large.bin" && artifact["consumed_bytes"] == 0 })
    );
    assert!(
        first_observation_result["binary_budget"]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| { artifact["name"] == "small.bin" && artifact["consumed_bytes"] == 4 })
    );

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

    fs::write(suite_dir.join("binaries/00/small.bin"), b"wxyz").unwrap();
    let tampered = command(commands.path(), data.path())
        .args([
            "--include-binaries",
            "--max-binary-bytes",
            "4",
            "--max-binary-total-bytes",
            "100",
        ])
        .output()
        .unwrap();
    assert_eq!(tampered.status.code(), Some(4));
    let tampered: Value = serde_json::from_slice(&tampered.stdout).unwrap();
    assert_eq!(tampered["errors"][0]["kind"], "integrity");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_unknown_length_binary_consumes_the_total_read_budget() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/artifacts/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<a href=\"a-unknown.bin\">a-unknown.bin</a><a href=\"z-small.bin\">z-small.bin</a>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/artifacts/z-small.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .mount(&server)
        .await;
    let proxy = HttpProxy::start(
        *server.address(),
        BTreeMap::from([(
            "/runs/123/sql/shard/00/artifacts/a-unknown.bin".to_owned(),
            b"0123456789".to_vec(),
        )]),
    );
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args([
            "--include-binaries",
            "--max-binary-bytes",
            "100",
            "--max-binary-total-bytes",
            "5",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let suite_dir = PathBuf::from(result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/123/attempts/2/test_sql");
    assert!(!suite_dir.join("binaries/00/a-unknown.bin").exists());
    assert!(
        !suite_dir.join("binaries/00/z-small.bin").exists(),
        "inventory: {}",
        fs::read_to_string(suite_dir.join("binary-inventory.json")).unwrap()
    );
    let inventory: Value =
        serde_json::from_slice(&fs::read(suite_dir.join("binary-inventory.json")).unwrap())
            .unwrap();
    let files = inventory["files"].as_array().unwrap();
    assert!(files.iter().any(|file| {
        file["name"] == "a-unknown.bin"
            && file["state"] == "excluded"
            && file["reason"] == "total_limit"
            && file["size_bytes"].is_null()
    }));
    assert!(files.iter().any(|file| {
        file["name"] == "z-small.bin"
            && file["state"] == "excluded"
            && file["reason"] == "total_limit"
    }));
    let observation =
        fs::read_dir(PathBuf::from(result["output_dir"].as_str().unwrap()).join("observations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let observation_result: Value =
        serde_json::from_slice(&fs::read(observation.join("result.json")).unwrap()).unwrap();
    assert_eq!(observation_result["binary_budget"]["configured_bytes"], 5);
    assert_eq!(observation_result["binary_budget"]["consumed_bytes"], 5);
    assert_eq!(observation_result["binary_budget"]["remaining_bytes"], 0);
    assert_eq!(
        observation_result["binary_budget"]["artifacts"],
        json!([
            {
                "suite": "test_sql",
                "shard": "00",
                "name": "a-unknown.bin",
                "consumed_bytes": 5
            },
            {
                "suite": "test_sql",
                "shard": "00",
                "name": "z-small.bin",
                "consumed_bytes": 0
            }
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_binary_transfer_consumes_bytes_already_delivered() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/artifacts/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<a href=\"broken.bin\">broken.bin</a>"),
        )
        .mount(&server)
        .await;
    let binary_path = "/runs/123/sql/shard/00/artifacts/broken.bin";
    let proxy = HttpProxy::start_truncated(*server.address(), binary_path, b"12345".to_vec());
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args([
            "--include-binaries",
            "--max-binary-bytes",
            "100",
            "--max-binary-total-bytes",
            "100",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let observation =
        fs::read_dir(PathBuf::from(result["output_dir"].as_str().unwrap()).join("observations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let observation_result: Value =
        serde_json::from_slice(&fs::read(observation.join("result.json")).unwrap()).unwrap();
    assert_eq!(observation_result["binary_budget"]["consumed_bytes"], 5);
    assert_eq!(observation_result["binary_budget"]["remaining_bytes"], 95);
    assert_eq!(
        observation_result["binary_budget"]["artifacts"],
        json!([{
            "suite": "test_sql",
            "shard": "00",
            "name": "broken.bin",
            "consumed_bytes": 5
        }])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_storage_failure_charges_the_response_bytes_already_consumed() {
    let server = MockServer::start().await;
    mount_evidence(
        &server,
        "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        SHA,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/artifacts/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<a href=\"large.bin\">large.bin</a>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/runs/123/sql/shard/00/artifacts/large.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 64 * 1024]))
        .mount(&server)
        .await;
    let commands = tempfile::tempdir().unwrap();
    write_ci_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path());
    child.args([
        "--include-binaries",
        "--max-binary-bytes",
        "100000",
        "--max-binary-total-bytes",
        "100000",
    ]);
    unsafe {
        child.pre_exec(|| {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let limit = libc::rlimit {
                rlim_cur: 16 * 1024,
                rlim_max: 16 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = child.output().unwrap();

    assert_eq!(output.status.code(), Some(6));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let observation =
        fs::read_dir(PathBuf::from(result["output_dir"].as_str().unwrap()).join("observations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
    let observation_result: Value =
        serde_json::from_slice(&fs::read(observation.join("result.json")).unwrap()).unwrap();
    let consumed = observation_result["binary_budget"]["consumed_bytes"]
        .as_u64()
        .unwrap();
    assert!(
        consumed > 16 * 1024,
        "the consumed response chunk must be charged before its storage write fails: {consumed}"
    );
    assert_eq!(
        observation_result["binary_budget"]["remaining_bytes"],
        100000 - consumed
    );
    assert_eq!(
        observation_result["binary_budget"]["artifacts"][0]["consumed_bytes"],
        consumed
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

#[tokio::test(flavor = "multi_thread")]
async fn fresh_collection_reuses_the_pinned_actions_run_metadata() {
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
    let calls = commands.path().join("gh-calls");
    write_command(
        commands.path(),
        "gh",
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":900,"name":"collect","run_attempt":2}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            calls.display(),
            server.uri()
        ),
    );
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert!(output.status.success());
    let run_metadata_calls = fs::read_to_string(calls)
        .unwrap()
        .lines()
        .filter(|call| *call == "api repos/CUBRID/cubrid/actions/runs/123")
        .count();
    assert_eq!(run_metadata_calls, 1);
}

fn write_ci_commands(commands: &Path, server: &MockServer) {
    write_ci_commands_with_base(commands, &server.uri());
}

fn write_ci_commands_with_base(commands: &Path, artifact_base: &str) {
    write_command(
        commands,
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot()).unwrap()
        ),
    );
    write_gh_command_with_base(commands, artifact_base);
}

fn write_gh_command(commands: &Path, server: &MockServer) {
    write_gh_command_with_base(commands, &server.uri());
}

fn write_gh_command_with_base(commands: &Path, artifact_base: &str) {
    write_command(
        commands,
        "gh",
        &format!(
            r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":900,"name":"collect","run_attempt":2}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            artifact_base
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
