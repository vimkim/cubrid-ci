# Replace suite commands with one collect command

The GitHub Actions cutover removes the CircleCI-shaped `test-medium`, `test-sql`, and `test-shell` commands without compatibility aliases. `cubrid-ci collect` selects all suites by default and accepts explicit suite filters, giving exact-commit pinning, partial-evidence handling, and JSON output one command-level contract.
