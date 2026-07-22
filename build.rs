use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMMIT_ENV: &str = "CUBRID_CI_BUILD_GIT_SHA";
const PROFILE_ENV: &str = "CUBRID_CI_BUILD_PROFILE";

fn main() {
    println!("cargo:rerun-if-env-changed={COMMIT_ENV}");
    watch_git_path("HEAD");
    watch_git_path("index");
    watch_git_path("packed-refs");
    watch_tracked_files();

    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&head_ref);
    }

    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let supplied_commit = env::var(COMMIT_ENV).ok().filter(|value| is_commit(value));

    let commit = if profile == "release" {
        require_clean_release(supplied_commit.as_deref())
    } else {
        supplied_commit
            .or_else(|| {
                git_output(&["rev-parse", "--verify", "HEAD"]).filter(|value| is_commit(value))
            })
            .unwrap_or_else(|| "unknown".to_owned())
    };
    let commit = commit.chars().take(12).collect::<String>();

    println!("cargo:rustc-env={COMMIT_ENV}={commit}");
    println!("cargo:rustc-env={PROFILE_ENV}={profile}");
}

fn require_clean_release(supplied_commit: Option<&str>) -> String {
    let inside_work_tree = git_output(&["rev-parse", "--is-inside-work-tree"]);
    if inside_work_tree.as_deref() != Some("true") {
        panic!("release builds require a Git working tree");
    }

    let head = git_output(&["rev-parse", "--verify", "HEAD^{commit}"])
        .filter(|value| is_commit(value))
        .unwrap_or_else(|| panic!("release builds require a valid Git HEAD commit"));

    if let Some(supplied) = supplied_commit
        && !supplied.eq_ignore_ascii_case(&head)
    {
        panic!("{COMMIT_ENV} ({supplied}) must exactly match the release HEAD ({head})");
    }

    let status = git_output_allow_empty(&[
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignore-submodules=none",
    ])
    .unwrap_or_else(|| panic!("failed to inspect the Git working tree for a release build"));
    if !status.is_empty() {
        panic!("release builds require a clean Git working tree:\n{status}");
    }

    head
}

fn git_output(args: &[&str]) -> Option<String> {
    git_output_allow_empty(args).filter(|value| !value.is_empty())
}

fn git_output_allow_empty(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest_dir())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    Some(value.trim().to_owned())
}

fn watch_git_path(path: &str) {
    let Some(path) = git_output(&["rev-parse", "--git-path", path]) else {
        return;
    };
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        manifest_dir().join(path)
    };
    println!("cargo:rerun-if-changed={}", path.display());
}

fn watch_tracked_files() {
    let Some(files) = git_output(&["ls-files", "--cached"]) else {
        return;
    };
    for file in files.lines() {
        println!("cargo:rerun-if-changed={file}");
    }
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn is_commit(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 7 && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
