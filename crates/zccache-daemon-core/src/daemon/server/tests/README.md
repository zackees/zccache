# server/tests

Unit tests for `server/` submodules, originally a single 2.3K-LOC `tests.rs`.
Split per domain so each file stays well under 1,000 LOC; one module per
sibling `server/` subject (pack/persist, cache trim, fingerprint, link cache,
PCH resolution, write-cached-output, post-link hook, server IPC end-to-end).
`fs_matrix.rs` runs the same materialization contract against every available
filesystem fixture and always prints executed/skipped rows with reasons; its
`ZCCACHE_MODE` column runs all four modes on every row (#1683).
`write_cached_mode.rs` runs the cache-hit executor under each `ZCCACHE_MODE`
(#1683): COPY/REFLINK share one independent-delivery contract, LINK shares the
cache inode only for eligible outputs, and switching modes migrates outputs.
`store_mode.rs` covers the store direction: COPY/REFLINK never hardlink the
compiler output into the cache.
`fingerprint_encoding.rs` checks the request encoder against literal legacy bytes.

`mod.rs` declares the per-domain submodules and owns the crate-wide canonical
test guard for process-global cache-dir mutations (`CacheDirEnvGuard`).
`staged_env.rs` lets C/C++ staged-lane tests opt into
`ZCCACHE_STAGED_ARTIFACTS=c-cpp` under that same lock, settle deferred
publication, and detect native change markers (multi-source publication
fails closed without them, #1193).
Per-domain helpers (fixture builders,
`start_daemon`, jobserver-env constructors, `write_fake_linker`, etc.) live
next to the tests that use them — no `common.rs` indirection.
