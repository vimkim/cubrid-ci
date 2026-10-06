# Collect historical Engine commits with shared validation

PR failure attribution needs CI at the exact merge base, which may never have been the current head of the PR being analyzed. Add `collect-commit` and a `collect-base` wrapper in `cubrid-ci`, sharing the existing suite collector and retaining the PR-head identity gate on `collect`; extend ADR-0003's selection modes without weakening its PR semantics. Keep interpretation in the analysis skills as required by ADR-0002.

Commit collection uses command/manifest v3 and observation v2 rather than fabricating a PR identity to fit the existing envelope. Existing PR and suite evidence schemas remain unchanged. The wrapper records pinned PR/head/target/merge-base identities and uses a dedicated Git cache, so collecting a historical baseline does not mutate source branches.

Discovery follows exact-commit status/check links with deterministic per-suite selection, or an explicit run ID. It records its coverage and never infers tested Engine identity from workflow `head_sha`; dispatched runs and cached builds must pass the existing shard provenance checks. Remote evidence is addressed by run but not attempt, so explicit older attempts require immutable local evidence; fetching current files as historical evidence would contradict ADR-0004's execution identity guarantee.
