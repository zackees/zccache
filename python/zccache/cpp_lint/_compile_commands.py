"""Parser for `compile_commands.json`.

Extracts the (TU, flags) pairs the runners need. compile_commands.json
is a JSON array of `{directory, file, arguments}` (or `command`)
entries; we normalize both into `(absolute_file_path, list[str] flags)`.
"""

from __future__ import annotations

import json
import shlex
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class CompileEntry:
    """One compile_commands.json entry, normalized."""

    file: Path
    directory: Path
    arguments: tuple[str, ...]


def parse_compile_commands(path: Path) -> tuple[CompileEntry, ...]:
    """Parse `path` and return one CompileEntry per TU.

    Raises FileNotFoundError if the file is missing, ValueError on
    structural problems (per Clang's compile_commands.json spec).
    """
    if not path.is_file():
        raise FileNotFoundError(f"compile_commands.json not found: {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ValueError(f"{path}: invalid JSON: {exc}") from exc
    if not isinstance(data, list):
        raise ValueError(f"{path}: top level must be a JSON array")

    out: list[CompileEntry] = []
    for i, entry in enumerate(data):
        if not isinstance(entry, dict):
            raise ValueError(f"{path}[{i}]: entry must be a mapping")
        file_str = entry.get("file")
        directory_str = entry.get("directory")
        if not isinstance(file_str, str):
            raise ValueError(f"{path}[{i}]: missing or non-string `file`")
        if not isinstance(directory_str, str):
            raise ValueError(f"{path}[{i}]: missing or non-string `directory`")
        directory = Path(directory_str)
        file = Path(file_str)
        if not file.is_absolute():
            file = (directory / file).resolve()

        arguments: tuple[str, ...]
        if "arguments" in entry:
            args = entry["arguments"]
            if not isinstance(args, list):
                raise ValueError(f"{path}[{i}]: `arguments` must be a list")
            arguments = tuple(str(a) for a in args)
        elif "command" in entry:
            cmd = entry["command"]
            if not isinstance(cmd, str):
                raise ValueError(f"{path}[{i}]: `command` must be a string")
            arguments = tuple(shlex.split(cmd, posix=True))
        else:
            raise ValueError(
                f"{path}[{i}]: must have either `arguments` or `command`"
            )

        out.append(
            CompileEntry(file=file, directory=directory, arguments=arguments)
        )
    return tuple(out)


__all__ = ["CompileEntry", "parse_compile_commands"]
