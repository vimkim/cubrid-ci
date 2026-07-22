# Evidence schema

The schemas use JSON Schema draft 2020-12. `schema_version: 1` in emitted
documents corresponds to:

- `manifest-v1.schema.json` for commit-level `manifest.json`;
- `suite-summary-v1.schema.json` for suite-level `summary.json`.

Raw CircleCI documents and `failed-tests.json` deliberately retain upstream
fields and are not constrained by a local closed schema.

