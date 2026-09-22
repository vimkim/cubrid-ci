use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "1111111111111111111111111111111111111111";
const TC_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
static TIMING_TEST: Mutex<()> = Mutex::new(());

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
    let delay = Duration::from_millis(150);
    mount_medium_with_delay(&server, delay).await;
    mount_shell_with_delay(&server, delay).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let _timing_guard = TIMING_TEST
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let started = Instant::now();
    let output = command(commands.path(), data.path())
        .arg("--json")
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(3));
    assert!(
        elapsed < Duration::from_millis(3000),
        "independent suites took {elapsed:?}; expected their provider waits to overlap"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_shards_are_fetched_concurrently() {
    let server = MockServer::start().await;
    let delay = Duration::from_millis(150);
    mount_medium_shards_with_delay(&server, 4, delay).await;
    let commands = tempfile::tempdir().unwrap();
    write_commands(commands.path(), &server);
    let data = tempfile::tempdir().unwrap();

    let _timing_guard = TIMING_TEST
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let started = Instant::now();
    let output = command(commands.path(), data.path())
        .args(["--suite", "test_medium", "--json"])
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    assert!(output.status.success());
    assert!(
        elapsed < Duration::from_millis(3000),
        "independent shards took {elapsed:?}; expected their provider waits to overlap"
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
            server.uri()
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

async fn mount_medium_with_delay(server: &MockServer, delay: Duration) {
    mount_medium_with_optional_delay(server, Some(delay)).await;
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

async fn mount_medium_shards_with_delay(server: &MockServer, count: usize, delay: Duration) {
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
        Some(delay),
    )
    .await;
    mount(
        server,
        &format!("{root}/plan/shards/"),
        &planned_index,
        Some(delay),
    )
    .await;
    mount(
        server,
        &format!("{root}/plan/plan.tsv"),
        &plan_table,
        Some(delay),
    )
    .await;
    mount(server, &format!("{root}/shard/"), &shard_index, Some(delay)).await;
    mount(
        server,
        &format!("{root}/collect/failed.list"),
        "",
        Some(delay),
    )
    .await;
    mount(
        server,
        &format!("{root}/collect/verdict"),
        "pass\n",
        Some(delay),
    )
    .await;
    for shard in shard_names {
        mount(
            server,
            &format!("{root}/shard/{shard}/build.read"),
            &format!("sha={SHA}\nmode=debug\nns=develop\nrun_id=200\nrun_attempt=1\n"),
            Some(delay),
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/tc.read"),
            &format!("tc_sha={TC_SHA}\ntc_branch=develop\n"),
            Some(delay),
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/summary_info"),
            &format!("case-{shard}.sql:ok 10ms\n"),
            Some(delay),
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/shard.done"),
            &format!("suite=medium\nidx={shard}\nassigned=1\nctp_rc=0\n"),
            Some(delay),
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/test-results/"),
            "<a href=\"results.xml\">results.xml</a>",
            Some(delay),
        )
        .await;
        mount(
            server,
            &format!("{root}/shard/{shard}/test-results/results.xml"),
            &format!(
                "<testsuite tests=\"1\"><testcase name=\"case-{shard}.sql\" time=\"0.01\"/></testsuite>"
            ),
            Some(delay),
        )
        .await;
    }
}

async fn mount_shell(server: &MockServer) {
    mount_shell_with_optional_delay(server, None).await;
}

async fn mount_shell_with_delay(server: &MockServer, delay: Duration) {
    mount_shell_with_optional_delay(server, Some(delay)).await;
}

async fn mount_shell_with_optional_delay(server: &MockServer, delay: Option<Duration>) {
    mount_common(
        server,
        300,
        "shell",
        "00\tshell/foo/cases/foo.sh\n",
        250,
        delay,
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
        delay,
    )
    .await;
    mount(
        server,
        "/runs/300/shell/shard/00/test-results/test-shell.xml",
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
