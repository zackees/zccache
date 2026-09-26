# cli/tests

Unit tests for `cli/` submodules, originally a single 1.4K-LOC `tests.rs`.
Split per domain so each file stays well under 1,000 LOC; one module per
sibling `cli/` subject (analyze, cache_ops, daemon, session/args, etc.).
`mod.rs` is just `pub mod` declarations — no shared helpers live here; each
file owns the tempfile/fixture helpers its tests use. `warm_delivery.rs` is
loaded by `commands/warm_delivery.rs` and covers `zccache warm` under each
`ZCCACHE_MODE`.
