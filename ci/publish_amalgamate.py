"""Build the crates.io `zccache` package as a self-contained crate."""

from __future__ import annotations

import re
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class AmalgamatedModule:
    crate: str
    module: str
    declaration: str
    drop_python_bindings: bool = False


INTERNAL_MODULES: tuple[AmalgamatedModule, ...] = (
    AmalgamatedModule("zccache-artifact", "artifact", "pub mod artifact;"),
    AmalgamatedModule("zccache-audit", "audit", "pub mod audit;"),
    AmalgamatedModule(
        "zccache-compile-trace",
        "compile_trace",
        "pub mod compile_trace;",
    ),
    AmalgamatedModule("zccache-compiler", "compiler", "pub mod compiler;"),
    AmalgamatedModule("zccache-core", "core", "pub mod core;"),
    AmalgamatedModule("zccache-depgraph", "depgraph", "pub mod depgraph;"),
    AmalgamatedModule(
        "zccache-download",
        "download",
        '#[cfg(feature = "download")]\npub mod download;',
    ),
    AmalgamatedModule(
        "zccache-download-protocol",
        "download_protocol",
        '#[cfg(feature = "download-protocol")]\npub mod download_protocol;',
    ),
    AmalgamatedModule(
        "zccache-fingerprint",
        "fingerprint",
        "pub mod fingerprint;",
        drop_python_bindings=True,
    ),
    AmalgamatedModule("zccache-fscache", "fscache", "pub mod fscache;"),
    AmalgamatedModule(
        "zccache-gha",
        "gha",
        '#[cfg(feature = "gha")]\npub mod gha;',
    ),
    AmalgamatedModule("zccache-hash", "hash", "pub mod hash;"),
    AmalgamatedModule("zccache-ipc", "ipc", "pub mod ipc;"),
    AmalgamatedModule("zccache-protocol", "protocol", "pub mod protocol;"),
    AmalgamatedModule(
        "zccache-symbols",
        "symbols",
        '#[cfg(feature = "symbols")]\npub mod symbols;',
    ),
    AmalgamatedModule(
        "zccache-watcher",
        "watcher",
        "pub mod watcher;",
        drop_python_bindings=True,
    ),
)

INTERNAL_FEATURE_REMOVALS: dict[str, set[str]] = {
    "cli": {"zccache-artifact/cli"},
    "download": {"dep:zccache-download"},
    "download-protocol": {"dep:zccache-download-protocol"},
    "gha": {"dep:zccache-gha", "zccache-artifact/gha"},
    "symbols": {"dep:zccache-symbols"},
}
INTERNAL_FEATURE_ADDITIONS: dict[str, tuple[str, ...]] = {
    "gha": ("dep:reqwest", "dep:sha2"),
}


def prepare_zccache_crate_for_publish(
    root: Path = ROOT,
    modules: Sequence[AmalgamatedModule] = INTERNAL_MODULES,
) -> None:
    """Rewrite `crates/zccache` into the single crate uploaded to crates.io."""

    root = root.resolve()
    zccache_dir = root / "crates" / "zccache"
    zccache_src = zccache_dir / "src"
    module_map = {module.crate: module.module for module in modules}

    for module in modules:
        source_src = root / "crates" / module.crate / "src"
        target_src = zccache_src / module.module
        if not source_src.is_dir():
            raise FileNotFoundError(f"missing internal crate source: {source_src}")
        if target_src.exists():
            shutil.rmtree(target_src)
        shutil.copytree(
            source_src,
            target_src,
            ignore=shutil.ignore_patterns("__pycache__", "*.pyc"),
        )

        copied_lib = target_src / "lib.rs"
        if copied_lib.exists():
            copied_lib.rename(target_src / "mod.rs")

        if module.drop_python_bindings:
            python_file = target_src / "python.rs"
            if python_file.exists():
                python_file.unlink()

        rewrite_copied_module_sources(target_src, module, module_map)

    copy_protocol_schema(root)
    write_publish_lib_rs(zccache_src, modules)
    write_publish_build_rs(zccache_dir)
    rewrite_zccache_manifest(zccache_dir / "Cargo.toml", module_map)
    assert_publish_crate_is_self_contained(zccache_dir, module_map)


