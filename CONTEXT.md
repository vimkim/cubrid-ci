# CUBRID CI Evidence

Vocabulary for observing CUBRID pull-request CI and retaining evidence for failure analysis.

## Language

**PR head**:
The latest commit published to a pull request. It can differ from the commit checked out in a local worktree.

**Status snapshot**:
One observation of a pull request and its reported CI states, together with the observation time, freshness, history coverage, and collection errors. It describes reported state, not independently verified test evidence.
_Avoid_: PR status

**Evidence snapshot**:
The normalized metadata and textual test evidence collected for one exact CUBRID commit at one observation time. An unavailable or inaccessible optional artifact is recorded as a limitation rather than silently omitted.
_Avoid_: CI result, latest result

**Selected commit**:
The exact CUBRID commit for which an evidence snapshot is requested. It is the local checkout commit for an implicit worktree request and an explicitly supplied commit for an explicit pull-request request.
_Avoid_: Current commit

**Suite result**:
The outcome reported for one of CUBRID's `test_medium`, `test_sql`, or `test_shell` suites in an evidence snapshot.
_Avoid_: Job result

**Collection success**:
Confirmation that every requested terminal suite was identity-validated and its mandatory evidence was captured. It says nothing about whether the suite result passed or failed.
_Avoid_: CI success

**Suite execution**:
One provider run and attempt selected through a suite status attached to the selected commit. Different suites in one evidence snapshot may select different executions.
_Avoid_: Build, attempt

**Unfinished suite**:
A requested suite that has either not reported a status for the selected commit or has reported a non-terminal status. These states are kept distinct as not observed and running.
_Avoid_: Missing result

**Not observed**:
No suite status was found on the selected commit. It does not prove that the suite was never requested, queued, skipped, or run.
_Avoid_: Not run yet

**Incomplete evidence snapshot**:
An evidence snapshot for which at least one requested suite is unfinished or its mandatory evidence could not be collected. Successfully captured suite evidence remains usable, but the collection command reports failure.
_Avoid_: Failed CI

**Evidence root**:
The configured durable directory that contains historical and current evidence snapshots. It is independent of the CUBRID worktree from which collection is invoked.
_Avoid_: Data directory, output directory

**Evidence server**:
The internal, short-retention HTTP service from which `gha-ci` run directories and per-shard test evidence can be read. It is distinct from GitHub's Actions artifact service.
_Avoid_: GitHub artifacts

**Build provenance**:
The exact CUBRID commit and producing workflow execution that created the build read by a testcase shard. A reused build can come from a different run than the suite execution.
_Avoid_: Build status

**Testcase provenance**:
The exact testcase repository revision and branch read by a testcase shard.
_Avoid_: Test source

**Failure evidence**:
The metadata, messages, and textual differences that directly describe a failed or abnormal testcase. It is distinct from an explanation of why the failure occurred.
_Avoid_: Root cause

**Raw evidence**:
The bounded original textual records from which normalized failure evidence is derived, retained so an extraction can be audited or repeated after the evidence server expires.
_Avoid_: Artifact dump

**Job-level failure**:
A terminal CI failure that produced no trustworthy testcase verdict, such as a failed build, plan, collector, or shard. It is evidence about pipeline execution rather than a fabricated failed testcase.
_Avoid_: Test failure

**Failure analysis**:
A reasoned interpretation of failure evidence that separates observations, inferences, and unknowns and assesses a failure's relationship to the pull request.
_Avoid_: Evidence collection
