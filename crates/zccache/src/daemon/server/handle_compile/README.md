# `handle_compile` — compile-request pipeline

Split-out modules from the original `handle_compile.rs`. The compile pipeline is the per-request flow that decides hit vs miss, materializes cached outputs on hit, or runs the compiler on miss.

## File map

| File | Role |
|---|---|
| `pipeline.rs` | `handle_compile_request` — the top-level dispatch (system include discovery, arg parse, context build, hash, depgraph check, hit branches, fallback to compiler exec, store) |
| `hit_branches.rs` | Request-cache / fast-hit / depgraph-hit probes — early returns into `cached_hit` |
| `cached_hit.rs` | Shared materialization of a cached artifact onto the output path (hardlink + mtime preservation), stats bookkeeping |
| `request.rs` | Decoded request shape consumed by `pipeline` |
| `error_cache.rs` | Caching + materialization of cached compiler errors |
| `miss_profile.rs` | Opt-in detailed per-phase profile of a miss (gated by `ZCCACHE_PROFILE_RUST_MISS`) |
| `miss_store.rs` | Post-exec artifact store path |

Tests live in `cached_hit.rs::tests` (mtime preservation, materialization shape) and in `pipeline.rs::tests` (phase wiring).
