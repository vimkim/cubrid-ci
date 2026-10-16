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
