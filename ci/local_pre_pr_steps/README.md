# Local pre-PR Linux suites

These recipes mirror the Linux assertions in the integration, wrapper-e2e,
and test-action workflows. Platform-specific Windows and macOS legs remain CI-only.

Run a subset with `uv run --no-project python ci/local_pre_pr.py --suites cargo-registry,gha-cache`.
