# perf_bench/ — split modules of `tests/perf_bench_test.rs`

This directory is the implementation backing of `tests/perf_bench_test.rs`,
which is a thin shim that `#[path]`-includes `mod.rs` here. All `#[tokio::test]
#[ignore]` perf benchmark functions live in submodules of this directory but
are discovered by cargo under the same `perf_bench_test` test binary, so the
canonical invocation pattern still works:

```
soldr cargo test -p zccache-daemon --test perf_bench_test -- \
    perf_c_zccache_vs_bare --nocapture --ignored
```

See [PERF.md](../../../../PERF.md) → "Preventing regressions — add a perf unit
test" for the rules on adding new perf benchmarks here.

## Module layout

| Module | Contains |
|---|---|
| `mod.rs` | Module declarations (no logic). |
| `common.rs` | `start_daemon`, sccache/em++/archiver finders, timing helpers (`median`, `fmt_dur`, `print_trials*`, `fmt_ratio`), shared tool runners, session helpers, constants, `ClientConn` type alias. |
| `c_project.rs` | C source generation + bare/sccache/zccache C compile helpers. |
| `cpp_project.rs` | C++ source generation, warmup, single/multi compile helpers (bare/sccache/zccache) including the `with_env` variant. |
| `response_file.rs` | `flags.rsp` / `defines.rsp` / `sources_multi.rsp` generation and the `_rsp` variants of single/multi compile helpers. |
| `rust_project.rs` | Rust source generation, `rustc_args_for` / `rustc_check_args_for`, batch runners for rustc / sccache rustc / zccache rustc (+ env variant). |
| `link.rs` | `LinkBenchResult`, `measure_ephemeral_link_scenario`, `print_link_benchmark_table`, archive + driver + rust-link input preparation. |
| `sibling_remap.rs` | `make_git_workspace`, `path_remap_auto_env`, `CppSiblingRemapResult`, `measure_cpp_sibling_remap_mode`. |
| `tests_c.rs` | `perf_c_zccache_vs_bare`, `generated_c_project_compiles_under_std_c11`. |
| `tests_cpp.rs` | `perf_warm_cache_zccache_vs_sccache`. |
| `tests_response_file.rs` | `perf_response_file`. |
| `tests_rust.rs` | `perf_rustc_zccache_vs_sccache`. |
| `tests_sibling_remap.rs` | `perf_cpp_sibling_remap_warm`, `perf_rustc_sibling_remap_warm`. |
| `tests_emcc.rs` | `perf_emcc_warm_cache_zccache_vs_sccache`, `perf_emcc_sibling_remap_warm`. |
| `tests_link.rs` | `perf_c_archive_link`, `perf_cpp_driver_link`, `perf_emcc_link`, `perf_rust_workspace_link`. |
