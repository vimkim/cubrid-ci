# CUBRID CircleCI Analyzer — Implementation Plan

## 1. Purpose

Build a command-line tool that retrieves the CircleCI result for one of the
three historical CUBRID CI suites attached to a GitHub pull request and commit:

- `test_medium`
- `test_sql`
- `test_shell`

The output is a durable, machine-readable evidence bundle for AI agents that
analyze CI failures. Collection and root-cause analysis are separate concerns:
this project collects and normalizes evidence without claiming why a test
failed.

## 2. Command contract

The CLI has exactly three suite subcommands:

```text
cubrid-ci test-medium <github-pr-url> [commit] [options]
cubrid-ci test-sql    <github-pr-url> [commit] [options]
cubrid-ci test-shell  <github-pr-url> [commit] [options]
```

Examples:

```text
cubrid-ci test-medium https://github.com/CUBRID/cubrid/pull/6864 c2cbeaf
cubrid-ci test-sql https://github.com/CUBRID/cubrid/pull/6864
cubrid-ci test-shell https://github.com/CUBRID/cubrid/pull/6864 --wait --timeout 26h
```

`commit` is optional. When omitted, the tool resolves the PR's current
`headRefOid` once at command start and pins the entire run to that SHA. If the
PR head changes while waiting, the tool does not silently switch commits.

An abbreviated explicit commit is resolved to a full SHA. The tool verifies
that the commit belongs to the repository and is associated with the supplied
PR. An unrelated commit is rejected unless a future explicit escape hatch is
added.

Recommended common options:

```text
--data-dir PATH                 default: ./data
--wait                          poll until a terminal result exists
--timeout DURATION              bounded even for test_shell
--poll-interval DURATION
--attempt CIRCLECI_JOB_NUMBER   select a specific verified rerun
--artifact-mode MODE            manifest | text | all
--include-test-sources          fetch accessible testcase/answer inputs
--json                          emit the command result to stdout as JSON
--verbose
```

The default command is one-shot. In particular, `test-shell` must not wait
indefinitely: the shared self-hosted runner can remain queued for many hours or
may never run.

## 3. Result identity and stale-result protection

The authoritative lookup chain is:

```text
PR URL + optional commit
  -> repository, PR number, PR metadata, exact full SHA
  -> GitHub commit status context `ci/circleci: <suite>`
  -> CircleCI job number from that status's target URL
  -> CircleCI v1.1 job metadata
  -> verify job revision, job name, and build number
  -> tests, logs, and artifacts
```

Before accepting any result, require all of the following:

1. CircleCI `vcs_revision` equals the requested full SHA.
2. CircleCI `workflows.job_name` equals the requested suite.
3. CircleCI `build_num` equals the number parsed from the GitHub status URL.
4. The GitHub status belongs to the exact requested SHA.

Never substitute:

- the numerically newest CircleCI job;
- a result attached to an older PR head;
- a local report found under `my-cubrid-docs`;
- a differently named job from the same workflow.

If several status attempts exist for the same SHA and suite, select the most
recent by GitHub status timestamp unless `--attempt` is supplied. A selected
explicit attempt must pass the same identity checks.

## 4. Prerequisites and state machine

The observed CUBRID workflow at `c2cbeaf` gates `test_medium` and `test_sql` on
both `build` and `build_debug`. `test_shell` is additionally gated by
`download-build` and runs on the exclusive `cubrid/ramdisk` resource class.

The collector records these prerequisite contexts and follows this state
machine:

```text
resolve input
  -> prerequisite missing/pending
       -> one-shot: unavailable
       -> --wait: poll until success, failure, or timeout
  -> prerequisite failed/canceled
       -> unavailable because build failed
  -> suite status missing/pending
       -> one-shot: unavailable
       -> --wait: poll until terminal or timeout
  -> suite status terminal
       -> verify CircleCI identity
       -> collect result, regardless of suite success or failure
```

A suite failure is successfully collected data, not a CLI execution failure.

## 5. Storage contract

The ticket is the first `CBRD-XXXXX` found in PR title, body, branch, or
explicit metadata. Preserve uppercase spelling in the path. The seven-character
short SHA is derived only after the full SHA has been validated.

