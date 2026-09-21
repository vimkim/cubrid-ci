# Prove evidence identity from consumer-visible records

Until `gha-ci` publishes a versioned manifest, `cubrid-ci` will trust a terminal suite only after reconciling its exact-commit status, Actions run and attempt, every participating shard's build and testcase provenance, failed-case list, JUnit records, counts, and posted verdict. Mismatched material is retained for diagnosis but marked untrusted, because the issue-comment workflow's own `head_sha` identifies the workflow revision rather than the tested pull-request commit.
