use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMMIT_ENV: &str = "CUBRID_CI_BUILD_GIT_SHA";

fn main() {
    println!("cargo:rerun-if-env-changed={COMMIT_ENV}");
    watch_git_path("HEAD");
    watch_git_path("packed-refs");

    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&head_ref);
    }

    let commit = env::var(COMMIT_ENV)
        .ok()
        .filter(|value| is_commit(value))
        .or_else(|| git_output(&["rev-parse", "--verify", "HEAD"]).filter(|value| is_commit(value)))
        .map(|value| value.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env={COMMIT_ENV}={commit}");
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest_dir())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
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

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn is_commit(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 7 && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
