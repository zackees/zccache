# parse_linker tests

Unit tests for the linker parser, split by domain so each file stays small.

- `mod.rs` — wires the per-domain test modules and exposes a shared `args` helper.
- `detection.rs` — `detect_family` / `is_linker` / `is_compiler_driver` tests.
- `gnu_ld.rs` — GNU `ld` / `lld` argument-parsing tests (incl. linker scripts).
- `msvc_link.rs` — MSVC `link.exe` argument-parsing tests.
- `compiler_driver.rs` — `gcc` / `clang` / `emcc` as-linker tests.
- `is_link.rs` — `is_link_invocation` classification tests.
- `implib.rs` — GNU/LLD `--out-implib` secondary-output tests.
