# Target the current workflow layout directly

The first GitHub Actions collector will implement today's job-log and internal run-directory contract directly, without a speculative adapter for the proposed future producer manifest. The current collector is deliberately replaceable: when the workflow publishes a stable manifest, it will be reimplemented against that contract rather than layered beside the temporary parser.
