#!/usr/bin/env python3
"""Inventory compile-handler coupling and reject new glob imports."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SERVER = Path("crates/zccache-daemon-core/src/daemon/server")
COMPILE_ROOT_FILES = {
    "handle_compile.rs",
    "handle_compile_ephemeral.rs",
    "handle_compile_multi.rs",
    "handle_compile_multi_args.rs",
    "handle_compile_multi_preflight.rs",
    "handle_compile_multi_staged.rs",
    "handle_compile_multi_types.rs",
}
# Production glob imports only. Test-module globs (`use super::*;` inside
# `#[cfg(test)] mod tests`) are excluded by strip_test_items, so they must
# never be listed here -- an entry that no longer corresponds to a real
# production glob silently exempts that file from the check, which is how
# cached_hit.rs / error_cache.rs / handle_compile_multi_args.rs ended up
# here. test_allowlist_has_no_dead_entries guards against that recurring.
ALLOWED_GLOB_IMPORTS = {
    "handle_compile.rs",
    "handle_compile_ephemeral.rs",
    "handle_compile_multi.rs",
    "handle_compile_multi_preflight.rs",
    "handle_compile_multi_staged.rs",
    "handle_compile_multi_types.rs",
}
GLOB_IMPORT = re.compile(r"^\s*use\s+(?:super|crate::daemon::server)::\*;", re.MULTILINE)
PRIVATE_ITEM = re.compile(
    r"\bpub\(super\)\s+(?:async\s+)?"
    r"(?:struct|enum|trait|fn|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)"
)
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
PATH_ATTR = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]')


def strip_test_items(text: str) -> str:
    """Drop `#[cfg(test)]` items so test code is not scanned as production code.

    A `use super::*;` inside `#[cfg(test)] mod tests { ... }` is the ordinary
    Rust idiom for reaching a module's own items from its test module. It is
    not compile-handler coupling, and treating it as such is what pushed three
    test-only files into ALLOWED_GLOB_IMPORTS -- which then had the side effect
    of exempting those files from the check for real.

    Brace counting ignores braces inside string literals, the same simplifying
    assumption `module_private_items` already makes.
    """
    lines = text.splitlines()
    kept: list[str] = []
    index = 0
    total = len(lines)
    while index < total:
        if not CFG_TEST.match(lines[index]):
            kept.append(lines[index])
            index += 1
            continue
        # Skip the attribute plus any attributes stacked under it.
        index += 1
        while index < total and lines[index].lstrip().startswith("#["):
            index += 1
        if index >= total:
            break
        # `mod name;` declares a whole test-only file; nothing to brace-match.
        code = lines[index].split("//", 1)[0]
        if "{" not in code:
            index += 1
            continue
        depth = 0
        while index < total:
            code = lines[index].split("//", 1)[0]
            depth += code.count("{") - code.count("}")
            index += 1
            if depth <= 0:
                break
    return "\n".join(kept)


def test_only_module_files(path: Path) -> set[Path]:
    """Files pulled in as `#[cfg(test)] #[path = "x.rs"] mod ...`.

    Such a file is entirely test code, so its top-level globs are test globs.
    Detected structurally rather than by a `*_tests.rs` naming convention, so
    renaming a file cannot silently re-arm the false positive.
    """
    found: set[Path] = set()
    lines = path.read_text(encoding="utf-8").splitlines()
    for position, line in enumerate(lines):
        if not CFG_TEST.match(line):
            continue
        for lookahead in lines[position + 1 : position + 4]:
            match = PATH_ATTR.match(lookahead)
            if match:
                found.add((path.parent / match.group(1)).resolve())
            if lookahead.lstrip().startswith("mod "):
                break
    return found



def compile_files(root: Path) -> list[Path]:
    server = root / SERVER
    files = [server / name for name in sorted(COMPILE_ROOT_FILES)]
    files.extend(sorted((server / "handle_compile").rglob("*.rs")))
    return files


def module_private_items(text: str) -> set[str]:
    """Find module-level private items while excluding impl/test members."""
    items: set[str] = set()
    depth = 0
    for line in text.splitlines():
        code = line.split("//", 1)[0]
        if depth == 0:
            items.update(PRIVATE_ITEM.findall(code))
        depth += code.count("{") - code.count("}")
        depth = max(depth, 0)
    return items


def inventory(root: Path) -> dict[str, object]:
    server = root / SERVER
    files = compile_files(root)
    # Files that exist only as test modules are test code end to end, so their
    # top-level globs are test globs too.
    test_only: set[Path] = set()
    for path in files:
        test_only |= test_only_module_files(path)
    glob_files = [
        path.relative_to(server).as_posix()
        for path in files
        if path.resolve() not in test_only
        and GLOB_IMPORT.search(strip_test_items(path.read_text(encoding="utf-8")))
    ]
    private_symbols: set[str] = set()
    compile_set = {path.resolve() for path in files}
    for path in server.rglob("*.rs"):
        if path.resolve() not in compile_set:
            private_symbols.update(module_private_items(path.read_text(encoding="utf-8")))
    references: dict[str, list[str]] = {}
    for path in files:
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(server).as_posix()
        for symbol in private_symbols:
            if re.search(rf"\b{re.escape(symbol)}\b", text):
                references.setdefault(symbol, []).append(relative)
    unexpected = sorted(set(glob_files) - ALLOWED_GLOB_IMPORTS)
    return {
        "schema_version": 1,
        "compile_files": len(files),
        "glob_imports": sorted(glob_files),
        "allowed_glob_imports": sorted(ALLOWED_GLOB_IMPORTS),
        "unexpected_glob_imports": unexpected,
        "server_private_references": {key: sorted(value) for key, value in sorted(references.items())},
        "server_private_symbol_count": len(references),
        "shared_state_reference_files": sorted(path.relative_to(server).as_posix() for path in files if re.search(r"\bSharedState\b", path.read_text(encoding="utf-8"))),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--deny-all-globs", action="store_true")
    args = parser.parse_args()
    result = inventory(args.root)
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")
    if result["unexpected_glob_imports"]:
        return 1
    if args.deny_all_globs and result["glob_imports"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
