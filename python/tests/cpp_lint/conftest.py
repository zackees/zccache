"""Shared fixtures for the cpp_lint test suite."""

from __future__ import annotations

import json
import os
import stat
import sys
from pathlib import Path

import pytest


@pytest.fixture
def compile_commands_for(tmp_path: Path):
    """Return a factory that writes a compile_commands.json for given TUs.

    Usage:
        cc = compile_commands_for([tu_a, tu_b])
        # cc is a Path to a valid compile_commands.json
    """

    def factory(tus: list[Path], extra_args: tuple[str, ...] = ()) -> Path:
        cc_path = tmp_path / "compile_commands.json"
        entries = []
        for tu in tus:
            entries.append(
                {
                    "directory": str(tu.parent),
                    "file": str(tu),
                    "arguments": ["clang++", "-c", str(tu), *list(extra_args)],
                }
            )
        cc_path.write_text(json.dumps(entries), encoding="utf-8")
        return cc_path

    return factory


@pytest.fixture
def write_tu(tmp_path: Path):
    """Return a factory: write_tu("foo.cpp", body) -> Path."""

    def factory(name: str, body: str = "int main() { return 0; }\n") -> Path:
        path = tmp_path / name
        path.write_text(body, encoding="utf-8")
        return path

    return factory


@pytest.fixture
def stub_clang_query(tmp_path: Path) -> Path:
    """Drop a fake `clang-query` executable that emits a single canned hit.

    Output mimics `clang-query` with `set output diag`:

        /path/to/tu.cpp:1:1: note: "<bind>" binds here

    The bind name is parsed from the script piped on stdin so different
    AstQuery names produce different hits. If stdin has no `# AstQuery
    <name>` line, no hits are emitted.
    """
    script = tmp_path / ("stub_clang_query.bat" if sys.platform == "win32" else "stub_clang_query")
    if sys.platform == "win32":
        # Use Python wrapper to keep parsing portable.
        py_helper = tmp_path / "stub_clang_query_helper.py"
        py_helper.write_text(_STUB_HELPER, encoding="utf-8")
        script.write_text(
            f'@echo off\r\n"{sys.executable}" "{py_helper}" %*\r\n',
            encoding="utf-8",
        )
    else:
        py_helper = tmp_path / "stub_clang_query_helper.py"
        py_helper.write_text(_STUB_HELPER, encoding="utf-8")
        script.write_text(
            f'#!/usr/bin/env bash\nexec "{sys.executable}" "{py_helper}" "$@"\n',
            encoding="utf-8",
        )
        _make_executable(script)
    return script


@pytest.fixture
def stub_iwyu(tmp_path: Path) -> Path:
    """Drop a fake `include-what-you-use` that emits one add + one keep."""
    if sys.platform == "win32":
        py_helper = tmp_path / "stub_iwyu_helper.py"
        py_helper.write_text(_IWYU_HELPER, encoding="utf-8")
        script = tmp_path / "stub_iwyu.bat"
        script.write_text(
            f'@echo off\r\n"{sys.executable}" "{py_helper}" %* 1>&2\r\n',
            encoding="utf-8",
        )
    else:
        py_helper = tmp_path / "stub_iwyu_helper.py"
        py_helper.write_text(_IWYU_HELPER, encoding="utf-8")
        script = tmp_path / "stub_iwyu"
        script.write_text(
            f'#!/usr/bin/env bash\nexec "{sys.executable}" "{py_helper}" "$@" 1>&2\n',
            encoding="utf-8",
        )
        _make_executable(script)
    return script


def _make_executable(path: Path) -> None:
    st = os.stat(path)
    os.chmod(path, st.st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


_STUB_HELPER = r'''
"""Fake clang-query for cpp_lint tests.

Reads the matcher script from stdin, extracts every `# AstQuery <name>`
line, and emits one hit per query against the TU passed as the final
positional argument. Exits 0.
"""
import re
import sys
from pathlib import Path


def main():
    argv = sys.argv[1:]
    tu = Path(argv[-1]).resolve()
    script = sys.stdin.read()
    names = re.findall(r"# AstQuery (\S+)", script)
    for name in names:
        sys.stdout.write("void " + name + "() {\n")
        sys.stdout.write(
            str(tu) + ':1:1: note: "' + name + '" binds here\n'
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
'''


_IWYU_HELPER = r'''
"""Fake include-what-you-use for cpp_lint tests."""

import sys
from pathlib import Path


def main():
    argv = sys.argv[1:]
    tu = None
    for arg in argv:
        if arg == "--":
            break
        if arg.startswith("-"):
            continue
        if arg.endswith((".cpp", ".cc", ".cxx", ".c", ".h", ".hpp")):
            tu = arg
            break
    if tu is None:
        return 0
    tu = str(Path(tu).resolve())
    sys.stderr.write(tu + " should add these lines:\n")
    sys.stderr.write("#include <string>  // for std::string\n\n")
    sys.stderr.write(tu + " should remove these lines:\n\n")
    sys.stderr.write("The full include-list for " + tu + ":\n")
    sys.stderr.write("#include <string>  // for std::string\n")
    sys.stderr.write("---\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
'''
