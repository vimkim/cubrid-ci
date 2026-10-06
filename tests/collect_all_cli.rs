mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::HttpProxy;

const SHA: &str = "1111111111111111111111111111111111111111";
const TC_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[tokio::test(flavor = "multi_thread")]
async fn mixed_suite_states_keep_completed_evidence_from_independent_runs() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    mount_shell(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .arg("--json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    assert!(
        output.stderr.is_empty(),
        "non-TTY JSON collection wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    validate_schema(
        include_str!("../schema/command-result-v2.schema.json"),
        &output.stdout,
    );
    assert_eq!(result["ok"], false);
    assert_eq!(result["suites"]["test_medium"]["state"], "completed");
    assert_eq!(
        result["suites"]["test_medium"]["status"]["state"],
        "SUCCESS"
    );
    assert_eq!(result["suites"]["test_medium"]["execution"]["run_id"], 200);
    assert_eq!(result["suites"]["test_sql"]["state"], "running");
    assert_eq!(result["suites"]["test_sql"]["execution"]["run_id"], 123);
    assert_eq!(result["suites"]["test_shell"]["state"], "completed");
    assert_eq!(result["suites"]["test_shell"]["status"]["state"], "FAILURE");
    assert_eq!(result["suites"]["test_shell"]["execution"]["run_id"], 300);
    assert_eq!(result["suites"]["test_shell"]["execution"]["attempt"], 3);
    let gh_calls = fs::read_to_string(commands.path().join("gh-calls")).unwrap();
    let first_download = gh_calls.find("actions/runs/200/jobs").unwrap();
    for run in ["actions/runs/200", "actions/runs/123", "actions/runs/300"] {
        assert!(
            gh_calls.find(run).unwrap() < first_download,
            "{run} was not pinned before evidence download:\n{gh_calls}"
        );
    }

    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    for suite in ["test_medium", "test_shell"] {
        let summary = result["suites"][suite]["summary"].as_str().unwrap();
        assert!(output_dir.join(summary).is_file());
    }
    let shell_summary_path = result["suites"]["test_shell"]["summary"].as_str().unwrap();
    let shell_summary: Value =
        serde_json::from_slice(&fs::read(output_dir.join(shell_summary_path)).unwrap()).unwrap();
    assert_eq!(shell_summary["run_id"], 300);
    assert_eq!(shell_summary["attempt"], 3);
    assert_eq!(shell_summary["shards"][0]["build"]["run_id"], 250);
    assert_eq!(shell_summary["shards"][0]["build"]["run_attempt"], 1);
    assert_eq!(
        fs::read_to_string(output_dir.join(
            "providers/github-actions/runs/300/attempts/3/test_shell/raw/shards/00/test_status.data"
        ))
        .unwrap(),
        concat!(
            "#\n",
            "#Tue Sep 22 06:03:57 KST 2026\n",
            "total_executed_case_count=1\n",
            "total_success_case_count=0\n",
            "total_fail_case_count=1\n",
            "total_skip_case_count=0\n"
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn human_output_reports_ci_outcomes_and_test_counts() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    mount_shell(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path()).output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    assert!(
        output.stderr.is_empty(),
        "non-TTY human collection wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("test_medium: SUCCESS (0 failed / 1 executed)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("test_shell: FAILURE (1 failed / 1 executed)"),
        "{stdout}"
    );
    assert!(stdout.contains("test_sql: PENDING (running)"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_terminal_suites_are_collected_concurrently() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    mount_shell(&server).await;
    let (proxy, gate) = HttpProxy::start_gated(*server.address(), "/plan/split.meta");
    let commands = tempfile::tempdir().unwrap();
    write_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path())
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    if !gate.wait_for_arrivals(2, Duration::from_secs(5)) {
        gate.release_all();
        let _ = child.kill();
        panic!("two terminal suites did not reach the response barrier concurrently");
    }
    gate.release_all();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    assert!(proxy.maximum() >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_shards_are_fetched_concurrently() {
    let server = MockServer::start().await;
    mount_medium_shards(&server, 4, None, &[]).await;
    let (proxy, gate) = HttpProxy::start_gated(*server.address(), "/build.read");
    let commands = tempfile::tempdir().unwrap();
    write_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path())
        .args([
            "--suite",
            "test_medium",
            "--fetch-concurrency",
            "10",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    if !gate.wait_for_arrivals(2, Duration::from_secs(5)) {
        gate.release_all();
        let _ = child.kill();
        panic!("two shards did not reach the response barrier concurrently");
    }
    gate.release_all();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(proxy.maximum() >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn configured_fetch_concurrency_is_shared_across_suites() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    mount_shell(&server).await;
    let (proxy, gate) = HttpProxy::start_gated(*server.address(), "/plan/split.meta");
    let commands = tempfile::tempdir().unwrap();
    write_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path())
        .args([
            "--suite",
            "test_medium",
            "--suite",
            "test_shell",
            "--fetch-concurrency",
            "1",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    if !gate.wait_for_arrivals(1, Duration::from_secs(5)) {
        gate.release_all();
        let _ = child.kill();
        panic!("first suite did not reach the response barrier");
    }
    assert_eq!(proxy.maximum(), 1);
    gate.release_one();
    if !gate.wait_for_arrivals(2, Duration::from_secs(5)) {
        gate.release_all();
        let _ = child.kill();
        panic!("second suite did not reach the response barrier after one release");
    }
    assert_eq!(proxy.maximum(), 1);
    gate.release_all();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(proxy.maximum(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_shard_raw_evidence_survives_a_later_shard_failure() {
    let server = MockServer::start().await;
    mount_medium_shards(&server, 12, None, &["01", "02"]).await;
    let (proxy, gate) = HttpProxy::start_gated(*server.address(), "/tc.read");
    let commands = tempfile::tempdir().unwrap();
    write_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path())
        .args([
            "--suite",
            "test_medium",
            "--fetch-concurrency",
            "10",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !gate.wait_for_arrivals(10, Duration::from_secs(5)) {
        gate.release_all();
        let _ = child.kill();
        panic!("the initial shard window did not reach the testcase-response barrier");
    }
    gate.release_all();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let output_dir = PathBuf::from(result["output_dir"].as_str().unwrap());
    let suite = output_dir.join("providers/github-actions/runs/200/attempts/1/test_medium");
    assert!(suite.join("raw/shards/00/results.xml").is_file());
    assert!(!suite.join("summary.json").exists());
    let observation = fs::read_dir(output_dir.join("observations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let observation_result: Value =
        serde_json::from_slice(&fs::read(observation.join("result.json")).unwrap()).unwrap();
    let shards = &observation_result["acquisition"]["test_medium"]["shards"];
    assert_eq!(shards["00"]["state"], "retained");
    assert_eq!(shards["01"]["state"], "failed");
    assert_eq!(shards["01"]["stage"], "testcase_provenance");
    assert_eq!(shards["02"]["state"], "failed");
    assert_eq!(shards["10"]["state"], "not_attempted");
    assert_eq!(shards["11"]["state"], "not_attempted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn suite_failure_stops_later_shard_scheduling_across_suites() {
    let server = MockServer::start().await;
    mount_medium_shards(&server, 4, None, &[]).await;
    mount_shell(&server).await;
    let shell_plan = "/runs/300/shell/plan/plan.tsv";
    let (failure_proxy, failure_gate) =
        HttpProxy::start_gated_truncated(*server.address(), shell_plan, b"00\t".to_vec());
    let (proxy, shard_gate) = HttpProxy::start_gated(failure_proxy.address(), "/tc.read");
    let commands = tempfile::tempdir().unwrap();
    write_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path())
        .args([
            "--suite",
            "test_medium",
            "--suite",
            "test_shell",
            "--fetch-concurrency",
            "3",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    if !failure_gate.wait_for_arrivals(1, Duration::from_secs(5))
        || !shard_gate.wait_for_arrivals(2, Duration::from_secs(5))
    {
        failure_gate.release_all();
        shard_gate.release_all();
        let _ = child.kill();
        panic!("suite failure and the initial cross-suite shard window were not both in flight");
    }
    failure_gate.release_all();
    if !shard_gate.wait_for_arrivals(3, Duration::from_secs(5)) {
        shard_gate.release_all();
        let _ = child.kill();
        panic!("the already-scheduled third shard did not drain after the suite failure");
    }
    shard_gate.release_all();
    let output = child.wait_with_output().unwrap();

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
    let medium = &observation_result["acquisition"]["test_medium"]["shards"];
    assert_eq!(medium["00"]["state"], "retained");
    assert_eq!(medium["01"]["state"], "retained");
    assert_eq!(medium["02"]["state"], "retained");
    assert_eq!(medium["03"]["state"], "not_attempted");
    assert_eq!(
        observation_result["acquisition"]["test_shell"]["errors"][0]["stage"],
        "plan_table"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_collection_rejects_tampered_retained_raw_evidence() {
    let server = MockServer::start().await;
    mount_medium_shards(&server, 12, None, &["01", "02"]).await;
    let (proxy, gate) = HttpProxy::start_gated(*server.address(), "/tc.read");
    let commands = tempfile::tempdir().unwrap();
    write_commands_with_base(commands.path(), &proxy.uri());
    let data = tempfile::tempdir().unwrap();

    let mut child = command(commands.path(), data.path())
        .args([
            "--suite",
            "test_medium",
            "--fetch-concurrency",
            "10",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !gate.wait_for_arrivals(10, Duration::from_secs(5)) {
        gate.release_all();
        let _ = child.kill();
        panic!("the initial shard window did not reach the testcase-response barrier");
    }
    gate.release_all();
    let first = child.wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(5));
    let first_result: Value = serde_json::from_slice(&first.stdout).unwrap();
    let retained = PathBuf::from(first_result["output_dir"].as_str().unwrap())
        .join("providers/github-actions/runs/200/attempts/1/test_medium/raw/shards/00/results.xml");
    fs::write(
        &retained,
        "<testsuite tests=\"1\"><testcase name=\"case-00.sql\" time=\"1.5\"/></testsuite>",
    )
    .unwrap();

    let replay = command(commands.path(), data.path())
        .args(["--suite", "test_medium", "--json"])
        .output()
        .unwrap();

    assert_eq!(replay.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("receipt")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn suites_from_one_run_share_actions_discovery() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args(["--suite", "test_medium", "--suite", "test_medium", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let calls = fs::read_to_string(commands.path().join("gh-calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|call| call.contains("actions/runs/200/jobs"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|call| call.contains("actions/jobs/920/logs"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn distinct_suites_from_one_run_share_successful_actions_discovery() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    mount_shell_for_run(&server, 200).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let mut snapshot = status_snapshot();
    snapshot["checks"][2] = status("test_shell", "FAILURE", 200);
    write_command(
        commands.path(),
        "cubrid-pr-status",
        &format!(
            "/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&snapshot).unwrap()
        ),
    );
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args(["--suite", "test_medium", "--suite", "test_shell", "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(commands.path().join("gh-calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|call| call.contains("actions/runs/200/jobs"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|call| call.contains("actions/jobs/920/logs"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_collection_accepts_junit_larger_than_generic_text_evidence() {
    let server = MockServer::start().await;
    mount_large_shell(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let output = command(commands.path(), data.path())
        .args(["--suite", "test_shell", "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_shell"]["state"], "completed");
}

fn command(commands: &Path, data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
    command
        .env("PATH", commands)
        .env("CUBRID_CI_CONFIG", missing_config_path())
        .env_remove("CUBRID_CI_DATA_DIR")
        .env_remove("CUBRID_CI_ARTIFACT_BASE")
        .args(["collect", "7990", "--commit", SHA, "--data-dir"])
        .arg(data);
    command
}

fn write_commands(commands: &Path, server: &MockServer) {
    write_commands_with_base(commands, &server.uri());
}

fn write_commands_with_base(commands: &Path, artifact_base: &str) {
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
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *actions/runs/200/jobs*) printf '%s\n' '{{"jobs":[{{"id":920,"name":"collect","run_attempt":1}}]}}' ;;
  *actions/runs/300/jobs*) printf '%s\n' '{{"jobs":[{{"id":930,"name":"collect","run_attempt":3}}]}}' ;;
  *actions/jobs/920/logs*|*actions/jobs/930/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/200*) printf '%s\n' '{{"id":200,"run_attempt":1}}' ;;
  *actions/runs/300*) printf '%s\n' '{{"id":300,"run_attempt":3}}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            commands.join("gh-calls").display(),
            artifact_base
        ),
    );
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
        "checks": [
            status("test_medium", "SUCCESS", 200),
            status("test_sql", "PENDING", 123),
            status("test_shell", "FAILURE", 300)
        ],
        "history": { "status": "disabled", "limit": 0, "searched": 0 },
        "errors": []
    })
}

fn status(suite: &str, state: &str, run_id: u64) -> Value {
    json!({
        "name": format!("gha-ci: {suite}"),
        "provider": "GitHub Actions",
        "expected": true,
        "freshness": "current",
        "current": {
            "state": state,
            "reported_for_sha": SHA,
            "detail_url": format!("https://github.com/CUBRID/cubrid/actions/runs/{run_id}"),
            "reported_at": "2026-09-21T18:10:04Z"
        },
        "previous": null
    })
}

async fn mount_medium(server: &MockServer) {
    mount_medium_with_optional_delay(server, None).await;
}

async fn mount_medium_with_optional_delay(server: &MockServer, delay: Option<Duration>) {
    mount_common(server, 200, "medium", "", 200, delay).await;
    mount(
        server,
        "/runs/200/medium/shard/00/summary_info",
        "case.sql:ok 10ms\n",
        delay,
    )
    .await;
    mount(
        server,
        "/runs/200/medium/shard/00/test-results/results.xml",
        "<testsuite tests=\"1\"><testcase name=\"case.sql\" time=\"0.01\"/></testsuite>",
        delay,
    )
    .await;
}

async fn mount_medium_shards(
    server: &MockServer,
    count: usize,
    delay: Option<Duration>,
    failing_testcase_shards: &[&str],
) {
    let root = "/runs/200/medium";
    let shard_names = (0..count)
        .map(|index| format!("{index:02}"))
        .collect::<Vec<_>>();
    let planned_index = shard_names
        .iter()
        .map(|shard| format!("<a href=\"{shard}.list\">{shard}.list</a>"))
        .collect::<String>();
    let shard_index = shard_names
        .iter()
        .map(|shard| format!("<a href=\"{shard}/\">{shard}/</a>"))
        .collect::<String>();
    let plan_table = shard_names
        .iter()
        .map(|shard| format!("{shard}\t1\t1\t1.0\n"))
        .collect::<String>();
    mount(
        server,
        &format!("{root}/plan/split.meta"),
        &format!(
            "total={count}\nunits={count}\npar={count}\ntimed=0\ntc_sha='{TC_SHA}'\ntc_branch='develop'\n"
        ),
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/plan/shards/"),
        &planned_index,
        delay,
    )
    .await;
    mount(server, &format!("{root}/plan/plan.tsv"), &plan_table, delay).await;
    mount(server, &format!("{root}/shard/"), &shard_index, delay).await;
    mount(server, &format!("{root}/collect/failed.list"), "", delay).await;
    mount(server, &format!("{root}/collect/verdict"), "pass\n", delay).await;
    for shard in shard_names {
        mount(
            server,
            &format!("{root}/shard/{shard}/build.read"),
            &format!("sha={SHA}\nmode=debug\nns=develop\nrun_id=200\nrun_attempt=1\n"),
            delay,
        )
        .await;
        if failing_testcase_shards.contains(&shard.as_str()) {
            Mock::given(method("GET"))
                .and(path(format!("{root}/shard/{shard}/tc.read")))
                .respond_with(ResponseTemplate::new(503))
                .mount(server)
                .await;
        } else {
            mount(
                server,
                &format!("{root}/shard/{shard}/tc.read"),
                &format!("tc_sha={TC_SHA}\ntc_branch=develop\n"),
                delay,
            )
            .await;
        }
        mount(
            server,
            &format!("{root}/shard/{shard}/summary_info"),
            &format!("case-{shard}.sql:ok 10ms\n"),
            delay,
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/shard.done"),
            &format!("suite=medium\nidx={shard}\nassigned=1\nctp_rc=0\n"),
            delay,
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/test-results/"),
            "<a href=\"results.xml\">results.xml</a>",
            delay,
        )
        .await;
        let junit_path = format!("{root}/shard/{shard}/test-results/results.xml");
        let junit = format!(
            "<testsuite tests=\"1\"><testcase name=\"case-{shard}.sql\" time=\"0.01\"/></testsuite>"
        );
        mount(server, &junit_path, &junit, delay).await;
    }
}

async fn mount_shell(server: &MockServer) {
    mount_shell_for_run(server, 300).await;
}

async fn mount_shell_for_run(server: &MockServer, run_id: u64) {
    mount_shell_for_run_with_optional_delay(server, run_id, None).await;
}

async fn mount_shell_for_run_with_optional_delay(
    server: &MockServer,
    run_id: u64,
    delay: Option<Duration>,
) {
    mount_common(
        server,
        run_id,
        "shell",
        "00\tshell/foo/cases/foo.sh\n",
        250,
        delay,
    )
    .await;
    mount(
        server,
        &format!("/runs/{run_id}/shell/shard/00/test_status.data"),
        concat!(
            "#\n",
            "#Tue Sep 22 06:03:57 KST 2026\n",
            "total_executed_case_count=1\n",
            "total_success_case_count=0\n",
            "total_fail_case_count=1\n",
            "total_skip_case_count=0\n"
        ),
        delay,
    )
    .await;
    mount(
        server,
        &format!("/runs/{run_id}/shell/shard/00/test-results/test-shell.xml"),
        concat!(
            "<testsuite tests=\"1\" failures=\"1\">",
            "<testcase name=\"shell/foo/cases/foo.sh\"><failure><![CDATA[assert failed]]></failure></testcase>",
            "</testsuite>"
        ),
        delay,
    )
    .await;
}

async fn mount_large_shell(server: &MockServer) {
    mount_common(
        server,
        300,
        "shell",
        "00\tshell/foo/cases/foo.sh\n",
        250,
        None,
    )
    .await;
    mount(
        server,
        "/runs/300/shell/shard/00/test_status.data",
        concat!(
            "#\n",
            "#Tue Sep 22 06:03:57 KST 2026\n",
            "total_executed_case_count=1\n",
            "total_success_case_count=0\n",
            "total_fail_case_count=1\n",
            "total_skip_case_count=0\n"
        ),
        None,
    )
    .await;
    let xml = format!(
        "<testsuite tests=\"1\" failures=\"1\"><testcase name=\"shell/foo/cases/foo.sh\"><failure>{}</failure></testcase></testsuite>",
        "x".repeat(16 * 1024 * 1024)
    );
    mount(
        server,
        "/runs/300/shell/shard/00/test-results/test-shell.xml",
        &xml,
        None,
    )
    .await;
}

async fn mount_common(
    server: &MockServer,
    run_id: u64,
    suite: &str,
    failed: &str,
    build_run_id: u64,
    delay: Option<Duration>,
) {
    let root = format!("/runs/{run_id}/{suite}");
    mount(
        server,
        &format!("{root}/plan/split.meta"),
        &format!("total=1\nunits=1\npar=1\ntimed=0\ntc_sha='{TC_SHA}'\ntc_branch='develop'\n"),
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/plan/shards/"),
        "<a href=\"00.list\">00.list</a>",
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/plan/plan.tsv"),
        "00\t1\t1\t1.0\n",
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/shard/"),
        "<a href=\"00/\">00/</a>",
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/shard/00/test-results/"),
        if suite == "shell" {
            "<a href=\"test-shell.xml\">test-shell.xml</a>"
        } else {
            "<a href=\"results.xml\">results.xml</a>"
        },
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/collect/failed.list"),
        failed,
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/collect/verdict"),
        if failed.is_empty() {
            "pass\n"
        } else {
            "fail\n"
        },
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/shard/00/build.read"),
        &format!("sha={SHA}\nmode=debug\nns=develop\nrun_id={build_run_id}\nrun_attempt=1\n"),
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/shard/00/tc.read"),
        &format!("tc_sha={TC_SHA}\ntc_branch=develop\n"),
        delay,
    )
    .await;
    mount(
        server,
        &format!("{root}/shard/00/shard.done"),
        &format!("suite={suite}\nidx=00\nassigned=1\nctp_rc=0\n"),
        delay,
    )
    .await;
}

async fn mount(server: &MockServer, url_path: &str, body: &str, delay: Option<Duration>) {
    let mut response = ResponseTemplate::new(200).set_body_string(body);
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(method("GET"))
        .and(path(url_path))
        .respond_with(response)
        .mount(server)
        .await;
}

fn missing_config_path() -> PathBuf {
    std::env::temp_dir().join("cubrid-ci-all-no-config.toml")
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

// Historical dispatch collection exercises the same suite parser and cached-build proof.
#[tokio::test(flavor = "multi_thread")]
async fn commit_dispatch_collects_red_suite_with_reused_build_and_no_pr_identity() {
    let server = MockServer::start().await;
    mount_shell(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commit_commands(commands.path(), &server, "completed", 3);
    let data = tempfile::tempdir().unwrap();
    let output = commit_command(commands.path(), data.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    validate_schema(
        include_str!("../schema/command-result-v3.schema.json"),
        &output.stdout,
    );
    validate_schema(
        include_str!("../schema/manifest-v3.schema.json"),
        &output.stdout,
    );
    assert!(result.get("pr").is_none());
    assert_eq!(result["commit"], SHA);
    assert_eq!(
        result["selection"]["runs"]["300"]["event"],
        "workflow_dispatch"
    );
    assert_eq!(result["selection"]["runs"]["300"]["head_sha"], TC_SHA); // workflow revision differs
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let summary: Value = serde_json::from_slice(
        &fs::read(root.join(result["suites"]["test_shell"]["summary"].as_str().unwrap())).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["shards"][0]["build"]["run_id"], 250);
    assert_eq!(summary["counts"]["failures"], 1);
    let observation = fs::read_dir(root.join("observations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    validate_schema(
        include_str!("../schema/collection-observation-request-v2.schema.json"),
        &fs::read(observation.join("request.json")).unwrap(),
    );
    validate_schema(
        include_str!("../schema/collection-observation-result-v2.schema.json"),
        &fs::read(observation.join("result.json")).unwrap(),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_run_derives_suite_verdict_and_rejects_wrong_engine() {
    let server = MockServer::start().await;
    mount_medium(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commit_commands(commands.path(), &server, "completed", 3);
    let data = tempfile::tempdir().unwrap();
    // Aggregate run may be red; the medium suite is green.
    let output = commit_suite_command(commands.path(), data.path(), "test_medium")
        .args(["--run-id", "200"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["suites"]["test_medium"].get("status").is_none());
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let summary: Value = serde_json::from_slice(
        &fs::read(root.join(result["suites"]["test_medium"]["summary"].as_str().unwrap())).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["ci_state"], "success");
    let wrong = "2222222222222222222222222222222222222222";
    let script = fs::read_to_string(commands.path().join("gh"))
        .unwrap()
        .replace(
            &format!("\"sha\":\"{SHA}\""),
            &format!("\"sha\":\"{wrong}\""),
        );
    fs::write(commands.path().join("gh"), script).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
        .env("PATH", commands.path())
        .env("CUBRID_CI_CONFIG", missing_config_path())
        .args([
            "collect-commit",
            wrong,
            "--run-id",
            "200",
            "--suite",
            "test_medium",
            "--json",
            "--data-dir",
        ])
        .arg(data.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let wrong_result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(wrong_result["errors"][0]["kind"], "integrity");
    assert!(
        wrong_result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("build")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn newer_attempt_running_does_not_reuse_old_terminal_status() {
    let server = MockServer::start().await;
    let commands = tempfile::tempdir().unwrap();
    write_commit_commands(commands.path(), &server, "in_progress", 4);
    let data = tempfile::tempdir().unwrap();
    let output = commit_command(commands.path(), data.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["suites"]["test_shell"]["state"], "running");
    assert_eq!(result["suites"]["test_shell"]["execution"]["attempt"], 4);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn old_attempt_requires_validated_cache_and_never_downloads_current_artifacts() {
    let server = MockServer::start().await;
    mount_shell(&server).await;
    let commands = tempfile::tempdir().unwrap();
    write_commit_commands(commands.path(), &server, "completed", 4);
    let data = tempfile::tempdir().unwrap();
    let args = ["--run-id", "300", "--attempt", "3"];
    let output = commit_command(commands.path(), data.path())
        .args(args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(server.received_requests().await.unwrap().is_empty());
    // Populate an honestly collected immutable attempt-3 bundle, then advance metadata.
    write_commit_commands(commands.path(), &server, "completed", 3);
    assert!(
        commit_command(commands.path(), data.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let requests = server.received_requests().await.unwrap().len();
    write_commit_commands(commands.path(), &server, "completed", 4);
    let output = commit_command(commands.path(), data.path())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    // Tampered cache must not fall back to remote evidence.
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let summary = PathBuf::from(result["output_dir"].as_str().unwrap())
        .join(result["suites"]["test_shell"]["summary"].as_str().unwrap());
    fs::write(summary, b"{}").unwrap();
    assert_eq!(
        commit_command(commands.path(), data.path())
            .args(args)
            .output()
            .unwrap()
            .status
            .code(),
        Some(4)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

fn commit_command(commands: &Path, data: &Path) -> Command {
    commit_suite_command(commands, data, "test_shell")
}

fn commit_suite_command(commands: &Path, data: &Path, suite: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
    command
        .env("PATH", commands)
        .env("CUBRID_CI_CONFIG", missing_config_path())
        .env_remove("CUBRID_CI_DATA_DIR")
        .env_remove("CUBRID_CI_ARTIFACT_BASE")
        .args([
            "collect-commit",
            SHA,
            "--suite",
            suite,
            "--json",
            "--data-dir",
        ])
        .arg(data);
    command
}

fn write_commit_commands(commands: &Path, server: &MockServer, state: &str, attempt: u64) {
    write_commands(commands, server);
    let path = commands.join("gh");
    let mut script = fs::read_to_string(&path).unwrap();
    let statuses = json!([[{"context":"gha-ci: test_shell","state":"failure","target_url":"https://github.com/CUBRID/cubrid/actions/runs/300","created_at":"2026-10-06T00:00:00Z","id":1}]]);
    script = script.replace(
        "case \"$*\" in",
        &format!(
            r#"case "$*" in
*commits/*/statuses*) printf '%s\n' '{statuses}' ;;
*commits/*/check-runs*) printf '%s\n' '[{{"check_runs":[]}}]' ;;
*commits/*) printf '%s\n' '{{"sha":"{SHA}"}}' ;;"#
        ),
    );
    for (id, at) in [(200, 1), (300, attempt)] {
        let old = format!(
            "{{\"id\":{id},\"run_attempt\":{}}}",
            if id == 300 { 3 } else { 1 }
        );
        let run = json!({"id":id,"run_attempt":at,"status":state,"conclusion":"failure","event":"workflow_dispatch","head_sha":TC_SHA,"path":".github/workflows/gha-ci.yml","repository":{"full_name":"CUBRID/cubrid"},"updated_at":"2026-10-06T00:00:00Z"});
        script = script.replace(&old, &run.to_string());
    }
    fs::write(path, script).unwrap();
}
