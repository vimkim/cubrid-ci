use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn status_without_pr_delegates_current_directory_and_options() {
    let commands = tempfile::tempdir().unwrap();
    write_status_command(
        commands.path(),
        r#"#!/bin/sh
printf '%s\n' "$@"
"#,
    );

    let output = cubrid_ci(commands.path())
        .args(["status", "--json", "--history", "0"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "--json\n--history\n0\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn status_canonicalizes_pr_number_and_preserves_delegated_exit() {
    let commands = tempfile::tempdir().unwrap();
    write_status_command(
        commands.path(),
        r#"#!/bin/sh
printf 'stdout:%s\n' "$*"
printf 'stderr:%s\n' "$*" >&2
exit 7
"#,
    );

    let output = cubrid_ci(commands.path())
        .args(["status", "7990", "--human"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "stdout:--human https://github.com/CUBRID/cubrid/pull/7990\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "stderr:--human https://github.com/CUBRID/cubrid/pull/7990\n"
    );
}

#[test]
fn status_passes_canonical_pr_url_and_watch_options() {
    let commands = tempfile::tempdir().unwrap();
    write_status_command(
        commands.path(),
        r#"#!/bin/sh
printf '%s\n' "$@"
"#,
    );

    let output = cubrid_ci(commands.path())
        .args([
            "status",
            "https://github.com/CUBRID/cubrid/pull/7990",
            "--watch",
            "--interval",
            "5",
            "--config",
            "/tmp/checks.json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "--config\n/tmp/checks.json\n--watch\n--interval\n5\n",
            "https://github.com/CUBRID/cubrid/pull/7990\n"
        )
    );
}

#[test]
fn missing_status_dependency_is_structured_in_json_mode() {
    let commands = tempfile::tempdir().unwrap();

    let output = cubrid_ci(commands.path())
        .args(["status", "--json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["kind"], "input");
    assert_eq!(error["exit_code"], 2);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("cubrid-pr-status")
    );
}

fn cubrid_ci(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
    command.env("PATH", path);
    command
}

fn write_status_command(directory: &Path, body: &str) {
    let path = directory.join("cubrid-pr-status");
    fs::write(&path, body).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
