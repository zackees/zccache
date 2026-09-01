# Compiler output tests

This directory holds focused tests split from the parent compiler-output
module so the production implementation stays within the repository
source-file size limit.

`tests.rs` covers target-specific sidecars included in cached compiler output
sets, including packed Linux DWARF, MSVC program databases, the Dylint
lint-library toolchain-qualified sidecar (Linux/macOS/Windows), and the MSVC
import-library pair (`<dll>.lib` + `.exp`) beside a linked Windows DLL. The
import-library pair is opportunistic-only (collected when present, never a
required staged output) — see the doc comment on
`msvc_dll_implib_sidecar_output_paths` in `rustc.rs` for why.
