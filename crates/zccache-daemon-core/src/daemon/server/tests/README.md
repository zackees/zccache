# server/tests

Unit tests for `server/` submodules, originally a single 2.3K-LOC `tests.rs`.
Split per domain so each file stays well under 1,000 LOC; one module per
sibling `server/` subject (pack/persist, cache trim, fingerprint, link cache,
PCH resolution, write-cached-output, post-link hook, server IPC end-to-end).
`fs_matrix.rs` runs the same materialization contract against every available
filesystem fixture and always prints executed/skipped rows with reasons.

`mod.rs` declares the per-domain submodules and owns only the cross-module
helpers (`CacheDirEnvGuard`). Per-domain helpers (fixture builders,
`start_daemon`, jobserver-env constructors, `write_fake_linker`, etc.) live
next to the tests that use them — no `common.rs` indirection.
