use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const SHA: &str = "1111111111111111111111111111111111111111";
const OTHER_SHA: &str = "2222222222222222222222222222222222222222";

#[test]
fn current_directory_collection_selects_head_and_publishes_partial_manifest() {
    let fixture = Fixture::new(status_snapshot(SHA, true));

    let output = fixture.command(&[]).output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["schema_version"], 2);
    assert_eq!(result["ok"], false);
    assert_eq!(result["pr"]["number"], 7990);
    assert_eq!(result["commit"], SHA);
    assert_eq!(result["suites"]["test_medium"]["state"], "running");
    assert_eq!(result["suites"]["test_medium"]["execution"]["run_id"], 123);
    assert_eq!(result["suites"]["test_medium"]["execution"]["attempt"], 2);
    assert_eq!(result["suites"]["test_sql"]["state"], "not_observed");
    assert_eq!(result["suites"]["test_shell"]["state"], "not_observed");

    let manifest = fixture.manifest();
    assert_eq!(manifest, result);
}

#[test]
fn explicit_pr_requires_full_commit() {
    let fixture = Fixture::new(status_snapshot(SHA, false));

    let missing = fixture.command(&["7990"]).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let missing_json: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing_json["kind"], "input");

    let abbreviated = fixture
        .command(&["7990", "--commit", "1111111"])
        .output()
        .unwrap();
    assert_eq!(abbreviated.status.code(), Some(2));
    let abbreviated_json: Value = serde_json::from_slice(&abbreviated.stdout).unwrap();
    assert_eq!(abbreviated_json["kind"], "input");
}

#[test]
fn explicit_pr_url_and_commit_resolve_identity() {
    let fixture = Fixture::new(status_snapshot(SHA, false));

    let output = fixture
        .command(&[
            "https://github.com/CUBRID/cubrid/pull/7990",
            "--commit",
            SHA,
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["pr"]["url"],
        "https://github.com/CUBRID/cubrid/pull/7990"
    );
    assert_eq!(result["commit"], SHA);
}

#[test]
fn implicit_collection_rejects_a_different_published_head() {
    let fixture = Fixture::new(status_snapshot(OTHER_SHA, false));

    let output = fixture.command(&[]).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["kind"], "integrity");
    assert!(!fixture.manifest_path(SHA).exists());
}

#[test]
fn repeatable_suite_filters_publish_only_requested_suites() {
    let fixture = Fixture::new(status_snapshot(SHA, true));

    let output = fixture
        .command(&[
            "7990",
            "--commit",
            SHA,
            "--suite",
            "test_medium",
            "--suite",
            "test_shell",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["suites"].get("test_medium").is_some());
    assert!(result["suites"].get("test_shell").is_some());
    assert!(result["suites"].get("test_sql").is_none());
}

#[test]
fn wrong_sha_suite_status_publishes_integrity_failure() {
    let mut snapshot = status_snapshot(SHA, true);
    snapshot["checks"][0]["current"]["reported_for_sha"] = json!(OTHER_SHA);
    let fixture = Fixture::new(snapshot);

    let output = fixture.command(&[]).output().unwrap();

    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["suites"]["test_medium"]["state"],
        "collection_failed"
    );
    assert_eq!(result["errors"][0]["kind"], "integrity");
    assert_eq!(fixture.manifest(), result);
}

#[test]
fn actions_api_failure_publishes_remote_failure() {
    let fixture = Fixture::new(status_snapshot(SHA, true));
    write_command(
        fixture.commands.path(),
        "gh",
        &format!(
            r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *) exit 1 ;;
esac"#
        ),
    );

    let output = fixture.command(&[]).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["suites"]["test_medium"]["state"],
        "collection_failed"
    );
    assert_eq!(result["errors"][0]["kind"], "remote");
    assert_eq!(fixture.manifest(), result);
}

#[test]
fn suites_from_one_actions_run_share_the_execution_lookup() {
    let mut snapshot = status_snapshot(SHA, true);
    for suite in ["test_sql", "test_shell"] {
        let mut check = snapshot["checks"][0].clone();
        check["name"] = json!(format!("gha-ci: {suite}"));
        snapshot["checks"].as_array_mut().unwrap().push(check);
    }
    let fixture = Fixture::new(snapshot);
    let calls = fixture.commands.path().join("gh-calls");
    write_command(
        fixture.commands.path(),
        "gh",
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
            calls.display()
        ),
    );

    let output = fixture.command(&[]).output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    let actions_run_calls = fs::read_to_string(calls)
        .unwrap()
        .lines()
        .filter(|call| *call == "api repos/CUBRID/cubrid/actions/runs/123")
        .count();
    assert_eq!(actions_run_calls, 1);
}