def copy_protocol_schema(root: Path) -> None:
    source_proto = root / "crates" / "zccache-protocol" / "proto"
    target_proto = root / "crates" / "zccache" / "proto"
    if target_proto.exists():
        shutil.rmtree(target_proto)
    shutil.copytree(source_proto, target_proto)


def write_publish_lib_rs(
    zccache_src: Path,
    modules: Sequence[AmalgamatedModule],
) -> None:
    internal_declarations = "\n".join(module.declaration for module in modules)
    text = f"""//! Public zccache crate.
//!
//! Release packaging rewrites this facade into a self-contained crate so
//! crates.io consumers only depend on `zccache`, while git and path consumers
//! still get parallel compilation from the internal workspace crates.

{internal_declarations}
/// Issue zccache#926 - durable audit JSONL writer for the embedded service.
pub mod audit_writer;
#[cfg(feature = "ci")]
pub mod ci;
#[cfg(feature = "cli")]
pub mod cli;
pub mod daemon;
#[cfg(feature = "download-client")]
pub mod download_client;
#[cfg(feature = "download-daemon")]
pub mod download_daemon;
pub mod embedded;

#[cfg(feature = "test-support")]
pub mod test_support;
"""
    (zccache_src / "lib.rs").write_text(text, encoding="utf-8")


def write_publish_build_rs(zccache_dir: Path) -> None:
    text = """// Build scripts are allowed to panic on setup failure: cargo surfaces
// the panic message and fails the build cleanly.
#![allow(clippy::expect_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=ZCCACHE_BUILD_TARGET={target}");

    println!("cargo:rerun-if-changed=proto/zccache_v1.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc is available for zccache wire protobufs");
    std::env::set_var("PROTOC", protoc);

    prost_build::Config::new()
        .compile_protos(&["proto/zccache_v1.proto"], &["proto"])
        .expect("zccache wire protobufs compile");
}
"""
    (zccache_dir / "build.rs").write_text(text, encoding="utf-8")


def rewrite_copied_module_sources(
    target_src: Path,
    module: AmalgamatedModule,
    module_map: dict[str, str],
) -> None:
    for source in sorted(target_src.rglob("*.rs")):
        text = source.read_text(encoding="utf-8")
        text = rewrite_rust_source_for_amalgamation(
            text,
            module=module.module,
            module_map=module_map,
        )
        if module.drop_python_bindings:
            text = drop_python_extension_bindings(text)
        source.write_text(text, encoding="utf-8")


def rewrite_rust_source_for_amalgamation(
    text: str,
    *,
    module: str,
    module_map: dict[str, str],
) -> str:
    text = re.sub(r"\bcrate::", f"crate::{module}::", text)
    for crate_name, module_name in sorted(
        module_map.items(),
        key=lambda item: len(rust_crate_ident(item[0])),
        reverse=True,
    ):
        ident = rust_crate_ident(crate_name)
        text = re.sub(rf"\b{re.escape(ident)}::", f"crate::{module_name}::", text)
        text = re.sub(rf"\b{re.escape(ident)}\b", f"crate::{module_name}", text)
    return text


def drop_python_extension_bindings(text: str) -> str:
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    skip_next_python_item = False
    for line in lines:
        if line.strip() == '#[cfg(feature = "python")]':
            skip_next_python_item = True
            continue
        if skip_next_python_item:
            stripped = line.strip()
            if stripped == "mod python;" or stripped.startswith("pub use python::"):
                skip_next_python_item = False
                continue
            out.append('#[cfg(feature = "python")]\n')
            skip_next_python_item = False
        out.append(line)
    if skip_next_python_item:
        out.append('#[cfg(feature = "python")]\n')
    return "".join(out)


def rust_crate_ident(crate_name: str) -> str:
    return crate_name.replace("-", "_")


def rewrite_zccache_manifest(manifest_path: Path, module_map: dict[str, str]) -> None:
    text = manifest_path.read_text(encoding="utf-8")
    text = strip_internal_dependency_lines(text, module_map.keys())
    for feature, removals in INTERNAL_FEATURE_REMOVALS.items():
        additions = INTERNAL_FEATURE_ADDITIONS.get(feature, ())
        text = rewrite_feature_items(text, feature, remove=removals, add=additions)
    text = ensure_build_dependency(text, "prost-build = { workspace = true }")
    text = ensure_build_dependency(text, "protoc-bin-vendored = { workspace = true }")
    manifest_path.write_text(text, encoding="utf-8")


