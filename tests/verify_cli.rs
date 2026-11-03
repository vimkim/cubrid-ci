use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use cubrid_ci::diff::{extract_diff, stable_test_id};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "1111111111111111111111111111111111111111";
const SQL_SUMMARY: &str = "providers/github-actions/runs/123/attempts/2/test_sql/summary.json";
const FAILED_CASE: &str = "sql/bugs/cases/bug_123.sql";
const OTHER_ID: &str = "sql-bugs-cases-bug_123.sql";
const OTHER_DIFF: &str = "--- answer\n+++ actual\n-old\n+new\n";

#[tokio::test(flavor = "multi_thread")]
async fn complete_bundle_verifies_offline_without_writing() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let requests = fixture.server.received_requests().await.unwrap().len();
    let before = tree(fixture.data.path());

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(0), "{report:#}");
    assert_eq!(report["ok"], true);
    assert_eq!(
        report["assessment"],
        json!({
            "identity": "established",
            "observation": "complete",
            "manifest": "validated",
            "requested_summaries": "validated",
            "result_matches_observation": true,
            "requested_suites_reconciled": true
        })
    );
    assert_eq!(report["baseline"], Value::Null);
    assert_eq!(report["suites"]["test_sql"]["summary"], "validated");
    assert_eq!(report["suites"]["test_sql"]["path"], SQL_SUMMARY);
    assert_eq!(report["output_dir"], result["output_dir"]);
    assert_eq!(report["issues"], json!([]));
    assert_eq!(
        fixture.server.received_requests().await.unwrap().len(),
        requests
    );
    assert_eq!(tree(fixture.data.path()), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn modified_raw_evidence_is_an_integrity_failure() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let junit = find(&fixture.suite_dir(&result).join("raw"), "results.xml");
    append(&junit, "<!-- edited -->");

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["assessment"]["identity"], "established");
    assert_eq!(report["assessment"]["observation"], "complete");
    assert_eq!(report["assessment"]["requested_summaries"], "invalid");
    assert_eq!(report["suites"]["test_sql"]["summary"], "invalid");
    assert_issue(&report, "test_sql", "integrity", "disagrees with its index");
}

#[tokio::test(flavor = "multi_thread")]
async fn rewritten_summary_must_still_reconcile_its_counts() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let suite = fixture.suite_dir(&result);
    rewrite_json(&suite.join("summary.json"), |summary| {
        summary["counts"]["passed"] = json!(1);
    });
    reindex_summary(&suite);

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "counts do not reconcile");
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_files_must_exist_and_match_the_summary() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let failure = find(&fixture.suite_dir(&result).join("failures"), "message.txt")
        .parent()
        .unwrap()
        .to_owned();
    let message = fs::read(failure.join("message.txt")).unwrap();
    fs::remove_file(failure.join("message.txt")).unwrap();

    let (code, report) = fixture.verify(&result, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "message.txt");

    fs::write(failure.join("message.txt"), b"edited message").unwrap();
    let (code, report) = fixture.verify(&result, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "retained JUnit record");

    fs::write(failure.join("message.txt"), message).unwrap();
    rewrite_json(&failure.join("metadata.json"), |metadata| {
        metadata["result"] = json!("error");
    });
    let (code, report) = fixture.verify(&result, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "metadata disagrees");
}

