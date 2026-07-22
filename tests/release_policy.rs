use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

#[test]
fn clean_repository_is_accepted() {
    let repo = initialized_repository();
    let output = run_check(repo.path(), None);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim().len(), 40);
}

#[test]
fn unstaged_change_is_rejected() {
    let repo = initialized_repository();
    fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();

    assert_dirty_failure(run_check(repo.path(), None), "tracked.txt");
}

#[test]
fn staged_change_is_rejected() {
    let repo = initialized_repository();
    fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
    git(repo.path(), &["add", "tracked.txt"]);

    assert_dirty_failure(run_check(repo.path(), None), "tracked.txt");
}

#[test]
fn untracked_file_is_rejected() {
    let repo = initialized_repository();
    fs::write(repo.path().join("untracked.txt"), "new\n").unwrap();

    assert_dirty_failure(run_check(repo.path(), None), "untracked.txt");
}

#[test]
fn mismatched_commit_override_is_rejected() {
    let repo = initialized_repository();
    let output = run_check(
        repo.path(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    );

    assert!(!output.status.success());
    assert!(stderr(&output).contains("must exactly match release HEAD"));
}

fn initialized_repository() -> TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "--quiet"]);
    git(repo.path(), &["config", "user.name", "Release Policy Test"]);
    git(
        repo.path(),
        &["config", "user.email", "release-policy@example.invalid"],
    );
    fs::write(repo.path().join("tracked.txt"), "original\n").unwrap();
    git(repo.path(), &["add", "tracked.txt"]);
    git(repo.path(), &["commit", "--quiet", "-m", "initial"]);
    repo
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

fn run_check(repo: &Path, commit_override: Option<&str>) -> Output {
    let mut command = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/check-release-source.sh"
    ));
    command.arg(repo).env_remove("CUBRID_CI_BUILD_GIT_SHA");
    if let Some(commit) = commit_override {
        command.env("CUBRID_CI_BUILD_GIT_SHA", commit);
    }
    command.output().unwrap()
}

fn assert_dirty_failure(output: Output, path: &str) {
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("require a clean Git working tree"));
    assert!(stderr.contains(path));
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
