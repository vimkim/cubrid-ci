# Retain partial evidence on collection failure

When any requested suite is not run yet, still running, or missing mandatory evidence, `cubrid-ci collect` exits nonzero but retains the manifest and every fully collected suite. This makes the unsuccessful overall snapshot explicit without discarding exact-commit evidence that is already complete.