def strip_internal_dependency_lines(text: str, internal_crates: Iterable[str]) -> str:
    internal = set(internal_crates)
    output: list[str] = []
    section = ""
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        match = re.match(r"^\[([^\]]+)\]$", stripped)
        if match:
            section = match.group(1)

        if section == "dependencies" and stripped == "# internal facade crates":
            continue
        if section == "dependencies" and dependency_line_name(line) in internal:
            continue
        if section == "dev-dependencies" and dependency_line_name(line) == "zccache":
            continue
        output.append(line)
    return "".join(output)


def dependency_line_name(line: str) -> str | None:
    match = re.match(r"^([A-Za-z0-9_-]+)\s*=", line)
    if not match:
        return None
    return match.group(1)


def rewrite_feature_items(
    text: str,
    feature: str,
    *,
    remove: set[str],
    add: Sequence[str] = (),
) -> str:
    pattern = re.compile(rf"(?ms)^{re.escape(feature)}\s*=\s*\[(.*?)\]")
    match = pattern.search(text)
    if not match:
        return text

    existing = re.findall(r'"([^"]+)"', match.group(1))
    items: list[str] = []
    for item in existing:
        if item in remove or item in items:
            continue
        items.append(item)
    for item in add:
        if item not in items:
            items.append(item)

    return text[: match.start()] + format_feature(feature, items) + text[match.end() :]


def format_feature(feature: str, items: Sequence[str]) -> str:
    if not items:
        return f"{feature} = []"
    if len(items) <= 3:
        rendered = ", ".join(f'"{item}"' for item in items)
        return f"{feature} = [{rendered}]"
    lines = [f"{feature} = [\n"]
    lines.extend(f'    "{item}",\n' for item in items)
    lines.append("]")
    return "".join(lines)


def ensure_build_dependency(text: str, dependency_line: str) -> str:
    if dependency_line in text:
        return text
    section_header = "[build-dependencies]"
    if section_header in text:
        return insert_into_section(text, section_header, dependency_line)

    insert_at = len(text)
    for marker in ("\n[target.", "\n[dev-dependencies]", "\n[[bench]]", "\n[lints]"):
        index = text.find(marker)
        if index != -1:
            insert_at = index
            break
    block = f"\n{section_header}\n{dependency_line}\n"
    return text[:insert_at].rstrip() + "\n" + block + text[insert_at:]


def insert_into_section(text: str, section_header: str, dependency_line: str) -> str:
    start = text.index(section_header) + len(section_header)
    next_section = re.search(r"(?m)^\[", text[start:])
    end = len(text) if next_section is None else start + next_section.start()
    section = text[start:end].rstrip()
    updated = f"{section}\n{dependency_line}\n"
    return text[:start] + updated + text[end:]


def assert_publish_crate_is_self_contained(
    zccache_dir: Path,
    module_map: dict[str, str],
) -> None:
    manifest = (zccache_dir / "Cargo.toml").read_text(encoding="utf-8")
    source_text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted((zccache_dir / "src").rglob("*.rs"))
    )

    manifest_errors = [
        crate
        for crate in sorted(module_map)
        if re.search(rf"^{re.escape(crate)}\s*=", manifest, re.M)
    ]
    source_errors = [
        rust_crate_ident(crate)
        for crate in sorted(module_map)
        if re.search(
            rf"\b(?:use\s+|extern\s+crate\s+)?{re.escape(rust_crate_ident(crate))}::",
            source_text,
        )
        or re.search(
            rf"\bextern\s+crate\s+{re.escape(rust_crate_ident(crate))}\b",
            source_text,
        )
    ]
    if manifest_errors or source_errors:
        details = []
        if manifest_errors:
            details.append("manifest dependencies: " + ", ".join(manifest_errors))
        if source_errors:
            details.append("source references: " + ", ".join(source_errors))
        raise RuntimeError("amalgamated crate is not self-contained: " + "; ".join(details))


def main() -> None:
    prepare_zccache_crate_for_publish()


if __name__ == "__main__":
    main()
