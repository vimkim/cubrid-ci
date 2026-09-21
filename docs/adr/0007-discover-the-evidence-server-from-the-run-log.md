# Discover the evidence server from the run log

The current workflow publishes no standard GitHub Actions artifacts and exposes its internal evidence-server base URL in the collect-job log. `cubrid-ci` will discover that URL from the pinned run through `gh api`, with an explicit configuration or command-line override, instead of hard-coding a private address; this adapter can be superseded when `gha-ci` adopts the versioned manifest proposed in `gha-ci-suggestion.md`.
