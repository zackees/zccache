import io
import struct
import zipfile

from ci import wheel_check


def elf64(section_names: list[str]) -> bytes:
    """Minimal little-endian ELF64 with a section table and .shstrtab."""
    names = ["", *section_names, ".shstrtab"]
    strtab = b""
    offsets = []
    for name in names:
        offsets.append(len(strtab))
        strtab += name.encode() + b"\0"
    header_size, entry = 64, 64
    strtab_offset = header_size
    shoff = strtab_offset + len(strtab)
    header = bytearray(header_size)
    header[:4] = b"\x7fELF"
    header[4], header[5], header[6] = 2, 1, 1
    struct.pack_into("<Q", header, 0x28, shoff)
    struct.pack_into("<HHH", header, 0x3A, entry, len(names), len(names) - 1)
    table = bytearray()
    for index, offset in enumerate(offsets):
        section = bytearray(entry)
        struct.pack_into("<I", section, 0, offset)
        if index == len(names) - 1:
            struct.pack_into("<Q", section, 0x18, strtab_offset)
        table += section
    return bytes(header) + strtab + bytes(table)


def macho64(segment_names: list[str]) -> bytes:
    commands = b""
    for name in segment_names:
        command = bytearray(72)
        struct.pack_into("<II", command, 0, wheel_check.LC_SEGMENT_64, 72)
        command[8 : 8 + len(name)] = name.encode()
        commands += command
    header = bytearray(32)
    header[:4] = b"\xcf\xfa\xed\xfe"
    struct.pack_into("<II", header, 16, len(segment_names), len(commands))
    return bytes(header) + commands


def fat_macho(*slices: bytes) -> bytes:
    header = bytearray(8 + 20 * len(slices))
    header[:4] = wheel_check.MACHO_FAT
    struct.pack_into(">I", header, 4, len(slices))
    body = b""
    offset = len(header)
    for index, thin in enumerate(slices):
        struct.pack_into(">IIIII", header, 8 + index * 20, 0, 0, offset + len(body), len(thin), 0)
        body += thin
    return bytes(header) + body


def pe(section_names: list[str], long_names: list[str] | None = None) -> bytes:
    long_names = long_names or []
    pe_offset = 0x40
    optional_size = 0
    table_offset = pe_offset + 4 + 20 + optional_size
    # COFF string table: 4-byte total size, then NUL-terminated names.
    strings = bytearray(4)
    raw_names = []
    for name in section_names:
        raw_names.append(name.encode())
    for name in long_names:
        raw_names.append(f"/{len(strings)}".encode())
        strings += name.encode() + b"\0"
    symtab = table_offset + 40 * len(raw_names)
    data = bytearray(symtab)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, pe_offset)
    data[pe_offset : pe_offset + 4] = b"PE\0\0"
    coff = pe_offset + 4
    struct.pack_into("<H", data, coff + 2, len(raw_names))
    struct.pack_into("<II", data, coff + 8, symtab if long_names else 0, 0)
    struct.pack_into("<H", data, coff + 16, optional_size)
    for index, raw in enumerate(raw_names):
        data[table_offset + index * 40 : table_offset + index * 40 + len(raw)] = raw
    struct.pack_into("<I", strings, 0, len(strings))
    return bytes(data) + (bytes(strings) if long_names else b"")


def wheel(members: dict[str, bytes]) -> tuple[str, int, zipfile.ZipFile]:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, content in members.items():
            archive.writestr(name, content)
    size = buffer.tell()
    buffer.seek(0)
    return "zccache-0.0.0-py3-none-any.whl", size, zipfile.ZipFile(buffer)


def test_elf_with_dwarf_is_rejected() -> None:
    kind, debug = wheel_check.debug_sections(elf64([".text", ".debug_info", ".debug_line"]))
    assert kind == "ELF"
    assert debug == [".debug_info", ".debug_line"]


def test_stripped_elf_passes_and_keeps_gdb_scripts_exemption() -> None:
    assert wheel_check.debug_sections(elf64([".text", ".symtab", ".debug_gdb_scripts"])) == ("ELF", [])


def test_compressed_elf_debug_is_rejected() -> None:
    assert wheel_check.debug_sections(elf64([".zdebug_info"]))[1] == [".zdebug_info"]


def test_macho_dwarf_segment_is_rejected_thin_and_fat() -> None:
    assert wheel_check.debug_sections(macho64(["__TEXT", "__DWARF"])) == ("Mach-O", ["__DWARF"])
    assert wheel_check.debug_sections(macho64(["__TEXT", "__LINKEDIT"])) == ("Mach-O", [])
    fat = fat_macho(macho64(["__TEXT"]), macho64(["__TEXT", "__DWARF"]))
    assert wheel_check.debug_sections(fat) == ("Mach-O", ["__DWARF"])


def test_pe_debug_sections_including_long_names() -> None:
    assert wheel_check.debug_sections(pe([".text", ".rdata"])) == ("PE", [])
    assert wheel_check.debug_sections(pe([".text"], long_names=[".debug_info"])) == ("PE", [".debug_info"])


def test_non_binaries_are_ignored() -> None:
    assert wheel_check.debug_sections(b"#!/usr/bin/env python\n") is None


def test_wheel_with_unstripped_binary_fails() -> None:
    problems = wheel_check.check_wheels(
        [wheel({"zccache/_native.so": elf64([".debug_info"]), "zccache/__init__.py": b"x = 1\n"})]
    )
    assert len(problems) == 1
    assert "zccache/_native.so" in problems[0] and ".debug_info" in problems[0]


def test_clean_wheel_passes() -> None:
    assert wheel_check.check_wheels([wheel({"zccache/_native.so": elf64([".text"])})]) == []


def test_wheel_and_release_budgets(monkeypatch) -> None:
    candidate = wheel({"zccache/_native.so": elf64([".text"])})
    monkeypatch.setattr(wheel_check, "MAX_WHEEL_BYTES", candidate[1] - 1)
    monkeypatch.setattr(wheel_check, "MAX_RELEASE_BYTES", 2 * candidate[1] - 1)
    problems = wheel_check.check_wheels([candidate, wheel({"zccache/_native.so": elf64([".text"])})])
    assert sum("budget is" in problem for problem in problems) == 3


def test_release_workflow_runs_the_check_before_upload() -> None:
    from pathlib import Path

    workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/release-auto.yml").read_text()
    build = workflow.index("python -m ci.release_workflow build-wheels")
    check = workflow.index("ci/wheel_check.py dist/wheels/*.whl")
    upload = workflow.index("name: pypi-wheels")
    assert build < check < upload
