"""Behavioural tests for release-auto.yml's `detect-bump` step.

The interesting property is not that the YAML contains a string -- it is that
the *shell logic* takes the right branch. So these tests extract the real
`run:` block out of the workflow and execute it under bash with stubbed
`gh`, `git`, and `python3`. Extracting rather than copying means the test
cannot drift away from the workflow it is guarding.

The case that motivated this: 1.13.6 was bumped, its release run failed on an
ARM64 build, and every push afterwards skipped the release and reported
success. The failure was invisible under an unbroken wall of green until
someone checked PyPI by hand (issue #1472).
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import textwrap
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "release-auto.yml"

WARNING_MARKER = "::warning title=Version"
NEXT_JOB_RE = re.compile(r"^  [a-z][a-z0-9-]*:$")

def _bash() -> str | None:
    """A POSIX bash that can run the extracted step, or None to skip.

    Skipped on Windows deliberately. `shutil.which("bash")` finds WSL's bash,
    which cannot execute a host-path script, and Git Bash re-derives PATH
    through the MSYS translation layer on startup, so the stub directory this
    test prepends is dropped before the script runs. The step under test runs
    on `ubuntu-latest`, so a POSIX bash is both the faithful environment and
    the one CI provides.
    """
    if os.name == "nt":
        return None
    return shutil.which("bash")


BASH = _bash()

pytestmark = pytest.mark.skipif(
    BASH is None, reason="needs a POSIX bash to execute the extracted step"
)


def _detect_bump_script() -> str:
    """The `run: |` body of the detect-bump step, dedented to column 0."""
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == "run: |")
    body: list[str] = []
    for line in lines[start + 1 :]:
        if line.strip() and NEXT_JOB_RE.match(line):
            break
        body.append(line)
    script = textwrap.dedent("\n".join(body))
    assert "should_release" in script, "extracted the wrong block"
    return script


def _write_stubs(stub_dir: Path) -> None:
    stub_dir.mkdir(parents=True, exist_ok=True)

    # Succeeds only for tags named in RELEASED_TAGS.
    gh = r"""#!/usr/bin/env bash
for arg in "$@"; do
  case "$arg" in
    */releases/tags/*) tag="${arg##*/}"
      for t in ${RELEASED_TAGS:-}; do [ "$t" = "$tag" ] && exit 0; done
      exit 1;;
  esac
done
exit 1
"""
    # `git show HEAD^:Cargo.toml` -> the previous manifest.
    git = r"""#!/usr/bin/env bash
if [ "$1" = "show" ]; then
  if [ -n "${OLD_MANIFEST:-}" ]; then printf '%s\n' "$OLD_MANIFEST"; exit 0; fi
  exit 1
fi
exit 0
"""
    # Stands in for tomllib, which needs Python >= 3.11. CI has 3.13; a dev box
    # may not, and the logic under test is the shell branching, not the parse.
    python3 = r"""#!/usr/bin/env bash
if [ "$1" = "-c" ]; then
  sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1
else
  cat > /dev/null
  sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
fi
"""
    for name, body in (("gh", gh), ("git", git), ("python3", python3)):
        path = stub_dir / name
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)


def _run(
    tmp_path: Path,
    *,
    version: str,
    old_manifest: str,
    released_tags: str,
    event_name: str = "push",
    ref_type: str = "branch",
) -> tuple[str, str]:
    """Run detect-bump; return (combined output, GITHUB_OUTPUT contents)."""
    stub_dir = tmp_path / "stub"
    _write_stubs(stub_dir)
    (tmp_path / "Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{version}"\n', encoding="utf-8"
    )
    script = tmp_path / "detect.sh"
    script.write_text(_detect_bump_script(), encoding="utf-8")
    github_output = tmp_path / "gh_output.txt"
    github_output.write_text("", encoding="utf-8")

    env = dict(os.environ)
    env["PATH"] = f"{stub_dir}{os.pathsep}{env['PATH']}"
    env.update(
        EVENT_NAME=event_name,
        REF_TYPE=ref_type,
        RUN_ATTEMPT="1",
        GITHUB_REPOSITORY="zackees/zccache",
        GITHUB_OUTPUT=str(github_output),
        OLD_MANIFEST=old_manifest,
        RELEASED_TAGS=released_tags,
    )
    proc = subprocess.run(
        [BASH, str(script)],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    # A crashed script yields empty output, which would otherwise read as
    # "no warning emitted" and pass the negative assertions vacuously.
    assert proc.returncode == 0, (
        f"detect-bump exited {proc.returncode}: {proc.stdout}{proc.stderr}"
    )
    return proc.stdout + proc.stderr, github_output.read_text(encoding="utf-8")


def test_an_unreleased_version_warns_on_a_quiet_push(tmp_path: Path) -> None:
    """The stuck case: no bump, and this version never shipped."""
    out, gh_output = _run(
        tmp_path,
        version="1.13.6",
        old_manifest='version = "1.13.6"',
        released_tags="1.13.5",
    )

    assert WARNING_MARKER in out, "a stuck release must not be silent"
    assert "1.13.6" in out
    # Warning only -- an unrelated push must still skip cleanly, not fail.
    assert "should_release=false" in gh_output


def test_a_released_version_stays_quiet(tmp_path: Path) -> None:
    """The normal case. Warning on every quiet push would train people to ignore it."""
    out, gh_output = _run(
        tmp_path,
        version="1.13.6",
        old_manifest='version = "1.13.6"',
        released_tags="1.13.5 1.13.6",
    )

    assert WARNING_MARKER not in out
    assert "should_release=false" in gh_output


def test_a_v_prefixed_release_tag_also_counts_as_released(tmp_path: Path) -> None:
    """Both tag spellings are accepted elsewhere in this workflow."""
    out, _ = _run(
        tmp_path,
        version="1.13.6",
        old_manifest='version = "1.13.6"',
        released_tags="v1.13.6",
    )

    assert WARNING_MARKER not in out


def test_a_real_bump_proceeds_and_reports_the_transition(tmp_path: Path) -> None:
    """Guards the `version bumped: old -> new` line, which sits next to the
    warning branch and is easy to drop when editing around it."""
    out, gh_output = _run(
        tmp_path,
        version="1.13.6",
        old_manifest='version = "1.13.5"',
        released_tags="1.13.5",
    )

    assert "version bumped: 1.13.5 -> 1.13.6" in out
    assert WARNING_MARKER not in out, "a bump is releasing right now, not stuck"
    assert "should_release=true" in gh_output


def test_manual_dispatch_still_short_circuits(tmp_path: Path) -> None:
    out, gh_output = _run(
        tmp_path,
        version="1.13.6",
        old_manifest='version = "1.13.6"',
        released_tags="",
        event_name="workflow_dispatch",
    )

    assert "should_release=true" in gh_output
    assert WARNING_MARKER not in out


def test_a_tag_push_is_unaffected(tmp_path: Path) -> None:
    out, gh_output = _run(
        tmp_path,
        version="1.13.6",
        old_manifest='version = "1.13.6"',
        released_tags="",
        ref_type="tag",
    )

    assert "should_release=true" in gh_output
    assert WARNING_MARKER not in out