#[tokio::test(flavor = "multi_thread")]
async fn rewritten_verdict_must_agree_with_ci_state_and_recorded_failures() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let suite = fixture.suite_dir(&result);
    let summary = fs::read(suite.join("summary.json")).unwrap();

    for (field, value, fragment) in [
        (
            "verdict",
            "pass",
            "verdict pass and CI state failure disagree",
        ),
        (
            "ci_state",
            "success",
            "verdict fail and CI state success disagree",
        ),
    ] {
        rewrite_json(&suite.join("summary.json"), |summary| {
            summary[field] = json!(value);
        });
        reindex_summary(&suite);

        let (code, report) = fixture.verify(&result, &[]);

        assert_eq!(code, Some(4), "{field}: {report:#}");
        assert_issue(&report, "test_sql", "integrity", fragment);
        fs::write(suite.join("summary.json"), &summary).unwrap();
        reindex_summary(&suite);
    }

    rewrite_json(&suite.join("summary.json"), |summary| {
        summary["verdict"] = json!("pass");
        summary["ci_state"] = json!("success");
    });
    reindex_summary(&suite);

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(
        &report,
        "test_sql",
        "integrity",
        "disagrees with its 1 recorded",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_failures_must_match_the_retained_failed_list_and_junit() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let suite = fixture.suite_dir(&result);
    forget_recorded_failure(&suite);

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["suites"]["test_sql"]["summary"], "invalid");
    assert_issue(
        &report,
        "test_sql",
        "integrity",
        "retained failed-case list",
    );

    let raw = suite.join("raw");
    fs::write(raw.join("collect/failed.list"), b"").unwrap();
    reindex_raw(&raw, "collect/failed.list");

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "retained JUnit failures");
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_files_from_another_derivation_verify_offline() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    rederive_failure_files(&fixture.suite_dir(&result));

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(0), "{report:#}");
    assert_eq!(report["suites"]["test_sql"]["summary"], "validated");
    assert_eq!(report["issues"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_files_from_another_derivation_are_reused_without_refetching() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    rederive_failure_files(&fixture.suite_dir(&result));
    let requests = fixture.server.received_requests().await.unwrap().len();

    let output = fixture.collect();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let reused: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reused["suites"]["test_sql"]["state"], "completed");
    assert_eq!(
        fixture.server.received_requests().await.unwrap().len(),
        requests
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rederived_failure_files_still_reject_tampering() {
    type Tamper = fn(&Path);
    let cases: [(&str, Tamper, &str); 6] = [
        (
            "an invented diff line",
            |suite| append(&rederived_dir(suite).join("diff.txt"), "+invented\n"),
            "retained JUnit record",
        ),
        (
            "a deleted message",
            |suite| fs::remove_file(rederived_dir(suite).join("message.txt")).unwrap(),
            "message.txt",
        ),
        (
            "an edited message",
            |suite| fs::write(rederived_dir(suite).join("message.txt"), b"edited").unwrap(),
            "retained JUnit record",
        ),
        (
            "edited metadata",
            |suite| {
                rewrite_json(&rederived_dir(suite).join("metadata.json"), |metadata| {
                    metadata["result"] = json!("error");
                });
            },
            "metadata disagrees",
        ),
        (
            "a deleted declared diff",
            |suite| fs::remove_file(rederived_dir(suite).join("diff.txt")).unwrap(),
            "diff.txt",
        ),
        (
            "a parent-directory id",
            |suite| {
                rewrite_json(&suite.join("summary.json"), |summary| {
                    point_record_at(&mut summary["failures"][0], "..");
                });
                reindex_summary(suite);
            },
            "stored failure record",
        ),
    ];
    for (label, tamper, fragment) in cases {
        let fixture = SqlFixture::new().await;
        let result = fixture.collect_ok();
        let suite = fixture.suite_dir(&result);
        rederive_failure_files(&suite);
        tamper(&suite);

        let (code, report) = fixture.verify(&result, &[]);
        assert_eq!(code, Some(4), "{label}: {report:#}");
        assert_issue(&report, "test_sql", "integrity", fragment);

        let output = fixture.collect();
        assert_eq!(
            output.status.code(),
            Some(4),
            "{label}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn symlinked_evidence_is_rejected_even_with_identical_bytes() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let junit = find(&fixture.suite_dir(&result).join("raw"), "results.xml");
    let outside = fixture.commands.path().join("results.xml");
    fs::copy(&junit, &outside).unwrap();
    fs::remove_file(&junit).unwrap();
    std::os::unix::fs::symlink(&outside, &junit).unwrap();

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "symbolic link");
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_indexed_and_declared_paths_are_rejected() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let suite = fixture.suite_dir(&result);
    let original = fs::read(suite.join("raw/index.json")).unwrap();
    rewrite_json(&suite.join("raw/index.json"), |index| {
        index["files"].as_array_mut().unwrap().push(json!({
            "path": "../summary.json",
            "size_bytes": 1,
            "sha256": "0".repeat(64)
        }));
    });
    let (code, report) = fixture.verify(&result, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "unsafe or repeated path");
    fs::write(suite.join("raw/index.json"), original).unwrap();

    let escaped =
        "providers/github-actions/runs/123/attempts/2/test_sql/../../../../../../summary.json";
    let mut forged = result.clone();
    forged["suites"]["test_sql"]["summary"] = json!(escaped);
    let observation = only_observation(&fixture.snapshot(&result));
    rewrite_json(&observation.join("result.json"), |observed| {
        observed["command"]["suites"]["test_sql"]["summary"] = json!(escaped);
    });
    fs::write(
        fixture.snapshot(&result).join("manifest.json"),
        serde_json::to_vec_pretty(&forged).unwrap(),
    )
    .unwrap();
    let (code, report) = fixture.verify(&forged, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "test_sql", "integrity", "derived path");
}

#[tokio::test(flavor = "multi_thread")]
async fn pins_and_requested_suites_must_match_the_observation() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let other = "2222222222222222222222222222222222222222";

    let (code, report) = fixture.verify_with(&result, &["--pr", "7990", "--pr-head", other]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["assessment"]["identity"], "unknown");
    assert_issue(&report, "identity", "integrity", "pinned PR head");

    let (code, report) = fixture.verify_with(&result, &["--pr", "8000", "--pr-head", SHA]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(&report, "identity", "integrity", "pinned PR");

    let (code, report) = fixture.verify_with(
        &result,
        &["--pr", "7990", "--pr-head", SHA, "--suite", "test_medium"],
    );
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["requested_suites_reconciled"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn unmatched_result_is_unvalidated_but_keeps_its_identity() {
    let fixture = SqlFixture::new().await;
    let mut result = fixture.collect_ok();
    result["collected_at"] = json!("2026-01-01T00:00:00Z");

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["identity"], "established");
    assert_eq!(report["assessment"]["observation"], "unvalidated");
    assert_eq!(report["observation_dir"], Value::Null);
    assert_issue(
        &report,
        "observation",
        "unavailable",
        "no collection observation",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_observation_request_is_an_integrity_failure() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let observation = only_observation(&fixture.snapshot(&result));
    rewrite_json(&observation.join("request.json"), |request| {
        request.as_object_mut().unwrap().remove("started_at");
    });

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "unvalidated");
    assert_issue(&report, "observation", "integrity", "schema");
}

#[tokio::test(flavor = "multi_thread")]
async fn later_manifests_are_accepted_superseded_or_rejected_by_agreement() {
    let fixture = SqlFixture::new().await;
    let first = fixture.collect_ok();
    let equivalent = fixture.collect_ok();
    assert_ne!(first["collected_at"], equivalent["collected_at"]);

    let (code, report) = fixture.verify(&first, &[]);
    assert_eq!(code, Some(0), "{report:#}");
    assert_eq!(report["assessment"]["manifest"], "validated");

    fixture.write_status("PENDING");
    let output = fixture.collect();
    assert_eq!(output.status.code(), Some(3));
    let running: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(running["suites"]["test_sql"]["state"], "running");

    let (code, report) = fixture.verify(&first, &[]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["manifest"], "invalid");
    assert_eq!(report["suites"]["test_sql"]["summary"], "validated");
    assert_issue(
        &report,
        "manifest",
        "unavailable",
        "replaced by observation",
    );

    let (code, report) = fixture.verify(&running, &[]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "incomplete");
    assert_eq!(report["assessment"]["manifest"], "validated");
    assert_eq!(report["assessment"]["requested_summaries"], "none");
    assert_issue(&report, "test_sql", "unavailable", "running");

    rewrite_json(
        &fixture.snapshot(&running).join("manifest.json"),
        |manifest| manifest["suites"]["test_sql"]["state"] = json!("completed"),
    );
    let (code, report) = fixture.verify(&running, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["assessment"]["manifest"], "invalid");
    assert_issue(
        &report,
        "manifest",
        "integrity",
        "every retained observation",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_observation_is_located_by_pins_and_start_time() {
    let fixture = SqlFixture::new().await;
    fixture.write_gh("2222222222222222222222222222222222222222");
    let since = now();
    let output = fixture.collect();
    assert_eq!(output.status.code(), Some(4));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["kind"], "integrity");

    let (code, report) = fixture.verify_with(&error, &["--pr", "7990", "--pr-head", SHA]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["identity"], "unknown");
    assert_eq!(report["assessment"]["observation"], "unvalidated");

    let (code, report) = fixture.verify_with(
        &error,
        &["--pr", "7990", "--pr-head", SHA, "--since", &since],
    );
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(
        report["assessment"],
        json!({
            "identity": "established",
            "observation": "failed",
            "manifest": "missing",
            "requested_summaries": "none",
            "result_matches_observation": false,
            "requested_suites_reconciled": true
        })
    );
    assert!(report["observation_dir"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_observation_is_selected_only_from_this_invocation() {
    let fixture = SqlFixture::new().await;
    write_command(
        fixture.commands.path(),
        "gh",
        r#"while kill -0 "$PPID" 2>/dev/null
do
  /bin/sleep 0.01
done
exit 1"#,
    );
    let since = now();
    let mut child = fixture
        .collect_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let observations = fixture.data.path().join(format!(
        "github-actions/CUBRID-cubrid/pr-7990/{SHA}/observations"
    ));
    let published = (0..500).any(|_| {
        let found = fs::read_dir(&observations).is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("request.json").is_file())
        });
        if !found {
            std::thread::sleep(Duration::from_millis(10));
        }
        found
    });
    assert!(published, "collector did not publish its request");
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.is_empty());

    let (code, report) =
        fixture.verify_raw(b"", &["--pr", "7990", "--pr-head", SHA, "--since", &since]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["identity"], "established");
    assert_eq!(report["assessment"]["observation"], "interrupted");
    assert_eq!(report["assessment"]["requested_summaries"], "none");
    assert_eq!(report["suites"], json!({}));

    let later = now();
    let (code, report) =
        fixture.verify_raw(b"", &["--pr", "7990", "--pr-head", SHA, "--since", &later]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "unvalidated");
    assert_eq!(report["assessment"]["identity"], "unknown");
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_lineage_and_carried_verdicts_verify_offline() {
    let fixture = RerunFixture::new().await;
    let result = fixture.collect(&[]);
    assert_eq!(result.0, Some(0), "{:#}", result.1);
    let result = result.1;

    let (code, report) = verify(&fixture.data, &result, &["--pr", "7990"]);

    assert_eq!(code, Some(0), "{report:#}");
    assert_eq!(
        report["suites"]["test_shell"]["rerun_sources"],
        json!([{
            "run_id": 40323508581_u64,
            "attempt": 1,
            "mode": "partial",
            "path": "providers/github-actions/runs/40323508581/attempts/1/test_shell/summary.json"
        }])
    );
    for suite in ["test_medium", "test_sql"] {
        assert_eq!(
            report["suites"][suite]["rerun_sources"][0]["mode"],
            "carried_forward"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_rerun_source_invalidates_only_its_suite() {
    let fixture = RerunFixture::new().await;
    let (_, result) = fixture.collect(&[]);
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let source = root.join("providers/github-actions/runs/40323508581/attempts/1/test_shell");
    rewrite_json(&source.join("summary.json"), |summary| {
        summary["verdict"] = json!("pass");
    });
    reindex_summary(&source);

    let (code, report) = verify(&fixture.data, &result, &["--pr", "7990"]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["suites"]["test_shell"]["summary"], "invalid");
    assert_eq!(report["suites"]["test_medium"]["summary"], "validated");
    assert_eq!(report["suites"]["test_sql"]["summary"], "validated");
    assert_issue(
        &report,
        "test_shell",
        "integrity",
        "rerun source run 40323508581 attempt 1",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_in_a_rerun_source_is_an_integrity_failure() {
    let fixture = RerunFixture::new().await;
    let (_, result) = fixture.collect(&[]);
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let source = root.join("providers/github-actions/runs/40323508581/attempts/1/test_shell");
    append(&source.join("summary.json"), "x");

    let (code, report) = verify(&fixture.data, &result, &["--pr", "7990"]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_issue(
        &report,
        "test_shell",
        "integrity",
        "rerun source run 40323508581 attempt 1",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn carried_forward_summary_must_keep_the_source_verdict() {
    let fixture = RerunFixture::new().await;
    let (_, result) = fixture.collect(&[]);
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let carried = root.join(result["suites"]["test_medium"]["summary"].as_str().unwrap());
    rewrite_json(&carried, |summary| summary["verdict"] = json!("fail"));
    reindex_summary(carried.parent().unwrap());

    let (code, report) = verify(&fixture.data, &result, &["--pr", "7990"]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["suites"]["test_medium"]["summary"], "invalid");
    assert_issue(&report, "test_medium", "integrity", "carried-forward");
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_source_that_changed_after_the_rerun_started_is_unavailable() {
    let fixture = RerunFixture::new().await;
    let (_, result) = fixture.collect(&["--suite", "test_shell"]);
    let root = PathBuf::from(result["output_dir"].as_str().unwrap());
    let child = root.join(result["suites"]["test_shell"]["summary"].as_str().unwrap());
    let raw = child.parent().unwrap().join("raw");
    rewrite_json(&raw.join("github/rerun-source.json"), |source| {
        source["updated_at"] = json!("2099-01-01T00:00:00Z");
    });
    reindex_raw(&raw, "github/rerun-source.json");

    let (code, report) = verify(&fixture.data, &result, &["--suite", "test_shell"]);

    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["suites"]["test_shell"]["summary"], "invalid");
    assert_eq!(report["assessment"]["requested_summaries"], "invalid");
    assert_issue(
        &report,
        "test_shell",
        "unavailable",
        "its source attempt cannot be established",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_budget_must_reconcile_with_the_request() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let observation = only_observation(&fixture.snapshot(&result));
    rewrite_json(&observation.join("result.json"), |observed| {
        observed["binary_budget"]["consumed_bytes"] = json!(1);
    });

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "unvalidated");
    assert_issue(
        &report,
        "observation",
        "integrity",
        "binary transfer budget",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incomplete_observation_reports_validated_suites_separately() {
    let fixture = RerunFixture::new().await;
    Mock::given(method("GET"))
        .and(path(
            "/runs/40323508581/sql/shard/00/test-results/results.xml",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("<testsuite tests=\"0\"/>"))
        .with_priority(1)
        .mount(&fixture.server)
        .await;
    let (code, result) = fixture.collect(&[]);
    assert_eq!(code, Some(4), "{result:#}");

    let (code, report) = verify(&fixture.data, &result, &["--pr", "7990"]);

    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "incomplete");
    assert_eq!(report["assessment"]["manifest"], "validated");
    assert_eq!(report["assessment"]["requested_summaries"], "partial");
    assert_eq!(report["suites"]["test_sql"]["state"], "collection_failed");
    assert_eq!(report["suites"]["test_sql"]["summary"], "absent");
    assert_eq!(report["suites"]["test_shell"]["summary"], "validated");
    assert_issue(&report, "test_sql", "unavailable", "collection_failed");
    let named = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["scope"] == "test_shell")
        .count();
    assert_eq!(named, 0, "{report:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_baseline_is_validated_against_the_pinned_head() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache.git");
    git(&cache, &["init", "--bare"]);
    git(&cache, &["config", "user.name", "CI test"]);
    git(&cache, &["config", "user.email", "ci@example.invalid"]);
    let tree = git(&cache, &["mktree"]);
    let base = git(&cache, &["commit-tree", &tree, "-m", "base"]);
    let target = git(&cache, &["commit-tree", &tree, "-m", "target", "-p", &base]);
    let head = git(&cache, &["commit-tree", &tree, "-m", "head", "-p", &base]);
    let server = MockServer::start().await;
    mount_sql_evidence(&server, &base).await;
    let commands = root.path().join("bin");
    fs::create_dir(&commands).unwrap();
    let pull = json!({"number":42,"head":{"sha":head,"ref":"feature-test"},"base":{"sha":target,"ref":"develop","repo":{"full_name":"CUBRID/cubrid"}}});
    let run = json!({"id":123,"run_attempt":2,"status":"completed","conclusion":"failure","event":"workflow_dispatch","head_sha":base,"path":".github/workflows/gha-ci.yml","repository":{"full_name":"CUBRID/cubrid"},"created_at":"2026-10-06T00:00:00Z","updated_at":"2026-10-06T00:00:00Z"});
    write_command(
        &commands,
        "gh",
        &format!(
            r#"case "$*" in
  *pulls/42*) printf '%s\n' '{pull}' ;;
  *commits/{base}*) printf '%s\n' '{{"sha":"{base}"}}' ;;
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":900,"name":"collect","run_attempt":2}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {uri}' ;;
  *actions/runs/123*) printf '%s\n' '{run}' ;;
  *) exit 64 ;;
esac"#,
            uri = server.uri()
        ),
    );
    let data = root.path().join("evidence");
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
            "--run-id",
            "123",
        ])
        .args(["--json", "--git-cache"])
        .arg(&cache)
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let evidence = tempfile::tempdir().unwrap();
    let pins = ["--pr", "42", "--suite", "test_sql", "--pr-head"];

    let (code, report) = verify_in(
        evidence.path(),
        &data,
        &result,
        &[&pins[..], &[&head]].concat(),
    );
    assert_eq!(code, Some(0), "{report:#}");
    assert_eq!(report["baseline"], "validated_exact");

    let (code, report) = verify_in(
        evidence.path(),
        &data,
        &result,
        &[&pins[..], &[&target]].concat(),
    );
    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["baseline"], "invalid");

    let (code, report) = verify_in(evidence.path(), &data, &result, &["--suite", "test_sql"]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["baseline"], "unavailable");
    assert_eq!(report["assessment"]["identity"], "unknown", "{report:#}");
    assert_issue(&report, "identity", "unavailable", "--pr and --pr-head");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_result_flags_must_agree_with_suite_states() {
    let fixture = SqlFixture::new().await;
    let mut result = fixture.collect_ok();
    result["errors"] = json!([{
        "suite": "test_sql",
        "kind": "integrity",
        "diagnostic": "identity_or_integrity",
        "message": "edited"
    }]);
    let snapshot = fixture.snapshot(&result);
    rewrite_json(
        &only_observation(&snapshot).join("result.json"),
        |observed| observed["command"] = result.clone(),
    );
    fs::write(
        snapshot.join("manifest.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();

    let (code, report) = fixture.verify(&result, &[]);

    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["assessment"]["identity"], "unknown");
    assert_issue(&report, "identity", "integrity", "ok flag disagrees");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_manifest_and_unreadable_observations_are_reported() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let snapshot = fixture.snapshot(&result);
    fs::remove_file(snapshot.join("manifest.json")).unwrap();

    let (code, report) = fixture.verify(&result, &[]);
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["manifest"], "missing");
    assert_eq!(report["assessment"]["requested_summaries"], "validated");

    let observed = only_observation(&snapshot).join("result.json");
    let bytes = fs::read(&observed).unwrap();
    fs::write(&observed, &bytes[..bytes.len() / 2]).unwrap();
    let (code, report) = fixture.verify(&result, &[]);
    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "unvalidated");
    assert_issue(
        &report,
        "observation",
        "integrity",
        "unreadable or malformed",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn several_matching_observations_are_ambiguous() {
    let fixture = SqlFixture::new().await;
    fixture.write_gh("2222222222222222222222222222222222222222");
    let since = now();
    for _ in 0..2 {
        assert_eq!(fixture.collect().status.code(), Some(4));
    }

    let (code, report) =
        fixture.verify_raw(b"", &["--pr", "7990", "--pr-head", SHA, "--since", &since]);

    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["assessment"]["observation"], "unvalidated");
    assert_issue(&report, "observation", "unavailable", "ambiguous");
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_evidence_is_a_storage_failure() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let junit = find(&fixture.suite_dir(&result).join("raw"), "results.xml");
    fs::set_permissions(&junit, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&junit).is_ok() {
        return;
    }

    let (code, report) = fixture.verify(&result, &[]);

    fs::set_permissions(&junit, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(code, Some(6), "{report:#}");
    assert_eq!(report["suites"]["test_sql"]["summary"], "invalid");
    assert_issue(&report, "test_sql", "storage", "results.xml");
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_observation_files_are_storage_failures() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let snapshot = fixture.snapshot(&result);
    let observation = only_observation(&snapshot);
    for name in ["request.json", "result.json"] {
        let file = observation.join(name);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            return;
        }

        let (code, report) = fixture.verify(&result, &[]);

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(code, Some(6), "{name}: {report:#}");
        assert_issue(&report, "observation", "storage", name);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_observations_directory_is_a_storage_failure() {
    let fixture = SqlFixture::new().await;
    let result = fixture.collect_ok();
    let observations = fixture.snapshot(&result).join("observations");
    fs::set_permissions(&observations, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&observations).is_ok() {
        return;
    }

    let (code, report) = fixture.verify(&result, &[]);

    fs::set_permissions(&observations, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, Some(6), "{report:#}");
    assert_issue(&report, "observation", "storage", "observations");
}

#[test]
fn unreadable_result_path_is_an_input_error() {
    let data = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
        .env("PATH", data.path())
        .args(["verify", "--json"])
        .arg(data.path().join("missing.json"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["kind"], "input");
}

struct SqlFixture {
    server: MockServer,
    commands: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl SqlFixture {
    async fn new() -> Self {
        let server = MockServer::start().await;
        mount_sql_evidence(&server, SHA).await;
        let commands = tempfile::tempdir().unwrap();
        write_command(
            commands.path(),
            "cubrid-pr-status",
            &format!("/bin/cat <<'JSON'\n{}\nJSON", status_snapshot()),
        );
        let fixture = Self {
            server,
            commands,
            data: tempfile::tempdir().unwrap(),
        };
        fixture.write_gh(SHA);
        fixture
    }

    fn write_status(&self, state: &str) {
        let status = status_snapshot().replace("\"FAILURE\"", &format!("\"{state}\""));
        write_command(
            self.commands.path(),
            "cubrid-pr-status",
            &format!("/bin/cat <<'JSON'\n{status}\nJSON"),
        );
    }

    fn write_gh(&self, published_head: &str) {
        write_command(
            self.commands.path(),
            "gh",
            &format!(
                r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"{published_head}"}}}}' ;;
  *actions/runs/123/jobs*) printf '%s\n' '{{"jobs":[{{"id":900,"name":"collect","run_attempt":2}}]}}' ;;
  *actions/jobs/900/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {}' ;;
  *actions/runs/123*) printf '%s\n' '{{"id":123,"run_attempt":2}}' ;;
  *) exit 64 ;;
esac"#,
                self.server.uri()
            ),
        );
    }

    fn collect_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"));
        command
            .env("PATH", self.commands.path())
            .env("CUBRID_CI_CONFIG", self.data.path().join("missing.toml"))
            .env_remove("CUBRID_CI_DATA_DIR")
            .env_remove("CUBRID_CI_ARTIFACT_BASE")
            .args(["collect", "7990", "--commit", SHA, "--suite", "test_sql"])
            .args(["--json", "--data-dir"])
            .arg(self.data.path());
        command
    }

    fn collect(&self) -> std::process::Output {
        self.collect_command().output().unwrap()
    }

    fn collect_ok(&self) -> Value {
        let output = self.collect();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn snapshot(&self, result: &Value) -> PathBuf {
        PathBuf::from(result["output_dir"].as_str().unwrap())
    }

    fn suite_dir(&self, result: &Value) -> PathBuf {
        self.snapshot(result)
            .join(SQL_SUMMARY)
            .parent()
            .unwrap()
            .to_owned()
    }

    fn verify(&self, result: &Value, extra: &[&str]) -> (Option<i32>, Value) {
        self.verify_with(
            result,
            &[&["--pr", "7990", "--pr-head", SHA], extra].concat(),
        )
    }

    fn verify_with(&self, result: &Value, args: &[&str]) -> (Option<i32>, Value) {
        self.verify_raw(&serde_json::to_vec_pretty(result).unwrap(), args)
    }

    fn verify_raw(&self, result: &[u8], args: &[&str]) -> (Option<i32>, Value) {
        let path = self.commands.path().join("result.json");
        fs::write(&path, result).unwrap();
        run_verify(
            self.commands.path(),
            self.data.path(),
            &path,
            &[args, &["--suite", "test_sql"]].concat(),
        )
    }
}

struct RerunFixture {
    server: MockServer,
    commands: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl RerunFixture {
    async fn new() -> Self {
        const RUN: u64 = 40334404336;
        let server = MockServer::start().await;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr7990-rerun");
        for (run, prefix) in [(RUN, ""), (40323508581, "source")] {
            for suite in ["medium", "sql", "shell"] {
                let suite_fixture = fixture.join(prefix).join(suite);
                for entry in WalkDir::new(&suite_fixture) {
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
                r#"case "$*" in
  *pulls/7990*) printf '%s\n' '{{"number":7990,"head":{{"sha":"52987c654b6f04734cad01c11c80c7f30358b33c"}}}}' ;;
  *actions/runs/{RUN}/jobs*) /bin/cat '{fixture}/jobs.json' ;;
  *actions/runs/40323508581/jobs*) /bin/cat '{fixture}/source/jobs.json' ;;
  *actions/jobs/121614144713/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {base}' 'RERUN_FROM: 40323508581' ;;
  *actions/jobs/121594127253/logs*) printf '%s\n' 'ARTIFACT_URL_BASE: {base}' 'RERUN_FROM: ' ;;
  *actions/runs/{RUN}*) /bin/cat '{fixture}/run.json' ;;
  *actions/runs/40323508581*) /bin/cat '{fixture}/source/run.json' ;;
  *) exit 64 ;;
esac"#,
                fixture = fixture.display(),
                base = server.uri()
            ),
        );
        Self {
            server,
            commands,
            data: tempfile::tempdir().unwrap(),
        }
    }

    fn collect(&self, extra: &[&str]) -> (Option<i32>, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
            .env("PATH", self.commands.path())
            .env("CUBRID_CI_CONFIG", self.data.path().join("missing.toml"))
            .env_remove("CUBRID_CI_DATA_DIR")
            .env_remove("CUBRID_CI_ARTIFACT_BASE")
            .args(["collect", "7990", "--commit"])
            .arg("52987c654b6f04734cad01c11c80c7f30358b33c")
            .args(["--json", "--data-dir"])
            .arg(self.data.path())
            .args(extra)
            .output()
            .unwrap();
        (
            output.status.code(),
            serde_json::from_slice(&output.stdout).unwrap(),
        )
    }
}

