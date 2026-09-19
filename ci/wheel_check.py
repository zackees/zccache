"""Check release wheels before they reach PyPI -- the same steps in CI and locally.

    uv run --no-project ci/wheel_check.py dist/wheels/*.whl
    uv run --no-project ci/wheel_check.py --pypi 1.14.0      # audit a published release

Every native binary inside a wheel must be free of embedded debug info, and
each wheel and the release as a whole must stay inside a size budget. PyPI
caps a project at 10 GB in total; from 1.13.x the aarch64-linux wheel
shipped ~55 MB of DWARF per binary because the cross-built artifacts were
"stripped" with the host's x86_64 `strip`, which failed silently. That bloat
helped fill the project until PyPI refused the 1.14.2 upload. This check makes
the regression a release failure instead of a quota surprise.

Debug info is detected from section tables, not file names:
  ELF      `.debug_*` / `.zdebug_*` sections (except `.debug_gdb_scripts`)
  Mach-O   a `__DWARF` segment (thin or fat binaries)
  PE       `.debug*` sections (including `/N` long names via the string table)
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
import urllib.request
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

MIB = 1024 * 1024
# Largest healthy 1.14.0 wheel is 16.8 MB; the unstripped aarch64 one was 42.7 MB.
MAX_WHEEL_BYTES = 24 * MIB
# Six healthy wheels total ~93 MB; 1.14.0 with the aarch64 bloat was ~119 MB.
MAX_RELEASE_BYTES = 100 * MIB
USER_AGENT = "zccache-release (https://github.com/zackees/zccache)"


# -- binary section parsing -------------------------------------------------


def _cstr(data: bytes, offset: int) -> str:
    end = data.find(b"\0", offset)
    return data[offset : end if end >= 0 else len(data)].decode("latin-1")


def elf_section_names(data: bytes) -> list[str]:
    ei_class, ei_data = data[4], data[5]
    order = "<" if ei_data == 1 else ">"
    if ei_class == 2:
        shoff = struct.unpack_from(order + "Q", data, 0x28)[0]
        shentsize, shnum, shstrndx = struct.unpack_from(order + "HHH", data, 0x3A)
        name_fmt, offset_fmt, offset_at = "I", "Q", 0x18
    else:
        shoff = struct.unpack_from(order + "I", data, 0x20)[0]
        shentsize, shnum, shstrndx = struct.unpack_from(order + "HHH", data, 0x2E)
        name_fmt, offset_fmt, offset_at = "I", "I", 0x10
    if shoff == 0 or shnum == 0 or shstrndx >= shnum:
        return []
    strtab_header = shoff + shstrndx * shentsize
    strtab_offset = struct.unpack_from(order + offset_fmt, data, strtab_header + offset_at)[0]
    names = []
    for index in range(shnum):
        header = shoff + index * shentsize
        name_offset = struct.unpack_from(order + name_fmt, data, header)[0]
        names.append(_cstr(data, strtab_offset + name_offset))
    return names


# rustc's GDB pretty-printer loader: a few bytes, not DWARF, kept by --strip-debug.
ELF_KEPT = {".debug_gdb_scripts"}

MACHO_THIN = {
    b"\xcf\xfa\xed\xfe": ("<", True),
    b"\xce\xfa\xed\xfe": ("<", False),
    b"\xfe\xed\xfa\xcf": (">", True),
    b"\xfe\xed\xfa\xce": (">", False),
}
MACHO_FAT = b"\xca\xfe\xba\xbe"
LC_SEGMENT, LC_SEGMENT_64 = 0x1, 0x19


def macho_segment_names(data: bytes, base: int = 0) -> list[str]:
    magic = data[base : base + 4]
    if magic == MACHO_FAT:
        (count,) = struct.unpack_from(">I", data, base + 4)
        names: list[str] = []
        for index in range(count):
            offset = struct.unpack_from(">I", data, base + 8 + index * 20 + 8)[0]
            names.extend(macho_segment_names(data, offset))
        return names
    order, is64 = MACHO_THIN[magic]
    ncmds = struct.unpack_from(order + "I", data, base + 16)[0]
    cursor = base + (32 if is64 else 28)
    names = []
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from(order + "II", data, cursor)
        if cmd in (LC_SEGMENT, LC_SEGMENT_64):
            names.append(_cstr(data[cursor + 8 : cursor + 24], 0))
        cursor += cmdsize
    return names


def pe_section_names(data: bytes) -> list[str]:
    (pe_offset,) = struct.unpack_from("<I", data, 0x3C)
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        return []
    coff = pe_offset + 4
    nsections = struct.unpack_from("<H", data, coff + 2)[0]
    symtab, nsymbols = struct.unpack_from("<II", data, coff + 8)
    optional_size = struct.unpack_from("<H", data, coff + 16)[0]
    table = coff + 20 + optional_size
    strings = symtab + nsymbols * 18
    names = []
    for index in range(nsections):
        raw = _cstr(data[table + index * 40 : table + index * 40 + 8], 0)
        if raw.startswith("/") and raw[1:].isdigit() and symtab:
            raw = _cstr(data, strings + int(raw[1:]))
        names.append(raw)
    return names


def debug_sections(data: bytes) -> tuple[str, list[str]] | None:
    """(format, debug section names) for a native binary, or None if not one."""
    if data[:4] == b"\x7fELF":
        names = elf_section_names(data)
        return "ELF", [n for n in names if n.startswith((".debug_", ".zdebug_")) and n not in ELF_KEPT]
    if data[:4] in MACHO_THIN or data[:4] == MACHO_FAT and len(data) > 8 and data[4:8] < b"\0\0\0\x30":
        return "Mach-O", [n for n in macho_segment_names(data) if n == "__DWARF"]
    if data[:2] == b"MZ" and len(data) > 0x40:
        return "PE", [n for n in pe_section_names(data) if n.startswith(".debug")]
    return None


# -- wheel checks -----------------------------------------------------------


@dataclass
class WheelReport:
    name: str
    size: int
    binaries: list[tuple[str, str, int]] = field(default_factory=list)
    problems: list[str] = field(default_factory=list)


def check_wheel(name: str, size: int, archive: zipfile.ZipFile) -> WheelReport:
    report = WheelReport(name, size)
    if size > MAX_WHEEL_BYTES:
        report.problems.append(f"{name} is {size / MIB:.1f} MiB; budget is {MAX_WHEEL_BYTES / MIB:.0f} MiB")
    for info in archive.infolist():
        if info.is_dir():
            continue
        with archive.open(info) as member:
            head = member.read(8)
        if not (head[:4] == b"\x7fELF" or head[:4] in MACHO_THIN or head[:4] == MACHO_FAT or head[:2] == b"MZ"):
            continue
        found = debug_sections(archive.read(info))
        if found is None:
            continue
        kind, debug = found
        report.binaries.append((info.filename, kind, info.file_size))
        if debug:
            listed = ", ".join(sorted(set(debug))[:6])
            report.problems.append(
                f"{info.filename} ({kind}, {info.file_size / MIB:.1f} MiB) embeds debug info: {listed}"
            )
    return report


def check_wheels(wheels: list[tuple[str, int, zipfile.ZipFile]]) -> list[str]:
    problems = []
    total = 0
    for name, size, archive in wheels:
        report = check_wheel(name, size, archive)
        total += size
        print(f"{name}: {size / MIB:.1f} MiB")
        for filename, kind, file_size in report.binaries:
            print(f"    {kind:6} {file_size / MIB:6.1f} MiB  {filename}")
        problems.extend(report.problems)
    print(f"release total: {total / MIB:.1f} MiB (budget {MAX_RELEASE_BYTES / MIB:.0f} MiB)")
    if total > MAX_RELEASE_BYTES:
        problems.append(f"release totals {total / MIB:.1f} MiB; budget is {MAX_RELEASE_BYTES / MIB:.0f} MiB")
    return problems


def _download(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=120) as response:
        return response.read()


def pypi_wheels(version: str) -> list[tuple[str, int, zipfile.ZipFile]]:
    import io

    meta = json.loads(_download(f"https://pypi.org/pypi/zccache/{version}/json"))
    wheels = []
    for entry in meta["urls"]:
        if entry["filename"].endswith(".whl"):
            wheels.append((entry["filename"], entry["size"], zipfile.ZipFile(io.BytesIO(_download(entry["url"])))))
    return wheels


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("wheels", nargs="*", type=Path, help="wheel files to check")
    parser.add_argument("--pypi", metavar="VERSION", help="check a release already on PyPI")
    args = parser.parse_args(argv)
    if args.pypi:
        wheels = pypi_wheels(args.pypi)
    else:
        if not args.wheels:
            parser.error("give wheel paths or --pypi VERSION")
        wheels = [(p.name, p.stat().st_size, zipfile.ZipFile(p)) for p in args.wheels]
    problems = check_wheels(wheels)
    for problem in problems:
        print(f"::error::{problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
