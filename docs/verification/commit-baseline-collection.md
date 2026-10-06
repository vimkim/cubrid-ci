# Commit and merge-base collection verification

Verified on 2026-10-06 in the `feat/commit-baseline-collection` worktree.

## Automated checks

`just verify` passed: format, all-target check, 102 tests, and Clippy with warnings denied. Focused tests and lint were rerun after the final progress-message and schema-assertion changes.

New coverage includes paginated exact-commit links, unrelated checks, deterministic newer-run selection, a newer running attempt over an old terminal status, dispatched runs using another run's exact-Engine build, explicit per-suite verdicts, wrong Engine artifacts, missing historical-attempt cache, cache tampering, actual Git merge-base graphs including multiple/unrelated bases, missing-object fetches into a dedicated cache, and the feature-target wrapper with absent CI evidence.

## Read-only live verification

Both commands below completed successfully using the task binary and an isolated evidence root. No CI execution was created.

```sh
target/debug/cubrid-ci collect-commit 15e7dc8b5b56fd8751d56ccae1fdf87d315bd35d --suite test_shell --data-dir /home/vimkim/.cache/cubrid-ci-merge-base-verification --json
target/debug/cubrid-ci collect-base 8094 --suite test_shell --data-dir /home/vimkim/.cache/cubrid-ci-merge-base-verification --json
```

The wrapper's bare Git cache was seeded from the existing local Engine object database with `git clone --bare --shared`; automatic missing-object fetch is covered by the isolated Git test. The wrapper pinned PR head `0809a480df55ac6767a905d03fa3d31edd40a93c`, target `develop` at `15e7dc8b5b56fd8751d56ccae1fdf87d315bd35d`, and resolved that same SHA as the merge base.

Both selected dispatch run `37027696235`, attempt 1, and validated consumed Engine build run `36993422045`. The shell evidence reconciled to 3291 planned/tests, 3261 run, 3258 passed, 3 failures, 0 errors, 30 skipped, and 0 unrun. Collection exit zero describes complete evidence despite the red suite.

The live command JSON and both matching observation request/result pairs passed the v3/v2 schemas. Immutable observation IDs:

- Commit: `20261006T080645.427794505Z-2117915-0`
- Baseline wrapper: `20261006T080802.669174253Z-2134821-0`

Evidence root: `/home/vimkim/.cache/cubrid-ci-merge-base-verification/github-actions/CUBRID-cubrid/commits/15e7dc8b5b56fd8751d56ccae1fdf87d315bd35d`.

## Limits

Live collection covered the shell suite; existing suite tests cover SQL/medium acquisition. Missing links return not observed rather than scanning all workflow executions. Old attempts require a validated local bundle. Source validation does not install the binary or skills.