fn verify(data: &tempfile::TempDir, result: &Value, args: &[&str]) -> (Option<i32>, Value) {
    let scratch = tempfile::tempdir().unwrap();
    verify_in(scratch.path(), data.path(), result, args)
}

fn verify_in(scratch: &Path, data: &Path, result: &Value, args: &[&str]) -> (Option<i32>, Value) {
    let path = scratch.join("result.json");
    fs::write(&path, serde_json::to_vec_pretty(result).unwrap()).unwrap();
    run_verify(scratch, data, &path, args)
}

fn run_verify(
    empty_path: &Path,
    data: &Path,
    result: &Path,
    args: &[&str],
) -> (Option<i32>, Value) {
    let offline = empty_path.join("offline-bin");
    fs::create_dir_all(&offline).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
        .env("PATH", &offline)
        .env("CUBRID_CI_CONFIG", data.join("missing.toml"))
        .env_remove("CUBRID_CI_DATA_DIR")
        .args(["verify", "--json", "--data-dir"])
        .arg(data)
        .arg(result)
        .args(args)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    validate_report(&report);
    (output.status.code(), report)
}

fn validate_report(report: &Value) {
    let schema: Value =
        serde_json::from_str(include_str!("../schema/verification-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    if let Err(error) = validator.validate(report) {
        panic!("verification report violates its schema: {error}\n{report:#}");
    }
}

fn assert_issue(report: &Value, scope: &str, kind: &str, fragment: &str) {
    let found = report["issues"].as_array().unwrap().iter().any(|issue| {
        issue["scope"] == scope
            && issue["kind"] == kind
            && issue["message"].as_str().unwrap().contains(fragment)
    });
    assert!(
        found,
        "no {kind} issue for {scope} containing {fragment:?}: {report:#}"
    );
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| (entry.path().to_owned(), fs::read(entry.path()).unwrap()))
        .collect()
}

