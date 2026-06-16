# parse_linker

Linker detection and argument parsing for `ld`, `lld`, MSVC `link.exe`, and
compiler drivers (`gcc`, `clang`) used as linkers. Determines cacheability for
linking (shared libraries, DLLs, and executables).

## Layout

- `mod.rs` — public re-exports and the top-level `parse_linker_invocation`
  dispatch entry point.
- `types.rs` — public types (`LinkerFamily`, `ParsedLinkerInvocation`,
  `CacheableLink`).
- `detect.rs` — tool-name detection helpers (`detect_family`,
  `is_compiler_driver`, `is_linker`, `is_link_invocation`).
- `gnu_ld.rs` — GNU `ld` / LLVM `lld` argument parser.
- `msvc_link.rs` — MSVC `link.exe` argument parser.
- `compiler_driver.rs` — compiler-driver-as-linker parser
  (`gcc -shared -o ...`, `clang -o a.out ...`).
- `tests/` — unit tests, split by domain.
