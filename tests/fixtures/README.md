# Shard completion fixtures

These unchanged markers come from CUBRID PR7925, Engine commit
`6a487cdb6cbda36ac94ad9b22009cea6cb58aa2f`,
[workflow run 38938588691](https://github.com/CUBRID/cubrid/actions/runs/38938588691), attempt 1.
The medium marker identifies shard `00`. The shell marker identifies shard `30`.

The producer at workflow revision `f8ae0f47b729aa27043a4c421d200c4c7403262c`
writes `test_rc` from the test command's exit status.
See `.github/workflows/gha-ci.yml`, steps `Run CTP` and `Publish results for collect`.
The shell shard reports one failed testcase and accounts for all 66 assigned cases.
The marker alone does not establish a testcase verdict.

## PR7990 rerun

`pr7990-rerun/` reproduces run `40334404336` (attempt 1), Engine commit
`52987c654b6f04734cad01c11c80c7f30358b33c`, from source run `40323508581`
(attempt 1). The zero-case SQL/medium plans and the three-case passing shell
payloads are retained producer responses. Run/job JSON keeps only fields used
by collection and source-attempt validation; the status snapshot is retained.
The test supplies a local evidence-server URL and the concrete `RERUN_FROM`
step-environment value in place of the full collect log.

Source fixtures are minimized: SQL and medium retain one passing testcase each;
shell retains the three original failed cases from shards 21, 40, and 43.
Plans, completion records, and workflow counts are reduced to those cases.
Build/testcase provenance and selected JUnit cases retain their original values.
This fixture establishes rerun behavior against a consistent source, rather than
claiming that every full source shard reconciles.

Live verification found a separate source SQL contradiction: shard 01 reports
3533 successful cases in `summary_info`, but its JUnit contains 3531. The absent
JUnit paths are `sql/_05_plcsql/_01_testspec/_05_bug_fix/cbrd-27556/cases/33_static_sql_group_by_having_alias.sql`
and `sql/_05_plcsql/_01_testspec/_05_bug_fix/cbrd-27556/cases/33_static_sql_group_by_having_alias_error.sql`.
The collector must continue to reject this source evidence.
