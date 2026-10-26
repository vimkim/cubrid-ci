use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "52987c654b6f04734cad01c11c80c7f30358b33c";
const RUN: u64 = 40334404336;

#[tokio::test(flavor = "multi_thread")]
async fn rerun_collects_carried_suites_and_three_passing_shell_cases() {
    let fixture = Fixture::new().await;
    let output = fixture.collect(&[]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    for suite in ["test_medium", "test_sql", "test_shell"] {
        assert_eq!(result["suites"][suite]["state"], "completed");
    }
    let root = Path::new(result["output_dir"].as_str().unwrap());
    let summary: Value = serde_json::from_slice(
        &fs::read(root.join(result["suites"]["test_shell"]["summary"].as_str().unwrap())).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["counts"]["run"], 3);
    assert_eq!(summary["counts"]["failures"], 0);
    validate("suite-summary-v3.schema.json", &summary);
    assert_eq!(summary["rerun"]["mode"], "partial");
    assert_eq!(summary["rerun"]["run_id"], 40323508581_u64);
    let source: Value = serde_json::from_slice(
        &fs::read(root.join(summary["rerun"]["summary"].as_str().unwrap())).unwrap(),
    )
    .unwrap();
    assert_eq!(source["counts"]["failures"], 3);
    for suite in ["test_medium", "test_sql"] {
        let carried: Value = serde_json::from_slice(
            &fs::read(root.join(result["suites"][suite]["summary"].as_str().unwrap())).unwrap(),
        )
        .unwrap();
        validate("suite-summary-v3.schema.json", &carried);
        assert_eq!(carried["rerun"]["mode"], "carried_forward");
        assert_eq!(carried["counts"]["run"], 0);
        assert_eq!(carried["verdict"], "pass");
        assert_eq!(carried["shards"], serde_json::json!([]));
    }
    validate("command-result-v2.schema.json", &result);
    let requests = fixture.server.received_requests().await.unwrap().len();
    let replay = fixture.collect(&[]);
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stdout)
    );
    assert_eq!(
        fixture.server.received_requests().await.unwrap().len(),
        requests
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_cannot_reference_itself() {
    let fixture = Fixture::new().await;
    let gh = fixture.commands.path().join("gh");
    let script = fs::read_to_string(&gh)
        .unwrap()
        .replace("RERUN_FROM: 40323508581", "RERUN_FROM: 40334404336");
    fs::write(gh, script).unwrap();
    let output = fixture.collect(&["test_shell"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("cyclic")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_source_must_test_the_same_engine_commit() {
    let fixture = Fixture::new().await;
    Mock::given(method("GET"))
        .and(path("/runs/40323508581/shell/shard/21/build.read"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "sha=0000000000000000000000000000000000000000\nrun_id=40323508581\nrun_attempt=1\n",
        ))
        .with_priority(1)
        .mount(&fixture.server)
        .await;
    let output = fixture.collect(&["test_shell"]);
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("build SHA")
    );
    assert!(result["suites"]["test_shell"]["summary"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_artifact_base_still_requires_concrete_source_lineage() {
    let fixture = Fixture::new().await;
    let output = fixture
        .command(&[])
        .env("CUBRID_CI_ARTIFACT_BASE", fixture.server.uri())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let fixture = Fixture::new().await;
    let gh = fixture.commands.path().join("gh");
    let script = fs::read_to_string(&gh)
        .unwrap()
        .replace("RERUN_FROM: 40323508581", "RERUN_FROM: ");
    fs::write(gh, script).unwrap();
    let output = fixture.command(&["test_shell"]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("no concrete RERUN_FROM")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_rejects_tampered_linked_source_summary() {
    let fixture = Fixture::new().await;
    let output = fixture.collect(&["test_shell"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let root = Path::new(result["output_dir"].as_str().unwrap());
    let source =
        root.join("providers/github-actions/runs/40323508581/attempts/1/test_shell/summary.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    value["verdict"] = "pass".into();
    fs::write(source, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let output = fixture.collect(&["test_shell"]);
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("summary digest")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_carried_verdict_requires_reconciled_source_junit_counts() {
    let fixture = Fixture::new().await;
    Mock::given(method("GET"))
        .and(path(
            "/runs/40323508581/sql/shard/00/test-results/results.xml",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("<testsuite tests=\"0\"/>"))
        .with_priority(1)
        .mount(&fixture.server)
        .await;
    let output = fixture.collect(&["test_sql"]);
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("workflow result and JUnit counts disagree")
    );
    assert!(result["suites"]["test_sql"]["summary"].is_null());
}

fn validate(name: &str, instance: &Value) {
    let schema: Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("schema")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "{name}: {errors:?}");
}

struct Fixture {
    server: MockServer,
    commands: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr7990-rerun");
        for (run, prefix) in [(RUN, ""), (40323508581, "source")] {
            for suite in ["medium", "sql", "shell"] {
                let suite_fixture = fixture.join(prefix).join(suite);
                for entry in walkdir::WalkDir::new(&suite_fixture) {
                    let entry = entry.unwrap();
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let relative = entry.path().strip_prefix(&suite_fixture).unwrap();
                    let relative = relative.to_str().unwrap().trim_end_matches("index.html");
                    Mock::given(method("GET"))
                        .and(path(format!("/runs/{run}/{suite}/{relative}")))
                        .respond_with(
                            ResponseTemplate::new(200)
                                .set_body_bytes(fs::read(entry.path()).unwrap()),
                        )
                        .mount(&server)
                        .await;
                }
            }
        }
        Mock::given(method("GET"))
            .and(path(format!("/runs/{RUN}/shell/shard/00/test-results/")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<a href=\"test-shell.xml\">test-shell.xml</a>"),
            )
            .mount(&server)
            .await;
        let commands = tempfile::tempdir().unwrap();
        write_command(
            commands.path(),
            "cubrid-pr-status",
            &format!(
                "/bin/cat '{}'",
                fixture.join("status-snapshot.json").display()
            ),
        );
        write_command(
            commands.path(),
            "gh",
            &format!(
                r#"
case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *actions/runs/{RUN}/jobs*) /bin/cat '{fixture}/jobs.json' ;;
  *actions/runs/40323508581/jobs*) /bin/cat '{fixture}/source/jobs.json' ;;
  *actions/jobs/121614144713/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {base}' 'RERUN_FROM: 40323508581' ;;
  *actions/jobs/121594127253/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {base}' 'RERUN_FROM: ' ;;
  *actions/runs/{RUN}*) /bin/cat '{fixture}/run.json' ;;
  *actions/runs/40323508581*) /bin/cat '{fixture}/source/run.json' ;;
  *) printf 'unexpected gh request: %s\n' "$*" >&2; exit 64 ;;
esac
"#,
                fixture = fixture.display(),
                base = server.uri()
            ),
        );
        let data = tempfile::tempdir().unwrap();
        Self {
            server,
            commands,
            data,
        }
    }

    fn collect(&self, suites: &[&str]) -> std::process::Output {
        self.command(suites).output().unwrap()
    }

    fn command(&self, suites: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
        command
            .env("PATH", self.commands.path())
            .env("CUBRID_CI_CONFIG", self.data.path().join("missing.toml"))
            .env_remove("CUBRID_CI_DATA_DIR")
            .env_remove("CUBRID_CI_ARTIFACT_BASE")
            .args(["collect", "7990", "--commit", SHA, "--json", "--data-dir"])
            .arg(self.data.path());
        for suite in suites {
            command.args(["--suite", suite]);
        }
        command
    }
}

fn write_command(directory: &Path, name: &str, behavior: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{behavior}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
