"""Every internal link in the docs must resolve.

`docs/CLAUDE.md` promises an agent can reach any feature from `CLAUDE.md` in at
most three hops. That guarantee is only as good as the links, and links rot
silently: nothing fails when a heading is renamed or a file moves between
crates. Twenty-two were already broken when this test was written, most of them
fallout from the crate splits -- `crates/zccache/proto/...` after the proto
moved to `zccache-protocol`, `src/audit.rs` after it moved to `zccache-audit`,
and an index entry still advertising `ZCCACHE_FALLBACK`, a knob removed with
the uncached fallback itself.

Only tracked files are scanned, and `vendor/` is excluded: that is third-party
source we do not edit.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

MARKDOWN_LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
HEADING = re.compile(r"^(#{1,6})\s+(.*)$")
EXCLUDED_PREFIXES = ("vendor/", ".claude/worktrees/")
SKIPPED_SCHEMES = ("http://", "https://", "mailto:", "#")


def _slug(heading: str) -> str:
    """GitHub's anchor algorithm.

    Two details matter and are easy to get wrong: underscores survive (only
    backticks and emphasis are stripped), and *each* space becomes a hyphen, so
    a heading containing `&` yields a double hyphen once the `&` is dropped.
    """
    text = heading.strip().lower()
    text = re.sub(r"[`*]", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)  # inline link -> its text
    text = re.sub(r"[^\w\s-]", "", text)
    return re.sub(r"\s", "-", text)


def _anchors(path: Path) -> set[str]:
    found: set[str] = set()
    seen: dict[str, int] = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        match = HEADING.match(line)
        if not match:
            continue
        base = _slug(match.group(2))
        count = seen.get(base, 0)
        seen[base] = count + 1
        # GitHub disambiguates repeats with -1, -2, ...
        found.add(base if count == 0 else f"{base}-{count}")
    return found


def _tracked_markdown() -> list[Path]:
    listed = subprocess.run(
        ["git", "ls-files", "*.md"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return [
        ROOT / rel
        for rel in listed.stdout.splitlines()
        if rel and not rel.startswith(EXCLUDED_PREFIXES)
    ]


def _broken_links() -> list[str]:
    anchors_by_file: dict[Path, set[str]] = {}
    broken: list[str] = []

    for source in _tracked_markdown():
        if not source.exists():
            continue
        for raw in MARKDOWN_LINK.findall(
            source.read_text(encoding="utf-8", errors="replace")
        ):
            if raw.startswith(SKIPPED_SCHEMES) or raw.startswith("/"):
                continue
            target, _, fragment = raw.partition("#")
            destination = (source.parent / target).resolve() if target else source
            where = source.relative_to(ROOT).as_posix()
            if not destination.exists():
                broken.append(f"{where} -> {raw} (missing file)")
                continue
            if fragment and destination.suffix == ".md":
                if destination not in anchors_by_file:
                    anchors_by_file[destination] = _anchors(destination)
                if fragment.lower() not in anchors_by_file[destination]:
                    broken.append(f"{where} -> {raw} (missing anchor)")
    return broken


def test_no_broken_internal_doc_links() -> None:
    broken = _broken_links()

    assert not broken, "broken internal doc links:\n  " + "\n  ".join(broken)


def test_the_slug_algorithm_matches_github() -> None:
    """Guards the two rules above; getting either wrong makes this test report
    false breakage, which is worse than not having it."""
    assert _slug("Standalone daemon identity, deployment & lifecycle") == (
        "standalone-daemon-identity-deployment--lifecycle"
    )
    assert _slug("Host no-spawn guard (`ZCCACHE_NO_SPAWN`)") == (
        "host-no-spawn-guard-zccache_no_spawn"
    )
    assert _slug("Why is zccache so much faster?") == "why-is-zccache-so-much-faster"


def test_duplicate_headings_get_suffixed_anchors(tmp_path: Path) -> None:
    page = tmp_path / "page.md"
    page.write_text("# Notes\n\n## Notes\n\n### Notes\n", encoding="utf-8")

    assert _anchors(page) == {"notes", "notes-1", "notes-2"}
