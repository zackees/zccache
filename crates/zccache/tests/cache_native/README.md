# Native-compiler cache — `cache_native` test target

One linked test executable for 17 contracts (#1526). Covers PCH, DLL, link and response-file caching, the ninja/meson rebuild routes, the clang-cl / MSVC / Arduino classification surface, and generic exec.

Each file here is a **module** of `main.rs`, not its own integration-test
binary. Cargo compiles every top-level `tests/*.rs` file as a separate
executable that statically links the whole reachable graph; 98 of those
produced 1.77 GB of test binaries on one host. Adding a contract here costs
nothing extra — adding a new top-level file costs another full link.

## Adding a test

1. Drop the file in this directory.
2. Add `mod <file_stem>;` to `main.rs` (a missing `mod` line compiles fine and
   silently runs nothing).
3. Keep any `#![cfg(...)]` gate or `#![allow(...)]` block as an inner attribute
   at the top of your file — it applies to your module alone.

Its test ID is `cache_native::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test cache_native -E 'test(/^<file_stem>::/)'
```

## Modules

- `compiler_arduino_ino`
- `compiler_clang_cl_classification`
- `compiler_response_file_expansion_test`
- `compiler_response_file_integration_test`
- `compiler_response_file_parser_test`
- `daemon_burst_link_test`
- `daemon_dll_cache_test`
- `daemon_generic_exec_advanced_test`
- `daemon_generic_exec_test`
- `daemon_link_cache_test`
- `daemon_msvc_cl_cache_test`
- `daemon_ninja_rebuild_direct_test`
- `daemon_ninja_rebuild_meson_test`
- `daemon_pch_cache_basic_test`
- `daemon_pch_cache_invalidation_test`
- `daemon_response_file_cache`
- `link_bundle_integration_test`
