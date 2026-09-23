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