#[test]
fn wait_timeout_is_nonzero_and_preserves_the_running_execution() {
    let fixture = Fixture::new(status_snapshot(SHA, true));

    let output = fixture
        .command(&[
            "--suite",
            "test_medium",
            "--wait",
            "--timeout",
            "2ms",
            "--poll-interval",
            "1ms",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["suites"]["test_medium"]["state"], "running");
    assert_eq!(result["suites"]["test_medium"]["execution"]["run_id"], 123);
    assert_eq!(fixture.manifest(), result);
}

#[test]
fn interactive_collection_reports_progress_on_one_stderr_line() {
    let fixture = Fixture::new(status_snapshot(SHA, true));
    write_command(
        fixture.commands.path(),
        "cubrid-pr-status",
        &format!(
            "/bin/sleep 0.1\n/bin/cat <<'JSON'\n{}\nJSON",
            serde_json::to_string(&status_snapshot(SHA, true)).unwrap()
        ),
    );
    write_command(
        fixture.commands.path(),
        "gh",
        &format!(
            r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{SHA}"}}}}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#
        ),
    );
    let stdout = fixture.data.path().join("stdout.json");
    let command = format!(
        "env -u CUBRID_CI_DATA_DIR -u CUBRID_CI_ARTIFACT_BASE PATH={} CUBRID_CI_CONFIG={} {} collect --json --data-dir {} > {}",
        fixture.commands.path().display(),
        missing_config_path().display(),
        env!("CARGO_BIN_EXE_cubrid-ci"),
        fixture.data.path().display(),
        stdout.display(),
    );
    let command = format!("/bin/sh -c '{command}'");

    let output = Command::new("script")
        .current_dir(fixture.worktree.path())
        .args(["-qefc", &command, "/dev/null"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "terminal output: {}\nscript stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let progress = String::from_utf8_lossy(&output.stdout);
    assert!(
        progress.contains("Collecting ["),
        "terminal stderr did not contain collection progress: {progress:?}"
    );
    assert!(
        !progress.contains('\n'),
        "terminal progress used multiple lines: {progress:?}"
    );
    let result: Value = serde_json::from_slice(&fs::read(stdout).unwrap()).unwrap();
    assert_eq!(result["suites"]["test_medium"]["state"], "running");
}

#[test]
fn oversized_status_snapshot_is_rejected_before_json_parsing() {
    let fixture = Fixture::new(status_snapshot(SHA, true));
    write_command(
        fixture.commands.path(),
        "cubrid-pr-status",
        "/usr/bin/head -c 8388609 /dev/zero | /usr/bin/tr '\\000' x",
    );

    let output = fixture.command(&[]).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["diagnostic"], "oversized");
}

struct Fixture {
    commands: tempfile::TempDir,
    worktree: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Fixture {
    fn new(snapshot: Value) -> Self {
        let commands = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        write_command(commands.path(), "git", &format!("printf '%s\\n' '{SHA}'"));
        write_command(
            commands.path(),
            "cubrid-pr-status",
            &format!(
                "/bin/cat <<'JSON'\n{}\nJSON",
                serde_json::to_string(&snapshot).unwrap()
            ),
        );
        write_command(
            commands.path(),
            "gh",
            r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{"number":7990,"head":{"sha":"1111111111111111111111111111111111111111"}}' ;;
  *actions/runs/123*) printf '%s\n' '{"id":123,"run_attempt":2}' ;;
  *) exit 64 ;;
esac"#,
        );
        Self {
            commands,
            worktree,
            data,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
        command
            .current_dir(self.worktree.path())
            .env("PATH", self.commands.path())
            .env("CUBRID_CI_CONFIG", missing_config_path())
            .env_remove("CUBRID_CI_DATA_DIR")
            .env_remove("CUBRID_CI_ARTIFACT_BASE")
            .args(["collect", "--json", "--data-dir"])
            .arg(self.data.path())
            .args(args);
        command
    }

    fn manifest_path(&self, commit: &str) -> PathBuf {
        self.data
            .path()
            .join("github-actions/CUBRID-cubrid/pr-7990")
            .join(commit)
            .join("manifest.json")
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&fs::read(self.manifest_path(SHA)).unwrap()).unwrap()
    }
}

fn status_snapshot(head_sha: &str, medium_running: bool) -> Value {
    let mut checks = Vec::new();
    if medium_running {
        checks.push(json!({
            "name": "gha-ci: test_medium",
            "provider": "GitHub Actions",
            "expected": true,
            "freshness": "current",
            "current": {
                "state": "PENDING",
                "reported_for_sha": head_sha,
                "detail_url": "https://github.com/CUBRID/cubrid/actions/runs/123",
                "reported_at": "2026-09-21T18:10:04Z"
            },
            "previous": null
        }));
    }
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
        "checks": checks,
        "history": { "status": "disabled", "limit": 0, "searched": 0 },
        "errors": []
    })
}

fn missing_config_path() -> PathBuf {
    std::env::temp_dir().join("cubrid-ci-collect-no-config.toml")
}

fn write_command(directory: &Path, name: &str, behavior: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{behavior}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
