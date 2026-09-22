# cubrid-ci

`cubrid-ci` inspects CUBRID pull-request status and collects durable,
exact-commit GitHub Actions evidence for `test_medium`, `test_sql`, and
`test_shell`.

## Requirements

- Rust 1.85+ for development; releases use the pinned Rust 1.89.0 toolchain
- `gh`, authenticated for GitHub API access
- `cubrid-pr-status`
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

# Collect a subset, optionally waiting for terminal states.
cubrid-ci collect 7990 --commit <sha> \
  --suite test_sql --suite test_medium --wait --timeout 26h

# Opt in to bounded artifacts from abnormal shards.
cubrid-ci collect 7990 --commit <sha> --include-binaries \
  --max-binary-bytes 268435456 --max-binary-total-bytes 536870912
```

All commands accept `--json`. A fully collected red suite exits successfully:
CI outcome is data, while the process exit reports collection completeness and
trustworthiness.

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

## Evidence storage

The default evidence root is `~/.local/share/cubrid-ci-data` (for this user,
`/home/vimkim/.local/share/cubrid-ci-data`). Override it with `--data-dir`,
`CUBRID_CI_DATA_DIR`, or `~/.config/cubrid-ci/config.toml`, in that precedence
order.

GitHub Actions provider evidence is stored by PR, commit, run, attempt, and
suite. Provider inputs are immutable; `manifest.json` atomically records the
executions selected by the latest collection. Historical schema-v1 CircleCI
bundles remain recognizable but are never rewritten.

## Development and release

```sh
just verify
./scripts/build-release.sh
./scripts/verify-release-reproducibility.sh
```

Version 0.2.0 intentionally exposes only `status`, `collect`, and `doctor`.
The old suite-specific CircleCI commands are unsupported.
