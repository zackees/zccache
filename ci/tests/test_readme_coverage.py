"""Every directory with tracked files carries a README.md.

`CLAUDE.md` states the rule and says it is "enforced by hook", but
`ci/hooks/readme_guard.py` only fires when a file in that directory is
*edited*. A directory nobody has touched since the rule landed can sit without
one indefinitely, which is how seven of them did -- including
`perf/scenarios/build-then-check` (the one scenario of six without a README)
and `.github/actions/build-target` (a composite action used by both the dist
matrix and the release pipeline).

This closes the gap the hook cannot: it checks the whole tree rather than the
files in front of it.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# `.cargo` holds only `config.toml`; it is a repo-local cargo home, not a
# source directory, and the rest of it is generated and gitignored.
EXEMPT = {".cargo"}


def _directories_with_tracked_files() -> set[str]:
    listed = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    )
    directories = set()
    for rel in listed.stdout.splitlines():
        parent = str(Path(rel).parent).replace("\\", "/")
        if parent not in (".", ""):
            directories.add(parent)
    return directories


def test_every_directory_with_files_has_a_readme() -> None:
    missing = sorted(
        directory
        for directory in _directories_with_tracked_files()
        if directory not in EXEMPT and not (ROOT / directory / "README.md").is_file()
    )

    assert not missing, "directories without README.md:\n  " + "\n  ".join(missing)


def test_the_exemption_list_is_still_justified() -> None:
    """An exemption that stops being true is a hole nobody notices. `.cargo`
    earns its place only while `config.toml` is the sole tracked file in it."""
    listed = subprocess.run(
        ["git", "ls-files", ".cargo"], cwd=ROOT, capture_output=True, text=True, check=True
    )
    tracked = [line for line in listed.stdout.splitlines() if line]

    assert tracked == [".cargo/config.toml"], (
        f".cargo now tracks more than config.toml ({tracked}); "
        "it should either get a README.md or a narrower exemption"
    )
