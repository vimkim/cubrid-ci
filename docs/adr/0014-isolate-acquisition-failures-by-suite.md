# Isolate acquisition failures by suite

Supersedes the collection-wide scheduling-stop policy in
[ADR-0011](0011-separate-source-authorization-from-evidence-validation.md).
A mandatory acquisition failure stops scheduling new shards only in the affected
suite, drains its already-running work, and preserves its failure and unattempted
shard records. Independent suites continue under the existing shared HTTP
concurrency ceiling, so one malformed plan or unavailable shard cannot discard
otherwise complete evidence. The overall collection still exits nonzero when
any requested suite is incomplete.