```text
data/
└── CBRD-26357/
    └── c2cbeaf/
        ├── manifest.json
        ├── test_medium/
        ├── test_sql/
        └── test_shell/
```

If a suite result is unavailable, its directory exists but remains empty.
Unavailable state, prerequisite state, timestamps, and diagnostic reason live
in the commit-level `manifest.json`, not inside the suite directory.

Collection is staged outside the final suite directory and atomically renamed
only after validation and normalization succeed. A failed or interrupted
collection therefore cannot make an unavailable directory look complete.

Once available, a suite directory contains:

```text
test_sql/
├── summary.json
├── failed-tc.txt
├── failed-tests.json
├── failures/
│   └── <stable-test-id>/
│       ├── metadata.json
│       ├── message.txt
│       └── diff.txt
├── logs/
├── artifacts.json
└── attempts/
    └── 139540/
        └── raw/
            ├── github-status.json
            ├── job.json
            ├── tests.json
            └── artifacts.json
```

Top-level suite files are the normalized view of the selected/latest attempt.
Raw responses are retained per CircleCI job number so a rerun at the same SHA
does not destroy earlier evidence. Re-fetching the same immutable attempt is
idempotent.

If no ticket can be extracted, the recommended fallback is `PR-<number>`.
This remains a product decision; see section 11.

### Commit manifest

At minimum, `manifest.json` records:

- schema version;
- repository and PR identity;
- PR title, base branch, and head branch;
- requested input commit and resolved full SHA;
- ticket/fallback directory identity;
- collection timestamps and tool version;
- prerequisite statuses and URLs;
- each suite's state: `missing`, `pending`, `build_failed`, `completed`,
  `collection_failed`, or `timed_out`;
- selected CircleCI job number when known;
- last diagnostic without credentials or authorization headers.

## 6. Normalized evidence for AI agents

### Summary statistics

`summary.json` includes:

- suite and CircleCI job identity;
- CI status and result-count map, preserving unknown future result values;
- total, success, failure, skipped, error, and unknown counts;
- queued, started, and stopped timestamps;
- queue duration and wall-clock duration;
- configured/observed parallelism and failed node indexes;
- per-test total, minimum, maximum, median, p95, and slowest tests;
- failed-test count and artifact count;
- prerequisite status snapshot;
- source URLs for the GitHub PR/status and CircleCI job/tests endpoints;
- testcase repository revision when it can be established from CI evidence.

### Failed-test inventory

`failed-tc.txt` contains one original CircleCI test name per line.
`failed-tests.json` contains the complete failed test objects without truncating
their messages.

Stable test IDs used as directory names are derived from the original test
path plus a short hash, avoiding collisions and unsafe path components.

### Detailed failure evidence

For each failure:

- `metadata.json` preserves name, file, class, source, result, runtime, and
  relevant node mapping;
- `message.txt` stores the complete CircleCI failure message verbatim as the
  source of truth;
- `diff.txt` is a best-effort extraction of `[Diff]`, unified diff, side-by-side
  comparison, timeout, fatal, assertion, or core-dump evidence.

Diff extraction never replaces or mutates the original message. If no reliable
diff can be isolated, `diff.txt` is empty and metadata records
`diff_extraction: unavailable`.

## 7. Logs and artifacts

CircleCI action output URLs may expire, so failed action output is downloaded
during collection. Persist logs by step and parallel node with an index that
links them back to the failed actions and test failures where possible.

Always fetch the artifact manifest. Artifact download modes are:

- `manifest`: metadata and URLs only;
- `text`: recommended default; logs, XML, text, properties, and other bounded
  textual diagnostics;
- `all`: includes large binary artifacts such as core dumps.

Downloads are streamed, size-limited, checksummed, and written atomically.
Never persist request authorization headers or environment tokens. Redact
credentials if a remote response unexpectedly reflects them.

Optional testcase-source enrichment records exact source and answer URLs and
downloads them only when accessible. Private `cubrid-testcases-private-ex`
content may require `GH_TOKEN`/`GITHUB_TOKEN`; inability to fetch enrichment
must not invalidate an otherwise complete CI evidence bundle.

## 8. Authentication and remote APIs

