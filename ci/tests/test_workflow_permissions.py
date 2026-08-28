"""Every workflow must declare an explicit `permissions:` scope.

Without one, a job inherits the repository/organisation default token scope,
which is typically far broader than "read the checkout" -- the only thing most
of these jobs do. Declaring it bounds the blast radius if a third-party action
or a transitive dependency in one of these jobs is ever compromised, which
matters most for the ones triggered by `pull_request`.

17 of 22 workflows had no block at all when this was written; the repo already
had the right instinct in `perf-guard.yml` and in `ci.yml`'s msrv job, it just
was not applied workflow-wide.

Job-level `permissions` REPLACE the workflow-level block rather than merging
with it, so a workflow satisfies this rule when it declares one at the top
level or on every single job.
"""

from __future__ import annotations

from pathlib import Path

# Imported hard, not via importorskip: a guard that silently skips itself when
# a dependency is missing is the failure this whole line of work is about. If
# pyyaml is absent the suite fails loudly at collection instead. The CI job
# passes it with `--with pyyaml`.
import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW_DIR = ROOT / ".github" / "workflows"


def _workflows() -> list[Path]:
    return sorted(
        [p for p in WORKFLOW_DIR.iterdir() if p.suffix in {".yml", ".yaml"}]
    )


def _load(path: Path) -> dict:
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def test_every_workflow_declares_permissions() -> None:
    missing: list[str] = []
    for path in _workflows():
        doc = _load(path)
        if doc.get("permissions") is not None:
            continue
        jobs = doc.get("jobs") or {}
        # A reusable workflow called by another may legitimately inherit, but
        # only if every job pins its own scope -- otherwise the caller's
        # default leaks through.
        if jobs and all(
            isinstance(job, dict) and job.get("permissions") is not None
            for job in jobs.values()
        ):
            continue
        missing.append(path.name)

    assert not missing, (
        "workflows with no explicit `permissions:` (they inherit the default "
        "token scope):\n  " + "\n  ".join(missing)
    )


def test_the_check_sees_the_workflows() -> None:
    """Guards the test above from passing vacuously if the glob ever breaks."""
    found = _workflows()
    assert len(found) >= 20, f"only found {len(found)} workflows in {WORKFLOW_DIR}"
