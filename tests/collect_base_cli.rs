use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(cache: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(cache)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn baseline_wrapper_records_feature_target_and_missing_ci_without_a_false_pass() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache.git");
    git(&cache, &["init", "--bare"]);
    git(&cache, &["config", "user.name", "CI test"]);
    git(&cache, &["config", "user.email", "ci@example.invalid"]);
    let tree = git(&cache, &["mktree"]);
    let base = git(&cache, &["commit-tree", &tree, "-m", "base"]);
    let target = git(&cache, &["commit-tree", &tree, "-m", "target", "-p", &base]);
    let head = git(&cache, &["commit-tree", &tree, "-m", "head", "-p", &base]);
    let pull = json!({"number":42,"head":{"sha":head,"ref":"feature-test"},"base":{"sha":target,"ref":"feature/oos-merge","repo":{"full_name":"CUBRID/cubrid"}}});
    let commands = root.path().join("bin");
    fs::create_dir(&commands).unwrap();
    let gh = commands.join("gh");
    fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
case "$*" in
 *pulls/42*) printf '%s\n' '{pull}' ;;
 *commits/*/statuses*) printf '%s\n' '[[]]' ;;
 *commits/*/check-runs*) printf '%s\n' '[{{"check_runs":[]}}]' ;;
 *commits/{base}*) printf '%s\n' '{{"sha":"{base}"}}' ;;
 *) exit 64 ;;
esac
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
        .env("PATH", format!("{}:/usr/bin:/bin", commands.display()))
        .env("CUBRID_CI_CONFIG", root.path().join("missing.toml"))
        .env_remove("CUBRID_CI_DATA_DIR")
        .env_remove("CUBRID_CI_ARTIFACT_BASE")
        .args([
            "collect-base",
            "42",
            "--suite",
            "test_sql",
            "--json",
            "--git-cache",
        ])
        .arg(&cache)
        .arg("--data-dir")
        .arg(root.path().join("evidence"))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/command-result-v3.schema.json")).unwrap();
    jsonschema::validator_for(&schema)
        .unwrap()
        .validate(&result)
        .unwrap();
    assert_eq!(result["commit"], base);
    let relationship = &result["selection"]["baseline"];
    assert_eq!(relationship["head_sha"], head);
    assert_eq!(relationship["target_sha"], target);
    assert_eq!(relationship["target_ref"], "feature/oos-merge");
    assert_eq!(relationship["merge_base"], base);
    assert_eq!(result["suites"]["test_sql"]["state"], "not_observed");
    assert_eq!(result["suites"].as_object().unwrap().len(), 1);
    assert_eq!(result["ok"], false);
    assert!(!cache.join("FETCH_HEAD").exists()); // all pinned objects were already local
}
