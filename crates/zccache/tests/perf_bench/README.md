# perf_bench/ — split modules of `tests/perf_bench_test.rs`

This directory is the implementation backing of `tests/perf_bench_test.rs`,
which is a thin shim that `#[path]`-includes `mod.rs` here. All `#[tokio::test]
#[ignore]` perf benchmark functions live in submodules of this directory but
are discovered by cargo under the same `perf_bench_test` test binary, so the
canonical invocation pattern still works:

```
soldr cargo test -p zccache --test perf_bench_test -- \
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

## What the three sides actually execute

The cold multi-file rows deliberately use one compiler invocation per
translation unit on every side (#1437). zccache receives one in-process
`Request::Compile` containing all sources, then its multi-source cache path
invokes the compiler separately for each source. The bare and sccache cold
baselines now do the same, so their floors compare like invocation shapes:

| side | single-file | cold multi-file | warm multi-file |
|---|---|---|---|
| bare | one process per source | one process per source | one process, all sources |
| sccache | one wrapper process per source | one wrapper process per source | one wrapper process, all sources |
| zccache | one in-process request per source | one request; daemon invokes the compiler per source | one request; daemon serves per-source hits |

The response-file variant follows the same rule. Its cold baselines invoke
`-c unit.cpp @flags.rsp` once per source; its warm baselines retain the
historical single `@sources_multi.rsp` invocation.

Warm multi-file measurements intentionally retain their historical batched
bare and sccache semantics. Their numbers remain comparable to prior warm
results; they are not used as cold cache-miss parity baselines.

### zccache is measured without per-invocation cost

zccache goes through an in-process client; sccache and bare are subprocesses.
Real users reach zccache through the `zccache <compiler> ...` wrapper, which
pays process start, argv parse, tool resolution, endpoint resolve and IPC on
every compile — none of which these numbers include. `zccache_wrapper_profile`
(issue #1460) measures that path, but the benchmark does not exercise it.

So a passing floor here does not by itself establish that a user sees the same
win.

### sccache does not cache the batched warm multi-file fixture

`sccache_compile_multi` passes all 50 sources to **one** sccache process, and
sccache does not cache multi-source invocations. Measured in the pinned image
after one such call:

```
Compile requests                      1
Compile requests executed             0
Non-cacheable compilations            0
Non-cacheable calls                   1
```

One request, classified non-cacheable, forwarded straight through. This is
why the warm multi-file row deliberately retains its historical batched
baseline semantics rather than presenting it as a cache-vs-cache comparison.

The cold multi-file row is different: bare and sccache both run one compiler
invocation per translation unit, matching zccache's per-source compiler work.
Those cold rows are therefore invocation-shape comparable, and the sccache
side is cacheable. The benchmark purges those per-TU sccache entries before
measuring the historical batched warm row so the two meanings do not leak into
one another.
