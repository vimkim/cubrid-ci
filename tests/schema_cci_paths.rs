use serde_json::{Value, json};

const CCI_PATH: &str =
    "providers/github-actions/runs/44081723670/attempts/1/test_sql_by_cci/summary.json";

fn assert_paths(schema: &str, mut document: Value, pointer: &str) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for path in [
        CCI_PATH,
        "providers/github-actions/runs/1/attempts/1/test_sql/summary.json",
    ] {
        *document.pointer_mut(pointer).unwrap() = json!(path);
        assert!(validator.is_valid(&document), "valid path rejected: {path}");
    }
    for path in [
        "providers/github-actions/runs/1/attempts/1/test_unknown/summary.json",
        "providers/github-actions/runs/1/attempts/1/test_sql_by_cci/../summary.json",
    ] {
        *document.pointer_mut(pointer).unwrap() = json!(path);
        assert!(
            !validator.is_valid(&document),
            "invalid path accepted: {path}"
        );
    }
}

#[test]
fn carried_forward_summary_accepts_cci_source_path() {
    let document = json!({
        "schema_version": 3, "suite": "test_sql_by_cci", "ci_state": "success",
        "collection_state": "complete", "run_id": 2, "attempt": 1,
        "artifact_base": "https://example.com", "verdict": "pass",
        "counts": {"planned": 0, "run": 0, "unrun": 0, "tests": 0,
                   "passed": 0, "failures": 0, "errors": 0, "skipped": 0},
        "shards": [], "failures": [], "commit": "a".repeat(40),
        "rerun": {"mode": "carried_forward", "run_id": 1, "attempt": 1,
                  "summary": CCI_PATH, "summary_sha256": "a".repeat(64)}
    });
    assert_paths(
        include_str!("../schema/suite-summary-v3.schema.json"),
        document,
        "/rerun/summary",
    );
}

#[test]
fn verification_report_accepts_cci_summary_and_source_paths() {
    let document = json!({
        "schema_version": 1, "ok": true, "output_dir": null, "observation_dir": null,
        "assessment": {"identity": "established", "observation": "complete",
                       "manifest": "validated", "requested_summaries": "validated",
                       "result_matches_observation": true, "requested_suites_reconciled": true},
        "baseline": null,
        "suites": {"test_sql_by_cci": {"state": "completed", "summary": "validated",
                   "path": CCI_PATH, "rerun_sources": [{"run_id": 1, "attempt": 1,
                   "mode": "carried_forward", "path": CCI_PATH}]}},
        "issues": []
    });
    let schema = include_str!("../schema/verification-v1.schema.json");
    assert_paths(schema, document.clone(), "/suites/test_sql_by_cci/path");
    assert_paths(
        schema,
        document,
        "/suites/test_sql_by_cci/rerun_sources/0/path",
    );
}
