"""Verify the zccache protobuf wire is backward-compatible.

Implements zackees/zccache#693 Phase 1: extracts every `(message, field_number) ->
(field_name, field_type)` tuple from the zccache proto files and compares the
result against `ci/wire_stability_snapshot.txt`. Every live field must appear
in the snapshot, so adding a field requires regenerating that ledger. Removing
a field requires both its number and name to remain reserved; renumbering or
type-changing an existing field is **not** allowed — it breaks deployed clients
reading the wire.

The snapshot file is the canonical contract. To intentionally amend it, run
this script with `--write-snapshot` and commit the diff along with a comment
that explains the change and bumps `WIRE_STABILITY.md` if needed.

The script also enforces the contract documented in `docs/WIRE_STABILITY.md`:
    - oneof variant tags never renumber (a oneof variant is a field).
    - field types never change (e.g. `string` -> `bytes` is breaking).
    - enum value names + numbers never renumber.

Usage:
    uv run python ci/check_wire_stability.py            # verify (CI mode)
    uv run python ci/check_wire_stability.py --write-snapshot   # regenerate
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parent.parent
SNAPSHOT_PATH = REPO_ROOT / "ci" / "wire_stability_snapshot.txt"
PROTO_PATHS: tuple[Path, ...] = (
    REPO_ROOT / "crates" / "zccache-protocol" / "proto" / "zccache_v1.proto",
    REPO_ROOT / "crates" / "zccache-artifact" / "src" / "rust_plan_manifest.proto",
)


# --- proto parser -----------------------------------------------------------
#
# We deliberately do not pull in a full protobuf grammar (protoc, buf, etc) so
# this guard runs with stdlib-only Python on every supported platform. The
# parser only needs to extract `(message, field_number) -> (name, type)`
# tuples plus enum value names + numbers. The proto files in this repo use a
# subset of proto3 syntax that a regex parser handles deterministically:
#   - top-level `message Foo { ... }` and `enum Foo { ... }` blocks
#   - nested oneof blocks (variants are fields)
#   - simple field lines: [optional|repeated] <type> <name> = <number>;
# We reject the file with a parse error if we encounter syntax we don't model
# so the guard never silently misses a field.

MESSAGE_OPEN_RE = re.compile(r"^\s*message\s+([A-Za-z_]\w*)\s*\{\s*$")
MESSAGE_INLINE_EMPTY_RE = re.compile(r"^\s*message\s+([A-Za-z_]\w*)\s*\{\s*\}\s*$")
ENUM_OPEN_RE = re.compile(r"^\s*enum\s+([A-Za-z_]\w*)\s*\{\s*$")
ONEOF_OPEN_RE = re.compile(r"^\s*oneof\s+([A-Za-z_]\w*)\s*\{\s*$")
FIELD_RE = re.compile(
    r"""^\s*
        (?:(?P<label>optional|repeated)\s+)?
        # `map<K, V>` must come first so the identifier branch does
        # not eat the bare keyword. Both K and V are restricted to the
        # protobuf scalar types we use today (uint{32,64}, int{32,64},
        # string, bool); extend if a future proto needs more.
        (?P<type>
            map<\s*[A-Za-z_]\w*\s*,\s*[A-Za-z_][\w.]*\s*>
            |
            [A-Za-z_][\w.]*
        )\s+
        (?P<name>[A-Za-z_]\w*)\s*=\s*
        (?P<number>\d+)
        \s*(?:\[[^]]*\])?
        \s*;\s*$""",
    re.VERBOSE,
)
ENUM_VALUE_RE = re.compile(r"^\s*(?P<name>[A-Z_][A-Z0-9_]*)\s*=\s*(?P<number>\d+)\s*;\s*$")
RESERVED_NUMBERS_RE = re.compile(
    r"^\s*reserved\s+(?P<ranges>\d+(?:\s+to\s+\d+)?(?:\s*,\s*\d+(?:\s+to\s+\d+)?)*?)\s*;\s*$"
)
RESERVED_NAMES_RE = re.compile(
    r'^\s*reserved\s+"(?P<first>[^"]+)"(?:\s*,\s*"(?P<rest>[^"]+)")*\s*;\s*$'
)
BLOCK_END_RE = re.compile(r"^\s*\}\s*$")
COMMENT_OR_BLANK_RE = re.compile(r"^\s*(//.*)?$")


class ProtoParseError(RuntimeError):
    """Raised when a .proto file uses syntax this guard does not model.

    Bias toward failing closed — silently skipping a field would defeat the
    point of the contract.
    """


@dataclass
class ProtoSchema:
    """Parsed fields plus the number/name tombstones declared by a proto."""

    fields: dict[str, dict[int, tuple[str, str]]] = field(default_factory=dict)
    reserved_numbers: dict[str, set[int]] = field(default_factory=dict)
    reserved_names: dict[str, set[str]] = field(default_factory=dict)

    def safely_retires(self, message: str, number: int, name: str) -> bool:
        """Whether a removed snapshot field has both required tombstones."""
        return (
            number in self.reserved_numbers.get(message, set())
            and name in self.reserved_names.get(message, set())
        )


def parse_proto(path: Path) -> ProtoSchema:
    """Return fields plus reserved names/numbers for a parsed proto.

    `qualified_name` is the message or enum name (no nested-type support is
    needed today since the zccache protos are flat). Oneof variants are
    flattened into the parent message's field table — they share the field
    number space per protobuf semantics.
    """
    contents = ProtoSchema()
    stack: list[tuple[str, dict[int, tuple[str, str]]]] = []

    with path.open(encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, start=1):
            line = raw.rstrip("\n")
            if COMMENT_OR_BLANK_RE.match(line):
                continue
            if line.lstrip().startswith(("syntax", "package", "import", "option")):
                continue

            if m := MESSAGE_INLINE_EMPTY_RE.match(line):
                # `message Foo {}` — register the name with an empty field table.
                contents.fields.setdefault(m.group(1), {})
                continue
            if m := MESSAGE_OPEN_RE.match(line):
                name = m.group(1)
                table = contents.fields.setdefault(name, {})
                stack.append((name, table))
                continue
            if m := ENUM_OPEN_RE.match(line):
                name = m.group(1)
                table = contents.fields.setdefault(name, {})
                stack.append((name, table))
                continue
            if m := ONEOF_OPEN_RE.match(line):
                # variant fields share the parent's number space
                if not stack:
                    raise ProtoParseError(f"{path}:{lineno}: oneof outside any message")
                stack.append((f"{stack[-1][0]}.<oneof>", stack[-1][1]))
                continue
            if BLOCK_END_RE.match(line):
                if not stack:
                    raise ProtoParseError(f"{path}:{lineno}: unmatched '}}'")
                stack.pop()
                continue

            if not stack:
                raise ProtoParseError(f"{path}:{lineno}: unexpected line at file scope: {line!r}")
            parent_name, table = stack[-1]

            if m := RESERVED_NUMBERS_RE.match(line):
                if parent_name.endswith(".<oneof>"):
                    raise ProtoParseError(f"{path}:{lineno}: reserved declaration inside oneof")
                reserved = contents.reserved_numbers.setdefault(parent_name, set())
                for declared_range in m.group("ranges").split(","):
                    bounds = [int(value.strip()) for value in declared_range.split("to")]
                    reserved.update(range(bounds[0], bounds[-1] + 1))
                continue
            if m := RESERVED_NAMES_RE.match(line):
                if parent_name.endswith(".<oneof>"):
                    raise ProtoParseError(f"{path}:{lineno}: reserved declaration inside oneof")
                contents.reserved_names.setdefault(parent_name, set()).update(
                    re.findall(r'"([^"]+)"', line)
                )
                continue

            if m := FIELD_RE.match(line):
                number = int(m.group("number"))
                name = m.group("name")
                field_type = m.group("type")
                if number in table and table[number] != (name, field_type):
                    raise ProtoParseError(f"{path}:{lineno}: field number {number} reused in {parent_name}: {table[number]} vs ({name}, {field_type})")
                table[number] = (name, field_type)
                continue
            if m := ENUM_VALUE_RE.match(line):
                number = int(m.group("number"))
                name = m.group("name")
                if number in table and table[number] != (name, "<enum>"):
                    raise ProtoParseError(f"{path}:{lineno}: enum value {number} reused in {parent_name}: {table[number]} vs ({name}, <enum>)")
                table[number] = (name, "<enum>")
                continue

            raise ProtoParseError(f"{path}:{lineno}: unparseable line — extend the guard or simplify the proto:\n  {line!r}")

    if stack:
        raise ProtoParseError(f"{path}: file ended with {len(stack)} open block(s)")
    return contents


def parse_proto_files(paths: Iterable[Path]) -> ProtoSchema:
    merged = ProtoSchema()
    for path in paths:
        parsed = parse_proto(path)
        for name, table in parsed.fields.items():
            target = merged.fields.setdefault(name, {})
            for number, value in table.items():
                if number in target and target[number] != value:
                    raise ProtoParseError(
                        f"{path}: {name}.{number} disagrees across proto files: "
                        f"{target[number]} vs {value}"
                    )
                target[number] = value
        for name, numbers in parsed.reserved_numbers.items():
            merged.reserved_numbers.setdefault(name, set()).update(numbers)
        for name, names in parsed.reserved_names.items():
            merged.reserved_names.setdefault(name, set()).update(names)
    return merged


# --- snapshot file ----------------------------------------------------------

SNAPSHOT_HEADER = """\
# zccache wire stability snapshot (zackees/zccache#693 Phase 1).
#
# This file is the canonical record of every protobuf field number, name,
# and type that has shipped on the wire. The CI guard
# `ci/check_wire_stability.py` rejects PRs that renumber or type-change any
# tuple listed here. Retiring a field requires reserving both its number and
# name in the proto; `--write-snapshot` retains that historical record. Every
# live field must be recorded here, so adding a field requires updating it.
#
# When making an intentional, documented wire change (typically a
# `PROTOCOL_VERSION` bump), regenerate this file with:
#     uv run python ci/check_wire_stability.py --write-snapshot
# and explain the change in the commit message + docs/WIRE_STABILITY.md.
#
# Format: `<message_or_enum_name>\\t<field_number>\\t<field_name>\\t<field_type>`
# where field_type is `<enum>` for enum variants.
"""


def snapshot_lines(contents: dict[str, dict[int, tuple[str, str]]]) -> list[str]:
    lines: list[str] = []
    for name in sorted(contents):
        for number in sorted(contents[name]):
            field_name, field_type = contents[name][number]
            lines.append(f"{name}\t{number}\t{field_name}\t{field_type}")
    return lines


def write_snapshot(contents: ProtoSchema) -> None:
    """Record every live field while retaining safely retired history."""
    snapshot = read_snapshot() if SNAPSHOT_PATH.exists() else {}
    merged = {name: dict(fields) for name, fields in contents.fields.items()}
    for name, fields in snapshot.items():
        for number, field_info in fields.items():
            if (
                number not in merged.get(name, {})
                and contents.safely_retires(name, number, field_info[0])
            ):
                merged.setdefault(name, {})[number] = field_info

    body = SNAPSHOT_HEADER + "\n".join(snapshot_lines(merged)) + "\n"
    SNAPSHOT_PATH.write_text(body, encoding="utf-8")


def read_snapshot() -> dict[str, dict[int, tuple[str, str]]]:
    if not SNAPSHOT_PATH.exists():
        raise FileNotFoundError(f"missing wire stability snapshot at {SNAPSHOT_PATH}; run `uv run python ci/check_wire_stability.py --write-snapshot` to create the baseline.")
    snapshot: dict[str, dict[int, tuple[str, str]]] = {}
    with SNAPSHOT_PATH.open(encoding="utf-8") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            parts = line.split("\t")
            if len(parts) != 4:
                raise RuntimeError(f"malformed snapshot line: {line!r}")
            name, number_str, field_name, field_type = parts
            snapshot.setdefault(name, {})[int(number_str)] = (field_name, field_type)
    return snapshot


# --- diff & report ----------------------------------------------------------


def diff_against_snapshot(
    snapshot: dict[str, dict[int, tuple[str, str]]],
    current: ProtoSchema,
) -> list[str]:
    violations: list[str] = []
    for name in sorted(snapshot):
        if name not in current.fields:
            violations.append(f"REMOVED message/enum: {name}")
            continue
        for number in sorted(snapshot[name]):
            snap_value = snapshot[name][number]
            curr_value = current.fields[name].get(number)
            if curr_value is None:
                if not current.safely_retires(name, number, snap_value[0]):
                    violations.append(f"REMOVED field: {name}.{number} ({snap_value[0]}: {snap_value[1]})")
            elif curr_value != snap_value:
                violations.append(f"CHANGED field: {name}.{number}: {snap_value} -> {curr_value}")
    for name in sorted(current.fields):
        snapshot_fields = snapshot.get(name, {})
        for number in sorted(current.fields[name]):
            if number not in snapshot_fields:
                field_name, field_type = current.fields[name][number]
                violations.append(
                    f"UNTRACKED field: {name}.{number} ({field_name}: {field_type})"
                )
    return violations


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write-snapshot",
        action="store_true",
        help="record all live fields and retain safely dual-reserved tombstones",
    )
    args = parser.parse_args(argv)

    try:
        current = parse_proto_files(PROTO_PATHS)
    except ProtoParseError as exc:
        print(f"proto parse error: {exc}", file=sys.stderr)
        return 2

    if args.write_snapshot:
        write_snapshot(current)
        print(f"wrote {SNAPSHOT_PATH.relative_to(REPO_ROOT)}")
        return 0

    try:
        snapshot = read_snapshot()
    except FileNotFoundError as exc:
        print(str(exc), file=sys.stderr)
        return 2

    violations = diff_against_snapshot(snapshot, current)
    if violations:
        print("Wire stability violations:", file=sys.stderr)
        for v in violations:
            print(f"  {v}", file=sys.stderr)
        print(
            "\nThe zccache wire is a frozen contract (see docs/WIRE_STABILITY.md).\n"
            "If this change is intentional and you understand the compatibility\n"
            "impact, regenerate the snapshot with:\n"
            "  uv run python ci/check_wire_stability.py --write-snapshot\n"
            "and document the bump in the commit message.",
            file=sys.stderr,
        )
        return 1

    print(f"wire stability OK: {sum(len(t) for t in current.fields.values())} fields across {len(current.fields)} message/enum types")
    return 0


if __name__ == "__main__":
    sys.exit(main())
