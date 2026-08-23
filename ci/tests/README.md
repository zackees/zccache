# CI Script Tests

Python-level pytest suites that exercise the CI helper modules in `ci/`
(packaging, perf-guard parsing, release workflow). These are infrastructure
tests for the CI scripts themselves — not project tests. Per project policy,
all benchmarks and application tests are written in Rust.

Run with the same command CI uses, so a local pass means what the job means:

```bash
PYTHONPATH=. uv run --no-project --python 3.13 --with pytest --with pillow --with pyyaml python -m pytest ci/tests
```

`--no-project` because the suite reads the repo from source and needs nothing
built; `PYTHONPATH=.` makes `ci` importable without installing it. Do not reach
for `--frozen` — `uv.lock` is gitignored, so it only works on a machine that
happens to have one, and `pytest` is not declared in any dependency group.

Some suites also exercise workflow YAML directly. `test_release_detect_bump.py`
extracts the `detect-bump` step out of `.github/workflows/release-auto.yml` and
runs it under bash with stubbed `gh`/`git`/`python3`, so the branching logic is
tested rather than the file's text. It needs a POSIX bash and skips on Windows;
the step it covers runs on `ubuntu-latest`. `test_release_publish_gating.py` guards the
release recovery contract in CLAUDE.md § Publishing — that a failed run publishes
nothing, that `dry-run` defaults to rehearsing, and that a partial release resumes.

`test_doc_links.py` resolves every internal Markdown link in tracked files
(`vendor/` excluded) and fails on a missing file or heading anchor, so the
three-hop navigation guarantee in `docs/CLAUDE.md` cannot rot silently.

`test_crate_docs.py` checks `crates/CLAUDE.md` against the actual workspace
members — no phantom crates, no undocumented ones, and the stated count
matching in both `CLAUDE.md` files.

`test_readme_coverage.py` asserts every directory with tracked files carries a
`README.md`. The `readme_guard.py` hook only fires on an *edited* directory, so
untouched ones could sit without one indefinitely — seven did.

`test_documented_commands.py` checks that `-p <crate>` in any documented
command names a real workspace member, and that documented `cargo bench`
targets a crate that actually has benches — `cargo bench` on a crate with none
exits 0 and measures nothing.

`test_workflow_permissions.py` asserts every workflow declares an explicit
`permissions:` scope, at the top level or on every job (job-level blocks replace
the workflow-level one rather than merging). Without a block a job inherits the
default token scope, which is far broader than the read-only access nearly all
of them need.

CI runs this suite via `.github/workflows/python-tests.yml` on every push and
pull request. It is deliberately not a job in `ci.yml`: that workflow sets
`paths-ignore: "**/*.md"`, which would skip the Markdown guards on precisely the
docs-only PRs they exist to gate.
