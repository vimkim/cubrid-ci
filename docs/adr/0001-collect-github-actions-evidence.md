# Collect new evidence from GitHub Actions only

CUBRID's active regression pipeline has moved to GitHub Actions. `cubrid-ci` will collect new `test_medium`, `test_sql`, and `test_shell` evidence only from GitHub Actions instead of maintaining parallel CircleCI and GitHub Actions collectors; existing CircleCI bundles remain historical evidence and CircleCI statuses may still appear in status snapshots.
