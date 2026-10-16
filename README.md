# cubrid-ci

`cubrid-ci` inspects CUBRID pull-request status and collects durable,
exact-commit GitHub Actions evidence for `test_medium`, `test_sql`, and
`test_shell`.

## Requirements

- Rust 1.85+ for development; releases use the pinned Rust 1.89.0 toolchain
- `gh`, authenticated for GitHub API access
- `cubrid-pr-status` for `status` and PR-head `collect`
- `git` for `collect-base`
- access to the internal evidence server for terminal testcase evidence

Run `cubrid-ci doctor` to validate the local setup.

## Commands

```sh
# Show the PR associated with the current CUBRID worktree.
cubrid-ci status

# Show a specific PR.
cubrid-ci status 7990

# Collect all suites for local HEAD; local HEAD must equal the published PR head.
cubrid-ci collect

# Collect an explicit PR and exact commit.
cubrid-ci collect 7990 --commit <40-character-sha>

# Collect an arbitrary historical Engine commit, independently of a PR head.
cubrid-ci collect-commit <40-character-sha> --json

# Pin a PR head/target, resolve their merge base, and collect baseline CI.
cubrid-ci collect-base 7990 --json

# Choose an explicit historical run (Engine identity is still verified).
cubrid-ci collect-commit <sha> --run-id <run-id> --suite test_sql --json

# Collect a subset, optionally waiting for terminal states.
cubrid-ci collect 7990 --commit <sha> \
  --suite test_sql --suite test_medium --wait --timeout 26h

# Override the measured collection-wide HTTP concurrency ceiling.
cubrid-ci collect 7990 --commit <sha> --fetch-concurrency 40

# Opt in to bounded artifacts from abnormal shards.
cubrid-ci collect 7990 --commit <sha> --include-binaries \
  --max-binary-bytes 268435456 --max-binary-total-bytes 536870912
```

All commands accept `--json`. A fully collected red suite exits successfully:
CI outcome is data, while the process exit reports collection completeness and
trustworthiness.

Shard completion records support the workflow's `test_rc` field and the legacy
`ctp_rc` field. Conflicting fields and malformed exit codes fail validation.
Exit code 1 permits collection only when the shard reports failed testcases.
Assigned counts, JUnit records, and the failed-case inventory must still agree.
Other nonzero exit codes remain job-level failures.

Human-readable collection output reports each suite's CI outcome and trusted
test counts. For example, a red suite may appear as
`test_shell: FAILURE (3 failed / 3256 executed)` while the collection command
still exits successfully. Use `--json` when a stable machine-readable result is
required.

When stderr is an interactive terminal, collection progress is rendered in
place on one stderr line. Redirected stderr and stdout remain free of progress
output, so `--json` continues to emit one machine-readable value on stdout.

| Exit | Meaning |
|---:|---|
| 0 | Requested terminal evidence was validated and collected |
| 2 | Invalid input or configuration |
| 3 | Unfinished, expired, or job-level evidence unavailable |
| 4 | Identity, integrity, or malformed-evidence failure |
| 5 | Remote access or configured-size failure |
| 6 | Local storage or serialization failure |

## Historical commits and PR baselines

`collect-commit` verifies a full Engine SHA and follows paginated exact-commit
statuses and GitHub Actions check links. For each requested suite it selects the
greatest linked run ID, then report timestamp, source, and record ID, and pins
that run's latest attempt. A newer running attempt remains incomplete even if
an older status passed. Selection retains its time, candidates, run metadata,
and discovery coverage. Run attempts/state are checked again before publishing.
No repository-wide workflow scan is performed: absent exact links mean
`not_observed`, not proof that the commit never ran CI. API access failures mean
incomplete discovery, not an empty successful search.

Use `--run-id` to select a candidate outside those links. Workflow revision,
run title, and event do not prove the tested Engine; every completed suite must
pass the same build/testcase provenance, count, verdict, and digest validation
as PR collection. Explicit selection derives the suite outcome from producer
verdict and testcase evidence instead of inventing a commit-attached status.
Dispatched runs and reused builds are supported. `--attempt` requires
`--run-id`; a historical attempt can only reuse an existing validated local
bundle in the commit evidence root because remote suite files are not
attempt-addressable. Missing or damaged cached evidence never falls back to
current files labeled as the old attempt.

`collect-base` reads a CUBRID PR once, pins its actual head and target SHAs,
and executes `git merge-base --all`. It supports feature targets as well as
`develop`. The default bare Git cache is `<data-dir>/git-cache/CUBRID-cubrid.git`;
`--git-cache` overrides it. Missing ancestry is fetched by pinned SHA, without
changing working branches or refs. Zero or multiple bases are unresolved,
and pinned requests/results are retained in `<data-dir>/baseline-resolutions`
even when history resolution fails. A successful evidence selection records
PR number, head/target refs and SHAs, merge base, and observation time. Compare
that head with any independently collected head snapshot before analysis.

Both commands accept repeated `--suite`, `--data-dir`, `--artifact-base`,
`--fetch-concurrency`, and `--json`. They observe existing CI once; they do not
trigger or wait for CI. Signature matching and causal interpretation remain
in the analysis skills.

## Evidence storage

The default evidence root is `~/.local/share/cubrid-ci-data` (for this user,
`/home/vimkim/.local/share/cubrid-ci-data`). Override it with `--data-dir`,
`CUBRID_CI_DATA_DIR`, or `~/.config/cubrid-ci/config.toml`, in that precedence
order.

Evidence-server fetch concurrency defaults to 40 and is shared by every suite,
shard, text response, and optional binary response. Override it with
`--fetch-concurrency`, `CUBRID_CI_FETCH_CONCURRENCY`, or `fetch_concurrency` in
the TOML configuration, in that precedence order. `cubrid-ci doctor` reports
the effective value and source. The default's benchmark is recorded in
[`docs/benchmarks/collection-fetch-concurrency-2026-09-23.md`](docs/benchmarks/collection-fetch-concurrency-2026-09-23.md).

PR collection stores GitHub Actions evidence by PR, commit, run, attempt, and
suite. Commit/base collection stores it under
`github-actions/CUBRID-cubrid/commits/<sha>` with the same run/attempt layout.
These new commands emit command/manifest v3 and observation v2, with a commit
selection and optional baseline relationship instead of a fabricated PR head.
Existing `collect` retains command/manifest v2 and observation v1. Suite/raw/
failure schemas remain v2 for both paths. Provider inputs are immutable; `manifest.json` atomically records the
executions selected by the latest collection. Each validated raw index binds
its suite summary digest, so a warm summary cannot be reused after either side
changes. Historical schema-v1 CircleCI bundles remain recognizable but are
never rewritten.

Each invocation also creates an append-only
`observations/<observation-id>/request.json` before acquisition and adds
`result.json` on terminal completion. The observation records canonical suite
and shard acquisition states, effective fetch configuration, diagnostics, and
command-wide binary-budget consumption. Incomplete response reads retain their
stage, endpoint, consumed byte count, and diagnostic without publishing the
temporary body. A request without a result identifies an interrupted local
collection; it does not label the provider execution or a suite as failed.

## Development and release

```sh
just verify
./scripts/build-release.sh
./scripts/verify-release-reproducibility.sh
```

Version 0.2.0 intentionally exposes only `status`, `collect`, and `doctor`.
The old suite-specific CircleCI commands are unsupported.
