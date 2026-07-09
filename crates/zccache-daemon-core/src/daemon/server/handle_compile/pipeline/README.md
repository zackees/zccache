# `pipeline` — compile-request orchestrator (split from `pipeline.rs`)

The compile pipeline was extracted from a single 1.2k LOC `pipeline.rs` into a
directory module so each phase stays under the LOC budget. Re-exports preserve
the original `pipeline::handle_compile_request` public path so callers in the
parent `handle_compile.rs` need no changes.

## Layout

| File | Responsibility |
|---|---|
| `mod.rs` | `handle_compile_request` orchestrator; wires the phases together |
| `system_includes.rs` | Per-compiler system include discovery + initial watch |
| `hash_verify.rs` | Hash source + headers in parallel, run depgraph verdict |
| `compile_exec.rs` | Prepare depfile / response file, spawn compiler, parse output |
| `store_outcome.rs` | Successful-compile post path: scan deps, hash all, store artifact, emit miss profiles, schedule background watcher updates |

## Why a phase-per-file split

`handle_compile_request` was previously a single 700+ line function with many
local timing variables threaded through every phase. Splitting along the natural
"prep → hash → exec → store" boundary keeps each module focused on one phase
and lets the orchestrator in `mod.rs` stay readable.
