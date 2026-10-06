# Evidence schemas

The schemas use JSON Schema draft 2020-12. Schema v2 describes active GitHub
Actions command results, manifests, suite summaries, raw indexes, failures, and
bounded binary inventories. The collection-observation v1 schemas describe each
append-only collection request and its terminal result; their version is
independent of the evidence-bundle schema version. Raw indexes bind the digest
of their validated suite summary, and acquisition failures record structured
stage, endpoint, and consumed-response-byte evidence.

The command-result and manifest v1 schemas remain only to identify historical
CircleCI bundles. Those bundles are retained unchanged and are not produced or
rewritten by version 0.2.0.

Command-result/manifest v3 describe commit-centric collection by `collect-commit`
and `collect-base`. They contain `selection` with discovery coverage, candidates,
pinned run metadata, and an optional PR-to-merge-base relationship; there is no
PR-head identity field. Observation v2 embeds that selection in its request and
the v3 command in its result. Existing PR commands continue to produce the v2
command/v1 observation pair. Suite summaries, raw indexes, and failure schemas
remain v2 and retain their original digest/identity guarantees.
