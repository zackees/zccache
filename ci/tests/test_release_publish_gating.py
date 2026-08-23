"""Guards the release recovery contract documented in CLAUDE.md § Publishing.

The claim that "a failed release run publishes nothing, so re-dispatching is
safe" rests on three properties of `release-auto.yml`:

1. every publish job requires `result == 'success'` from each upstream, so the
   `always()` in those conditions cannot make them permissive;
2. `dry-run` defaults to true, so an operator who accepts the defaults
   rehearses rather than ships;
3. publishing is skipped for registries a version already reached, so a
   partially published release resumes instead of republishing.

Each is one edit away from silently becoming false -- deleting a `result ==`
clause or flipping the default still produces a workflow that runs. These
tests make that edit fail instead.
"""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "release-auto.yml"

JOB_RE = re.compile(r"^  [a-z][a-z0-9_-]*:$")

# Jobs that can make something public, and the upstreams each must require.
PUBLISH_GATES: dict[str, tuple[str, ...]] = {
    "publish-release": ("preflight", "build"),
    "publish-pypi": ("preflight", "publish-release", "build-wheels", "test-wheels"),
    "publish-crates": ("preflight", "publish-release"),
}

# Jobs that must never run during a rehearsal.
DRY_RUN_GATED_JOBS = ("publish-pypi", "publish-crates")


def _job_block(name: str) -> str:
    """The YAML text of one job, from its key to the next job at that indent."""
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines()
    start = next(i for i, line in enumerate(lines) if line == f"  {name}:")
    body: list[str] = []
    for line in lines[start + 1 :]:
        if JOB_RE.match(line):
            break
        body.append(line)
    assert body, f"no body found for job {name}"
    return "\n".join(body)


def _if_condition(name: str) -> str:
    """The job-level `if:` block, normalized to one line."""
    block = _job_block(name)
    match = re.search(r"^    if: \|\n((?:      .*\n?)+)", block, re.MULTILINE)
    if match:
        return " ".join(match.group(1).split())
    match = re.search(r"^    if: (.+)$", block, re.MULTILINE)
    assert match, f"job {name} has no if: condition"
    return match.group(1).strip()


def test_every_publish_job_requires_its_upstreams_to_have_succeeded() -> None:
    """`always()` disables the default skip-on-upstream-failure, so each
    upstream must be re-checked explicitly. Dropping one of these clauses would
    let a publish run after a failed build."""
    for job, upstreams in PUBLISH_GATES.items():
        condition = _if_condition(job)
        for upstream in upstreams:
            expected = f"needs.{upstream}.result == 'success'"
            assert expected in condition, f"{job} no longer requires {expected}"


def test_a_dry_run_publishes_nothing() -> None:
    """Including the GitHub Release, which is gated at the step rather than the
    job -- the job runs during a rehearsal, only its publishing step does not."""
    for job in DRY_RUN_GATED_JOBS:
        assert "inputs['dry-run'] != true" in _if_condition(job), (
            f"{job} would publish during a dry run"
        )

    release = _job_block("publish-release")
    assert "softprops/action-gh-release" in release, "release step moved; re-check its guard"
    step = release[release.index("Publish or update release") :]
    assert "inputs['dry-run'] != true" in step[:400], (
        "the GitHub Release step is no longer dry-run gated"
    )


def test_dry_run_defaults_to_rehearsing() -> None:
    """An operator who accepts the defaults must not ship by accident."""
    text = WORKFLOW.read_text(encoding="utf-8")
    dry_run = text[text.index("      dry-run:") :][:400]

    assert re.search(r"^        default: true$", dry_run, re.MULTILINE), (
        "dry-run no longer defaults to true"
    )


def test_a_partially_published_version_resumes_instead_of_republishing() -> None:
    """Recovery after a mid-publish failure depends on preflight reporting what
    each registry already has, and on the publish jobs honouring it."""
    preflight = _job_block("preflight")
    for output in ("pypi_complete", "crates_complete"):
        assert f"{output}: ${{{{ steps.registries.outputs.{output} }}}}" in preflight, (
            f"preflight no longer exports {output}"
        )

    assert "needs.preflight.outputs.pypi_complete != 'true'" in _if_condition("publish-pypi")

    crates = _if_condition("publish-crates")
    assert "needs.preflight.outputs.crates_complete != 'true'" in crates
    # crates.io must still be reachable when PyPI was already done on an
    # earlier attempt, otherwise a partial release could never be completed.
    assert "needs.preflight.outputs.pypi_complete == 'true'" in crates, (
        "publish-crates can no longer resume after an already-published PyPI"
    )


def test_manual_dispatch_proceeds_even_when_the_release_already_exists() -> None:
    """Recovery is always a manual dispatch, so detect-bump must not skip it on
    the grounds that the tag/release from the failed attempt is already there."""
    detect = _job_block("detect-bump")
    dispatch_arm = detect[detect.index('if [ "$EVENT_NAME" = "workflow_dispatch" ]') :][:300]

    assert "should_release=true" in dispatch_arm
    # It has to short-circuit before the "release already exists" check below it.
    assert dispatch_arm.index("should_release=true") < dispatch_arm.index("exit 0") + 40
