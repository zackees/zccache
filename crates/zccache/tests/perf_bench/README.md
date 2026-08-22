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

The bare / sccache / zccache columns are **not** three ways of running the same
command. Anyone reading a ratio, and especially anyone treating a failing floor
as a regression, needs the differences:

| side | invocation |
|---|---|
| bare | `Command::new(compiler)` — a subprocess |
| sccache | `Command::new(sccache)` — a subprocess, one per source file |
| zccache | in-process `ClientConn`, one `Request::Compile` |

Two consequences follow, both measured rather than assumed (issue #1437).

### Multi-file bare batches; zccache cannot

`baseline_multi` passes all sources to **one** compiler process
(`cpp_project.rs`), so the compiler's startup cost is amortized across the
whole set. zccache caches per translation unit, so the daemon invokes the
compiler **once per source** — the profile emits one `zccache_cc_miss_profile`
row per file.

For a driver with meaningful startup this is a fixed handicap that scales with
source count. Measured in the pinned image on 50 sources, with zccache removed
from the experiment entirely:

```
batched  (1 em++ invocation , 50 TUs)   40966 ms
separate (50 em++ invocations, 50 TUs)  44267 ms
penalty                                  3301 ms   (~67 ms per extra invocation)
```

em++ is an Emscripten Python driver, so its startup dominates; native clang's
is roughly an order of magnitude smaller, which is why the C and C++
multi-file rows pass on runs where the emscripten one does not.

**A cold multi-file zccache run cannot beat a single batched invocation of a
slow-starting driver on any hardware.** A `Multi-file, Cold vs Bare` floor for
such a driver is measuring batching strategy, not cache performance. Treat a
failure there as a fixture question before hunting for a regression.

### zccache is measured without per-invocation cost

zccache goes through an in-process client; sccache and bare are subprocesses.
Real users reach zccache through the `zccache <compiler> ...` wrapper, which
pays process start, argv parse, tool resolution, endpoint resolve and IPC on
every compile — none of which these numbers include. `zccache_wrapper_profile`
(issue #1460) measures that path, but the benchmark does not exercise it.

So a passing floor here does not by itself establish that a user sees the same
win, and the vs-sccache comparison is the fairer of the two for per-file work
because both sides spawn per file.
