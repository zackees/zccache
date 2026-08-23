"""`crates/CLAUDE.md` must describe the crates that actually exist.

This file is loaded into an agent's context whenever it works under `crates/`,
so a wrong statement here is worse than a missing one -- it is confidently
wrong. When written, it claimed 21 crates (there are 22), listed three
`zccache-download-*` crates that do not exist (the client and daemon are
modules of `zccache-cli-core`; the CLI is a bin of `zccache`), described
`zccache-ci` and `zccache-daemon` as crates when both are bin targets, and
omitted five real crates including `zccache-cli-core` and `zccache-daemon-core`
-- the two the #1018/#1022 splits created.

None of that is caught by the compiler, so it is checked here.
"""

from __future__ import annotations

import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CRATES_DOC = ROOT / "crates" / "CLAUDE.md"
ROOT_DOC = ROOT / "CLAUDE.md"

# Bolded leading entries in the responsibilities list, e.g. `- **zccache-hash** — ...`
RESPONSIBILITY = re.compile(r"^- \*\*(zccache[a-z-]*)\*\*", re.MULTILINE)
COUNT_CLAIM = re.compile(r"^(\d+) crates split", re.MULTILINE)


def _workspace_members() -> set[str]:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    members = manifest["workspace"]["members"]
    return {Path(m).name for m in members if m.startswith("crates/")}


def _documented() -> set[str]:
    section = CRATES_DOC.read_text(encoding="utf-8")
    start = section.index("## Crate Responsibilities")
    end = section.index("## Key Design Patterns")
    return set(RESPONSIBILITY.findall(section[start:end]))


def test_every_documented_crate_exists() -> None:
    """Guards against describing bin targets, or removed crates, as crates."""
    phantom = sorted(_documented() - _workspace_members())

    assert not phantom, f"crates/CLAUDE.md documents non-existent crates: {phantom}"


def test_every_crate_is_documented() -> None:
    """A crate nobody documented is one an agent will not know to look in."""
    undocumented = sorted(_workspace_members() - _documented())

    assert not undocumented, f"crates missing from crates/CLAUDE.md: {undocumented}"


def test_the_stated_crate_count_is_right() -> None:
    match = COUNT_CLAIM.search(CRATES_DOC.read_text(encoding="utf-8"))
    assert match, "crates/CLAUDE.md no longer states a crate count"

    assert int(match.group(1)) == len(_workspace_members())


def test_the_root_doc_agrees_on_the_crate_count() -> None:
    """`CLAUDE.md` states the same number in its opening sentence, and is loaded
    every session, so it drifts independently."""
    stated = re.search(r"compiler cache \((\d+) crates\)", ROOT_DOC.read_text(encoding="utf-8"))
    assert stated, "CLAUDE.md no longer states a crate count"

    assert int(stated.group(1)) == len(_workspace_members())