fn find(root: &Path, name: &str) -> PathBuf {
    WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == name)
        .unwrap_or_else(|| panic!("{name} not under {}", root.display()))
        .into_path()
}

fn only_observation(snapshot: &Path) -> PathBuf {
    let entries = fs::read_dir(snapshot.join("observations"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    entries.into_iter().next().unwrap()
}

fn append(path: &Path, text: &str) {
    let mut bytes = fs::read(path).unwrap();
    bytes.extend_from_slice(text.as_bytes());
    fs::write(path, bytes).unwrap();
}

fn rewrite_json(path: &Path, edit: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut value);
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn reindex_summary(suite_dir: &Path) {
    let digest = hex::encode(Sha256::digest(
        fs::read(suite_dir.join("summary.json")).unwrap(),
    ));
    rewrite_json(&suite_dir.join("raw/index.json"), |index| {
        index["summary_sha256"] = json!(digest);
    });
}

/// Rewrites the only failure as a collector with another slug rule and another
/// diff selection would have retained it, so the bundle stays self-consistent.
fn rederive_failure_files(suite: &Path) {
    let failures = suite.join("failures");
    let original = find(&failures, "message.txt").parent().unwrap().to_owned();
    let message = fs::read_to_string(original.join("message.txt")).unwrap();
    assert_ne!(stable_test_id(FAILED_CASE), OTHER_ID);
    assert_ne!(
        extract_diff(&message).map(|diff| format!("{diff}\n")),
        Some(OTHER_DIFF.to_owned())
    );

    let rederived = rederived_dir(suite);
    fs::rename(&original, &rederived).unwrap();
    fs::write(rederived.join("diff.txt"), OTHER_DIFF).unwrap();
    rewrite_json(&rederived.join("metadata.json"), |metadata| {
        point_record_at(metadata, OTHER_ID);
    });
    rewrite_json(&suite.join("summary.json"), |summary| {
        point_record_at(&mut summary["failures"][0], OTHER_ID);
    });
    reindex_summary(suite);
}

fn rederived_dir(suite: &Path) -> PathBuf {
    suite.join("failures").join(OTHER_ID)
}

fn point_record_at(record: &mut Value, id: &str) {
    record["stable_id"] = json!(id);
    record["message_path"] = json!(format!("failures/{id}/message.txt"));
    record["diff_path"] = json!(format!("failures/{id}/diff.txt"));
}

/// Drops the only recorded failure the way a coordinated edit would: counts,
/// verdict, CI state, and the raw index digest all agree afterwards.
fn forget_recorded_failure(suite: &Path) {
    rewrite_json(&suite.join("summary.json"), |summary| {
        let dropped = summary["failures"].as_array().unwrap().len() as u64;
        summary["failures"] = json!([]);
        let counts = &mut summary["counts"];
        counts["passed"] = json!(counts["passed"].as_u64().unwrap() + dropped);
        counts["failures"] = json!(0);
        counts["errors"] = json!(0);
        summary["verdict"] = json!("pass");
        summary["ci_state"] = json!("success");
    });
    reindex_summary(suite);
}

fn reindex_raw(raw: &Path, relative: &str) {
    let bytes = fs::read(raw.join(relative)).unwrap();
    rewrite_json(&raw.join("index.json"), |index| {
        let entry = index["files"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|file| file["path"] == relative)
            .unwrap();
        entry["size_bytes"] = json!(bytes.len());
        entry["sha256"] = json!(hex::encode(Sha256::digest(&bytes)));
    });
}

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

fn status_snapshot() -> String {
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
        "checks": [{
            "name": "gha-ci: test_sql",
            "provider": "GitHub Actions",
            "expected": true,
            "freshness": "current",
            "current": {
                "state": "FAILURE",
                "reported_for_sha": SHA,
                "detail_url": "https://github.com/CUBRID/cubrid/actions/runs/123",
                "reported_at": "2026-09-21T18:10:04Z"
            },
            "previous": null
        }],
        "history": { "status": "disabled", "limit": 0, "searched": 0 },
        "errors": []
    })
    .to_string()
}

