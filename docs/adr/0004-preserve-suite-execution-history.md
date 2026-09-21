# Preserve suite execution history

Each suite is selected independently through its status on the exact selected commit, so suites in one snapshot may come from different GitHub Actions runs. Evidence is retained by run ID and attempt instead of overwritten, and the commit manifest records which immutable suite executions the latest collection selected.
