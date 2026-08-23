# CI Script Tests

Python-level pytest suites that exercise the CI helper modules in `ci/`
(packaging, perf-guard parsing, release workflow). These are infrastructure
tests for the CI scripts themselves — not project tests. Per project policy,
all benchmarks and application tests are written in Rust.

Run with:

```bash
uv run pytest ci/tests
```

Some suites also exercise workflow YAML directly. `test_release_detect_bump.py`
extracts the `detect-bump` step out of `.github/workflows/release-auto.yml` and
runs it under bash with stubbed `gh`/`git`/`python3`, so the branching logic is
tested rather than the file's text. It needs a POSIX bash and skips on Windows;
the step it covers runs on `ubuntu-latest`. `test_release_publish_gating.py` guards the
release recovery contract in CLAUDE.md § Publishing — that a failed run publishes
nothing, that `dry-run` defaults to rehearsing, and that a partial release resumes.