Use GitHub REST/GraphQL APIs rather than requiring an interactive browser.
Support `GH_TOKEN` or `GITHUB_TOKEN`; permit unauthenticated public reads with
clear rate-limit errors.

Use CircleCI API v1.1 for job metadata, tests, and artifacts because the
current CUBRID endpoints are publicly readable. If CircleCI later requires
authentication, fail with a precise boundary instead of scraping the web UI.

HTTP behavior includes bounded connect/read timeouts, retry with backoff for
transient failures and rate limits, and no retry for validation failures.

## 9. Exit-code contract

Proposed exit codes:

| Code | Meaning |
|---:|---|
| 0 | A terminal suite result was validated and collected, including failed CI |
| 2 | Invalid CLI input or configuration |
| 3 | Current result unavailable, pending, build-blocked, or wait timed out |
| 4 | Stale or mismatched GitHub/CircleCI identity |
| 5 | Remote API/authentication/rate-limit failure |
| 6 | Local storage, schema, or normalization failure |

All human-readable errors go to stderr. With `--json`, stdout remains valid
JSON suitable for orchestration.

## 10. Implementation and verification sequence

1. Bootstrap the selected runtime, packaging, linting, and test configuration.
2. Define typed domain models and a versioned JSON schema.
3. Implement PR URL parsing, PR metadata lookup, commit resolution, ticket
   extraction, and commit association validation.
4. Implement exact-SHA GitHub status discovery and rerun selection.
5. Implement CircleCI job/tests/artifacts clients and identity validation.
6. Implement the shared collector pipeline and atomic storage transaction.
7. Add the three thin suite subcommands and prerequisite policies.
8. Normalize statistics, inventories, full messages, and extracted diffs.
9. Add failed-step output capture and artifact download modes.
10. Add polling with bounded timeout and interruption-safe behavior.
11. Add optional testcase-source enrichment.
12. Document installation, authentication, examples, output schema, and exit
    codes.

Verification uses recorded, sanitized fixtures plus narrow live integration
tests. Required cases include:

- malformed/non-CUBRID PR URL;
- omitted commit pinned to PR head;
- valid abbreviated historical commit;
- commit not associated with PR;
- missing, pending, successful, failed, and canceled prerequisites;
- missing/pending suite with empty suite directory;
- successful suite with zero failures;
- failed suite with full failure messages and extracted diffs;
- message with only `Test failed` and no fabricated diff;
- skipped tests excluded from failure count but retained in totals;
- parallel SQL and shell node statistics;
- same-SHA reruns and explicit `--attempt`;
- stale SHA, wrong job name, and wrong build-number rejection;
- transient API failures, rate limits, expired logs, interrupted download, and
  atomic retry;
- artifact path traversal, duplicate names across nodes, size limits, and
  checksum verification;
- no-ticket fallback behavior;
- deterministic output after volatile timestamps are normalized.

The end-to-end acceptance fixture is PR 6864 at
`c2cbeaf9f08c767290cdf14c9af0726447c59a1b`. The implementation must discover
and validate all three suite jobs, reproduce CircleCI result counts, preserve
the five SQL and eighteen shell failures observed for that attempt, and leave
no partial suite output when simulating an unavailable result.

## 11. Decisions requiring confirmation

| Decision | Recommended default | Alternatives/impact |
|---|---|---|
| Runtime | **Accepted: Rust 1.85+, edition 2024, Clap, Tokio, Reqwest, Serde** | Selected for a single distributable binary, typed state/identity validation, and safe streamed artifact handling |
| Waiting | One-shot by default; `--wait --timeout 26h` opt-in | Blocking by default is hazardous for the exclusive shell queue |
| Reruns | Preserve `attempts/<job-number>` and expose newest normalized view | Latest-only is smaller but destroys evidence |
| Artifacts | `text` default; core dumps opt-in | `all` may consume substantial disk/network unexpectedly |
| Build gates | Follow actual `build`, `build_debug`, and shell `download-build` contexts | Checking only `build` misrepresents the current workflow |
| Missing ticket | Fall back to `PR-<number>` | Failing forces manual input and prevents generic use |
| Test sources | Optional enrichment, non-fatal without private-repo access | Making it mandatory prevents public/unauthenticated collection |

The Rust runtime and all other recommended defaults were accepted for implementation.
