# zccache-compile-trace

Diagnostic JSONL trace support for embedded compile phases.

This unpublished internal crate owns the `ZCCACHE_INNER_TRACE` writer and
phase guard. The `zccache` facade is expected to re-export this crate as
`zccache::compile_trace` so existing compile-trace paths keep working.
