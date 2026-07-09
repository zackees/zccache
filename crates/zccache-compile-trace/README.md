# zccache-compile-trace

Diagnostic JSONL trace support for embedded compile phases.

This unpublished internal crate owns the `ZCCACHE_INNER_TRACE` writer and
phase guard. The `zccache` facade re-exports this crate as
`zccache::compile_trace` so existing compile-trace paths keep working.

## Enabling

Set `ZCCACHE_INNER_TRACE=<path>` before starting the process that hosts the
embedded compile (soldr / fbuild). Each recorded sub-phase appends one JSONL
line; the shape is byte-for-byte identical to soldr's daemon trace, so
soldr's `bench/parse_compile_trace.py` reads either file unchanged:

```jsonl
{"ts_ns":<u128>,"phase":"<name>","micros":<u64>,"compile_id":"<str>"}
```

Off by default: one `OnceLock` load resolves to `None` and every `record`
call returns immediately when the env var is unset.

## Sub-phase markers (issue #940)

`record`/`Phase` are the low-level API. The `zccache` daemon wires the
per-compile id and the sub-phase seams through
`crate::daemon::server::inner_trace` (a `tokio::task_local` scoped around
`EmbeddedDaemon::compile`). The markers emitted for an embedded compile:

| phase | path | emitted from |
|---|---|---|
| `embedded_daemon_compile` | always | outer span in `embedded.rs` |
| `input_hash` | miss | `pipeline/store_outcome.rs` (`hash_source_ns + hash_headers_ns`) |
| `cache_lookup` | miss | `pipeline/store_outcome.rs` (`depgraph_check_ns`) |
| `cache_load` | hit | `handle_compile/cached_hit.rs` (artifact index lookup + payload read) |
| `rustc_spawn` | miss | `pipeline/store_outcome.rs` (`compiler_prep_ns`) |
| `rustc_wait` | miss | `pipeline/store_outcome.rs` (`compiler_process_ns`) |
| `output_read` | miss | `pipeline/store_outcome.rs` (`collect_outputs_ns`) |
| `cache_store` | miss | `pipeline/store_outcome.rs` (`artifact_store_ns`) |

`rustc_spawn` and `rustc_wait` reuse the fused prep/process timings — the
subprocess spawn+wait+pipe-drain is one measured region (`compiler_process_ns`)
in `daemon/process.rs`. Splitting it into distinct spawn / wait / read markers
would need a process-layer refactor the diagnostic does not warrant; the two
markers approximate the split from the already-measured seams.

Only embedded compiles emit sub-phase markers: the IPC wrapper path does not
open an `inner_trace::scope`, so the shared pipeline seams stay silent there.

## Tests

- `crate::daemon::server::inner_trace` unit tests cover the task-local
  gating (no-op outside a scope, id visible inside).
- `crates/zccache/tests/inner_trace_file_test.rs` covers the JSONL writer
  wire shape end-to-end in its own test binary (fresh `OnceLock`).
