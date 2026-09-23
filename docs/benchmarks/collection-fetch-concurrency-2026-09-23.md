# Collection fetch-concurrency benchmark

Date: 2026-09-23 (Asia/Seoul)

## Decision

Use `40` as the built-in collection fetch-concurrency default. Both finalists
completed every requested suite without collector errors. Concurrency 40 had
the lower three-trial median wall time; concurrency 20 used less peak resident
memory.

## Source and evidence identity

- Pull request: `CUBRID/CUBRID#7990`
- Selected commit: `b8f3c4807470dbeb30b5113acb7dd99d2340347c`
- GitHub Actions run/attempt: `35821922423/1`
- Requested suites: `test_medium`, `test_sql`, `test_shell`
- Result in every trial: all three suites `completed`
- Collector baseline: `31bd609fda788169c462fcda61994dc0828de697`
- Benchmark source snapshot commit: `199ec718f3105b96aff6d90eef5a11eb84c25cb9`
- Benchmark source archive SHA-256:
  `c53d3c3439ab0bfc725a0bc741521d9a4f909ce187a1b1c19e67a352cda623fa`
- Collector version: `cubrid-ci 0.2.0 (199ec718f310, release)`
- Toolchain: `rustc 1.89.0`, `cargo 1.89.0`

The release binary was built from a clean temporary Git snapshot containing the
collection-hardening changes. Each collection used a separate empty evidence
root, so no response or normalized evidence was reused between trials.

## Protocol

The screen ran concurrency 10, 20, 40, and 80 once. The two fastest successful
candidates, 20 and 40, then ran three additional trials each in alternating
order. GNU `time` measured elapsed wall time and maximum resident set size for
the collector process. Exit status and each emitted manifest were checked; all
trials exited zero with the same commit, run, attempt, and three completed
suites.

## Screening

| Concurrency | Wall time (s) | Peak RSS (KiB) | Exit |
|---:|---:|---:|---:|
| 10 | 10.07 | 61,360 | 0 |
| 20 | 8.75 | 60,836 | 0 |
| 40 | 8.88 | 62,356 | 0 |
| 80 | 9.03 | 62,332 | 0 |

## Final trials

| Concurrency | Trial | Wall time (s) | Peak RSS (KiB) | Exit |
|---:|---:|---:|---:|---:|
| 20 | 1 | 9.07 | 61,504 | 0 |
| 20 | 2 | 9.40 | 61,300 | 0 |
| 20 | 3 | 8.81 | 60,856 | 0 |
| 40 | 1 | 9.86 | 62,216 | 0 |
| 40 | 2 | 8.98 | 63,212 | 0 |
| 40 | 3 | 8.88 | 62,872 | 0 |

| Concurrency | Median wall time (s) | Median peak RSS (KiB) | Errors |
|---:|---:|---:|---:|
| 20 | 9.07 | 61,300 | 0 |
| 40 | 8.98 | 62,872 | 0 |

The finalists differ by only 0.09 seconds in median wall time, so this is a
measured default rather than a claim that 40 is universally optimal. Users can
explicitly test or set another value with `--fetch-concurrency`,
`CUBRID_CI_FETCH_CONCURRENCY`, or the TOML setting.
