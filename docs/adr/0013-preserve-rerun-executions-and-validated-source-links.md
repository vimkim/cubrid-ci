# Preserve rerun executions and validated source links

`/run rerun` starts a new execution that can select only failed or changed cases,
while suites with nothing selected carry forward the source verdict. Record
these executions in suite-summary v3 with their own counts and a digest-pinned
link to the independently validated source summary; keep ordinary summaries at
v2 and leave the command and observation envelopes unchanged. This preserves
the original failures and actual rerun outcomes for analysis without inventing
a merged full-suite result.

The producer exposes the concrete `RERUN_FROM` value in the collect step's
environment log, while GitHub's jobs API does not expose gate outputs. Use that
value, pin the source run and attempt, require source completion before the
rerun began, and validate source Engine and testcase provenance recursively.
Reject ambiguous, unavailable, or cyclic lineage. A zero-case selection inherits
a verdict only when the plan reports no failed, unrun, missing, or changed
source cases; deleted or changed zero-case selections remain unavailable.