async fn mount_sql_evidence(server: &MockServer, build_sha: &str) {
    let build_read =
        format!("sha={build_sha}\nmode=debug\nns=develop\nrun_id=123\nrun_attempt=2\n");
    for (url_path, body) in [
        (
            "/runs/123/sql/plan/split.meta",
            concat!(
                "total=1\nunits=1\npar=1\ntimed=0\n",
                "tc_sha='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n",
                "tc_branch='develop'\nrerun_unrun=0\nrerun_short=0\nrerun_gone=0\n"
            ),
        ),
        (
            "/runs/123/sql/plan/shards/",
            "<a href=\"00.list\">00.list</a>",
        ),
        ("/runs/123/sql/plan/plan.tsv", "00\t1\t1\t1.0\n"),
        ("/runs/123/sql/shard/", "<a href=\"00/\">00/</a>"),
        (
            "/runs/123/sql/shard/00/test-results/",
            "<a href=\"results.xml\">results.xml</a>",
        ),
        (
            "/runs/123/sql/collect/failed.list",
            "00\tsql/bugs/cases/bug_123.sql\n",
        ),
        ("/runs/123/sql/collect/verdict", "fail\n"),
        ("/runs/123/sql/shard/00/build.read", build_read.as_str()),
        (
            "/runs/123/sql/shard/00/tc.read",
            "tc_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ntc_branch=develop\n",
        ),
        (
            "/runs/123/sql/shard/00/summary_info",
            "/home/cubrid-testcases/sql/bugs/cases/bug_123.sql:nok 1200ms\n",
        ),
        (
            "/runs/123/sql/shard/00/shard.done",
            "suite=sql\nidx=00\nassigned=1\nctp_rc=0\n",
        ),
        (
            "/runs/123/sql/shard/00/test-results/results.xml",
            concat!(
                "<testsuite tests=\"1\" failures=\"1\">",
                "<testcase classname=\"sql.bugs\" name=\"sql/bugs/cases/bug_123.sql\" time=\"1.2\">",
                "<failure><![CDATA[unexpected rows\n[Diff]\n--- answer\n+++ actual\n-old\n+new]]></failure>",
                "</testcase></testsuite>"
            ),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(url_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }
}

fn write_command(directory: &Path, name: &str, behavior: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{behavior}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
