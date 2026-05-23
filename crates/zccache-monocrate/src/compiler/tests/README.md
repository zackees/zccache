# zccache-compiler tests

Per-topic test modules for `zccache-compiler`'s parsing surface.

The tests live next to the implementation (via `#[cfg(test)] mod tests;` in
`lib.rs`) rather than under `tests/` so they can exercise `pub(crate)`
internals like `SourceMode` and helper functions without going through the
public API.

| File | What it covers |
|---|---|
| `mod.rs` | Re-exports test submodules + shared `args` helper |
| `cpp_parse.rs` | Clang/GCC parsing: -c, multi-file, -x header mode, header units, sticky-mode regressions |
| `cpp_output.rs` | Default output paths, PCH naming, concatenated -o, unknown-flag preservation, BUG_LINKER repro |
| `detect.rs` | `detect_family` + `supports_depfile` for clang/gcc/msvc/emcc |
| `rustc.rs` | Rustc invocation parsing: crate types, --emit, --out-dir, proc-macro/bin output naming |
| `clippy_driver.rs` | `clippy-driver` detection + caching (re-uses rustc parser) |
| `modules.rs` | C++20 modules: .cppm/.ixx, -x c++-module, header units, --precompile, GCC -fmodules-ts |
| `clang_cl.rs` | clang-cl / cl.exe dispatch into the MSVC parser (issue #261), MSVC-style flags |

Test groups under 600 LOC each — see `crates/zccache-compiler/src/lib.rs`
for the module wiring.
