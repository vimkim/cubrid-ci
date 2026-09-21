use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

#[test]
fn doctor_reports_healthy_local_setup_as_json() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");
    let evidence_root = tempfile::tempdir().unwrap();

    let output = doctor(commands.path(), evidence_root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["data_dir"], evidence_root.path().to_str().unwrap());
    assert_eq!(result["data_dir_source"], "command_line");
    assert_eq!(check_state(&result, "cubrid-pr-status"), "healthy");
    assert_eq!(check_state(&result, "gh-auth"), "healthy");
    assert_eq!(check_state(&result, "evidence-root"), "healthy");
    assert_eq!(check_state(&result, "evidence-server"), "deferred");
}

#[test]
fn doctor_reports_missing_status_dependency_independently() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "gh", "exit 0");
    let evidence_root = tempfile::tempdir().unwrap();

    let output = doctor(commands.path(), evidence_root.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(check_state(&result, "cubrid-pr-status"), "failing");
    assert_eq!(check_state(&result, "gh"), "healthy");
    assert_eq!(check_state(&result, "gh-auth"), "healthy");
}

#[test]
fn doctor_reports_missing_gh_independently() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    let evidence_root = tempfile::tempdir().unwrap();

    let output = doctor(commands.path(), evidence_root.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check_state(&result, "cubrid-pr-status"), "healthy");
    assert_eq!(check_state(&result, "gh"), "failing");
    assert_eq!(check_state(&result, "gh-auth"), "failing");
}

#[test]
fn doctor_reports_github_authentication_failure_independently() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(
        commands.path(),
        "gh",
        r#"if [ "$1" = "--version" ]; then exit 0; fi
exit 1"#,
    );
    let evidence_root = tempfile::tempdir().unwrap();

    let output = doctor(commands.path(), evidence_root.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check_state(&result, "gh"), "healthy");
    assert_eq!(check_state(&result, "gh-auth"), "failing");
}

#[test]
fn command_line_configuration_overrides_environment_and_file() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");
    let command_line_root = tempfile::tempdir().unwrap();
    let environment_root = tempfile::tempdir().unwrap();
    let file_root = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "data_dir = {:?}\nartifact_base = \"http://config.invalid\"\n",
            file_root.path().to_str().unwrap()
        ),
    )
    .unwrap();

    let output = doctor(commands.path(), command_line_root.path())
        .env("CUBRID_CI_CONFIG", &config_path)
        .env("CUBRID_CI_DATA_DIR", environment_root.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["data_dir"],
        command_line_root.path().to_str().unwrap()
    );
    assert_eq!(result["data_dir_source"], "command_line");
    assert_eq!(result["artifact_base"], "http://config.invalid/");
    assert_eq!(result["artifact_base_source"], "config_file");
    assert_eq!(check_state(&result, "evidence-server"), "failing");
}

#[test]
fn environment_configuration_overrides_file_without_command_line_value() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");
    let environment_root = tempfile::tempdir().unwrap();
    let file_root = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.toml");
    fs::write(
        &config_path,
        format!("data_dir = {:?}\n", file_root.path().to_str().unwrap()),
    )
    .unwrap();

    let output = doctor_without_data_dir(commands.path())
        .env("CUBRID_CI_CONFIG", &config_path)
        .env("CUBRID_CI_DATA_DIR", environment_root.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["data_dir"],
        environment_root.path().to_str().unwrap()
    );
    assert_eq!(result["data_dir_source"], "environment");
}

#[test]
fn invalid_config_is_a_structured_input_error() {
    let commands = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.toml");
    fs::write(&config_path, "this is not = valid toml [").unwrap();

    let output = doctor_without_data_dir(commands.path())
        .env("CUBRID_CI_CONFIG", &config_path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["kind"], "input");
    assert!(result["error"].as_str().unwrap().contains("invalid config"));
}

#[test]
fn unwritable_evidence_root_is_reported_as_a_failed_check() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");

    let output = doctor(commands.path(), Path::new("/proc/cubrid-ci-data"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check_state(&result, "evidence-root"), "failing");
}

#[test]
fn nonexistent_evidence_root_under_writable_parent_is_a_warning() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");
    let parent = tempfile::tempdir().unwrap();
    let evidence_root = parent.path().join("new/evidence/root");

    let output = doctor(commands.path(), &evidence_root).output().unwrap();

    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check_state(&result, "evidence-root"), "warning");
}

#[test]
fn non_directory_path_component_makes_evidence_root_fail() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");
    let parent = tempfile::tempdir().unwrap();
    let blocker = parent.path().join("blocker");
    fs::write(&blocker, "not a directory").unwrap();

    let output = doctor(commands.path(), &blocker.join("evidence"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check_state(&result, "evidence-root"), "failing");
}

#[test]
fn credential_bearing_artifact_base_is_rejected_without_echoing_secret() {
    let commands = tempfile::tempdir().unwrap();
    let evidence_root = tempfile::tempdir().unwrap();
    let secret = "top-secret-value";

    let output = doctor(commands.path(), evidence_root.path())
        .args([
            "--artifact-base",
            &format!("https://user:{secret}@example.invalid/?token={secret}"),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["kind"], "input");
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_checks_configured_evidence_server() {
    let commands = tempfile::tempdir().unwrap();
    write_command(commands.path(), "cubrid-pr-status", "exit 0");
    write_command(commands.path(), "gh", "exit 0");
    let evidence_root = tempfile::tempdir().unwrap();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let output = doctor(commands.path(), evidence_root.path())
        .args(["--artifact-base", &server.uri()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["artifact_base"], format!("{}/", server.uri()));
    assert_eq!(result["artifact_base_source"], "command_line");
    assert_eq!(check_state(&result, "evidence-server"), "healthy");
}

fn doctor(path: &Path, evidence_root: &Path) -> Command {
    let mut command = doctor_without_data_dir(path);
    command.arg("--data-dir").arg(evidence_root);
    command
}

fn doctor_without_data_dir(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
    command
        .env("PATH", path)
        .env_remove("CUBRID_CI_DATA_DIR")
        .env_remove("CUBRID_CI_ARTIFACT_BASE")
        .env("CUBRID_CI_CONFIG", missing_config_path())
        .args(["doctor", "--json"]);
    command
}

fn check_state<'a>(result: &'a Value, name: &str) -> &'a str {
    result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .unwrap()["state"]
        .as_str()
        .unwrap()
}

fn missing_config_path() -> PathBuf {
    std::env::temp_dir().join("cubrid-ci-doctor-no-config.toml")
}

fn write_command(directory: &Path, name: &str, behavior: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{behavior}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
