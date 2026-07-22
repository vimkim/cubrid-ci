# cubrid-circleci-analyzer

`cubrid-ci` fetches and normalizes the CircleCI evidence attached to an exact
CUBRID pull-request commit. It supports the historical `test_medium`,
`test_sql`, and `test_shell` jobs and produces durable evidence bundles for AI
failure analysis.

The collector never substitutes a result from another commit. It resolves the
GitHub status on the requested SHA, then verifies the CircleCI job revision,
job name, and build number before publishing output.

## Build and install

Rust 1.85 or newer is required.

```sh
cargo build --release
cargo install --path . --locked
```

The executable is named `cubrid-ci`.

## Usage

```sh
cubrid-ci test-medium https://github.com/CUBRID/cubrid/pull/6864 c2cbeaf
cubrid-ci test-sql https://github.com/CUBRID/cubrid/pull/6864
cubrid-ci test-shell https://github.com/CUBRID/cubrid/pull/6864 \
  --wait --timeout 26h
```

The commit is optional and defaults to the PR head resolved at command start.
The resolved SHA remains pinned if the PR head moves while the tool is waiting.

Useful options:

```text
--data-dir PATH
--wait --timeout 26h --poll-interval 60s
--attempt CIRCLECI_JOB_NUMBER
--artifact-mode manifest|text|all
--max-artifact-bytes BYTES
--include-test-sources
--json
```

The default artifact mode is `text`: the manifest and bounded textual
diagnostics are downloaded, while core dumps require `--artifact-mode all`.
Failed CircleCI step output is captured in every mode because its signed URL
can expire.

## Authentication

Public GitHub and current CircleCI v1.1 endpoints work without credentials,
subject to rate limits. Set `GH_TOKEN` or `GITHUB_TOKEN` for authenticated
GitHub access and private testcase-source enrichment. Tokens and authorization
headers are never written to the evidence bundle.

## Output

```text
data/CBRD-26357/c2cbeaf/
├── manifest.json
├── test_medium/
├── test_sql/
│   ├── summary.json
│   ├── failed-tc.txt
│   ├── failed-tests.json
│   ├── failures/<stable-id>/{metadata.json,message.txt,diff.txt}
│   ├── logs/
│   ├── artifacts.json
│   └── attempts/<circleci-job>/raw/
└── test_shell/
```

If a current result is missing or pending, the suite directory remains empty
and `manifest.json` records the remote state. CI failure itself is data, so a
successfully collected failed suite exits with code 0.

| Exit | Meaning |
|---:|---|
| 0 | Terminal suite result validated and collected |
| 2 | Invalid input or configuration |
| 3 | Result unavailable, pending, build-blocked, or timed out |
| 4 | GitHub/CircleCI identity mismatch |
| 5 | Remote API, authentication, or rate-limit failure |
| 6 | Local storage, schema, or normalization failure |

See [PLAN.md](PLAN.md) for the complete storage and verification contracts and
[schema/](schema/) for the versioned normalized JSON schemas.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
