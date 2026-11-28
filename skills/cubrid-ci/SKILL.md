---
name: cubrid-ci
description: Collect, inspect, and verify exact-commit CUBRID GitHub Actions CI evidence with the cubrid-ci CLI. Use when checking PR CI status, collecting exact-head or merge-base/historical-commit CI bundles, or verifying retained evidence offline.
argument-hint: "<status|collect|collect-base|collect-commit|verify|doctor> [PR-or-SHA]"
---

# CUBRID CI Evidence CLI (`cubrid-ci`)

Use `cubrid-ci` to inspect PR CI status, collect exact-commit GitHub Actions evidence bundles, and verify retained evidence offline.

This skill covers the standalone `cubrid-ci` CLI only. It never triggers or cancels CI workflows, and full failure attribution or persistent analysis reports belong to higher-level workflows such as `cubrid-ci-analyze`.

## 1. Preflight

Check `cubrid-ci doctor --json` when setup or environment health is uncertain:

```bash
command -v cubrid-ci
cubrid-ci doctor --json
```

If `cubrid-ci` is missing, install it from the local checkout (`just install` / `just sync` in `~/gh/cubrid-ci`) or `cargo install --path . --locked`.

## 2. Select a subcommand

| Subcommand | Purpose | Mutates evidence root |
|---|---|---|
| `status [PR]` | Snapshot PR CI status via `cubrid-pr-status` | No |
| `collect [PR] --commit <40-hex-sha>` | Collect exact-commit GitHub Actions evidence for a PR head | Yes |
| `collect-commit <40-hex-sha>` | Collect existing CI evidence for an arbitrary Engine commit | Yes |
| `collect-base <PR>` | Resolve PR merge base via `git` and collect its baseline CI | Yes |
| `verify <result.json> --pr <PR> --pr-head <sha>` | Audit a saved result and retained bundle offline | No |
| `doctor` | Validate `gh`, `cubrid-pr-status`, `git`, and effective config | No |

### Suites

- Default suites: `test_medium`, `test_sql`, `test_shell`.
- `test_sql_by_cci` is **opt-in**: pass `--suite test_sql_by_cci` alongside the suites you want (any `--suite` flag replaces the default suite list, so repeat `--suite` for all desired suites).

## 3. Common invocations

```bash
# Inspect current-worktree PR or explicit PR
cubrid-ci status --json
cubrid-ci status 7990 --json

# Collect exact PR head evidence (supports --wait --timeout 26h)
cubrid-ci collect 7990 --commit <40-hex-sha> --json

# Collect PR merge-base baseline or arbitrary Engine commit
cubrid-ci collect-base 7990 --json
cubrid-ci collect-commit <40-hex-sha> --json

# Offline verification of a saved collection result
cubrid-ci verify result.json --pr 7990 --pr-head <40-hex-sha> --json
```

Pass `--json` whenever machine-readable output is needed. Progress renders only on interactive `stderr`, keeping `stdout` clean for `--json`.

## 4. Interpret exit codes

A fully collected red CI suite exits `0`: **CI outcome is data** in `stdout`/JSON, while exit codes report collection completeness and evidence integrity:

| Exit | Meaning |
|---:|---|
| `0` | Requested terminal evidence was validated and collected (or verified by `verify`) |
| `2` | Invalid input or configuration |
| `3` | Unfinished, expired, or job-level evidence unavailable (or interrupted/unselected/partial/superseded in `verify`) |
| `4` | Identity, integrity, or malformed-evidence failure |
| `5` | Remote access or configured-size failure |
| `6` | Local storage or serialization failure |
