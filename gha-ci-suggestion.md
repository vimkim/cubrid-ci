# Suggestion: publish a versioned CI evidence manifest

## Problem

The `gha-ci` workflow already preserves the evidence needed to understand `test_medium`, `test_sql`, and `test_shell` results, but it exposes that evidence as workflow-internal files and directory listings. A consumer currently has to reconstruct the contract from:

- commit statuses whose target URLs identify an Actions run;
- GitHub job metadata and logs;
- `collect/failed.list` and `collect/verdict`;
- per-shard JUnit XML, `build.read`, `tc.read`, `shard.done`, and failure archives; and
- the internal HTTP directory layout under `runs/<run-id>/`.

The Actions run's own `head_sha` cannot identify the tested CUBRID commit for an `issue_comment` run because the workflow executes from `develop`. The tested commit is instead attached to the suite status and repeated in shard provenance. This is correct, but it leaves external consumers dependent on implicit layout and log conventions.

## Proposed contract

Have the `collect` job write one additive file without changing existing evidence or human summaries:

```text
runs/<run-id>/evidence-manifest.json
```

The file should be published after suite collection and before the job exits with its verdict. Use an integer `schema_version`, keep paths relative to the run directory, and never include credentials, authorization headers, or signed URLs.

Suggested shape:

```json
{
  "schema_version": 1,
  "workflow": "gha-ci",
  "run_id": 35591621517,
  "run_attempt": 1,
  "event": "issue_comment",
  "pr_number": 7990,
  "tested_commit": "2c792b423ccac027c41317c0ba28e26f5336870d",
  "created_at": "2026-09-21T11:37:45Z",
  "suites": {
    "test_sql": {
      "state": "failure",
      "counts": {
        "planned": 17466,
        "run": 17466,
        "passed": 17464,
        "failed": 2,
        "skipped": 0,
        "unrun": 0
      },
      "failed_list": "sql/collect/failed.list",
      "verdict": "sql/collect/verdict",
      "shards": [
        {
          "index": 3,
          "state": "complete",
          "build_provenance": "sql/shard/03/build.read",
          "testcase_provenance": "sql/shard/03/tc.read",
          "completion_record": "sql/shard/03/shard.done",
          "test_results": ["sql/shard/03/test-results/linux_sql_64bit.xml"]
        }
      ]
    }
  }
}
```

The real manifest should include all requested suites and all planned shards, including shards that never published a result. Suite states should distinguish at least `success`, `failure`, `error`, and `unfinished`. Counts should use the same authoritative values as the existing verdict logic.

## Identity requirements

The producer should fail rather than publish a misleading manifest when these checks do not hold:

1. `tested_commit` is a full lowercase SHA resolved by the gate.
2. Every participating shard's `build.read` identifies that tested commit, while separately retaining the build-producing run ID and attempt.
3. Every participating shard's `tc.read` agrees with the suite plan's testcase revision and branch.
4. Every entry in `failed.list` maps to the named shard and a retained testcase result, or carries an explicit diagnostic explaining why no result exists.
5. Counts reconcile with the existing suite verdict and coverage guards.

The manifest should describe reused builds accurately: the suite execution run and the build-producing run are different identities and must not be collapsed.

## Compatibility and rollout

- Add the manifest without moving or renaming current files.
- Keep the current step summaries and artifact links for humans.
- Document the schema beside `gha-ci.yml` and treat incompatible changes as a schema-version increment.
- Let consumers continue supporting the current implicit layout during rollout, then prefer the manifest whenever present.
- Preserve the existing seven-day run-directory retention unless a separate policy change is approved.

## Acceptance checks

- A `/run all` execution publishes one manifest covering all three suites.
- Separate suite runs publish manifests containing only their requested suites.
- A passing suite, a testcase-failing suite, a dead shard, and a cancelled/superseded run are represented without inference from missing files.
- A GitHub “Re-run failed jobs” records the new `run_attempt` and identifies retained earlier-attempt evidence rather than relabeling it.
- A consumer can locate every failed testcase's JUnit evidence from relative manifest paths and verify both build and testcase provenance.
- The manifest contains no internal credentials or expiring authorization material.
