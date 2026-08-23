"""Commands in the docs must name things that exist.

A wrong `-p <crate>` is a special kind of bad documentation: it looks
authoritative, and following it produces either "package not found" or -- worse
-- a command that succeeds while doing nothing. `cargo bench -p zccache-hash`
exits 0 and benchmarks nothing, because that crate has no bench targets; every
bench in the workspace lives in `crates/zccache`.

Four such commands were live when this was written, in `CLAUDE.md`, `CODEX.md`,
`bench/persist-rust-project/README.md`, and -- most pointedly --
`crates/zccache/benches/README.md`, which told you to bench a different crate
than the one it sits in.

`tasks/` is excluded: it is a historical log, and some entries deliberately
quote commands that failed at the time.
"""

from __future__ import annotations

import re
import subprocess
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

PACKAGE_FLAG = re.compile(r"-p (zccache[a-z0-9-]*)")
BENCH_COMMAND = re.compile(r"cargo bench[^\n`]*?-p (zccache[a-z0-9-]*)")
EXCLUDED_PREFIXES = ("vendor/", ".claude/worktrees/", "tasks/")


def _workspace_members() -> set[str]:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    return {
        Path(member).name
        for member in manifest["workspace"]["members"]
        if member.startswith("crates/")
    }


def _tracked_markdown() -> list[Path]:
    listed = subprocess.run(
        ["git", "ls-files", "*.md"], cwd=ROOT, capture_output=True, text=True, check=True
    )
    return [
        ROOT / rel
        for rel in listed.stdout.splitlines()
        if rel and not rel.startswith(EXCLUDED_PREFIXES)
    ]


def _crates_with_bench_targets() -> set[str]:
    with_benches = set()
    for member in _workspace_members():
        manifest = ROOT / "crates" / member / "Cargo.toml"
        if not manifest.is_file():
            continue
        text = manifest.read_text(encoding="utf-8")
        if "[[bench]]" in text or (ROOT / "crates" / member / "benches").is_dir():
            with_benches.add(member)
    return with_benches


def test_documented_package_flags_name_real_crates() -> None:
    members = _workspace_members()
    bad: list[str] = []
    for doc in _tracked_markdown():
        for crate in PACKAGE_FLAG.findall(doc.read_text(encoding="utf-8", errors="replace")):
            if crate not in members:
                bad.append(f"{doc.relative_to(ROOT).as_posix()} -> -p {crate}")

    assert not bad, "docs reference crates that do not exist:\n  " + "\n  ".join(sorted(set(bad)))


def test_documented_bench_commands_target_a_crate_that_has_benches() -> None:
    """The failure this catches is silent: `cargo bench` on a crate with no
    bench targets exits 0 and measures nothing."""
    benched = _crates_with_bench_targets()
    bad: list[str] = []
    for doc in _tracked_markdown():
        for crate in BENCH_COMMAND.findall(doc.read_text(encoding="utf-8", errors="replace")):
            if crate not in benched:
                bad.append(f"{doc.relative_to(ROOT).as_posix()} -> cargo bench -p {crate}")

    assert not bad, (
        "docs bench crates with no bench targets (would exit 0 and measure nothing):\n  "
        + "\n  ".join(sorted(set(bad)))
    )


def test_the_workspace_actually_has_benches_somewhere() -> None:
    """Guards the test above from passing vacuously if benches were removed."""
    assert _crates_with_bench_targets(), "no crate declares bench targets"
