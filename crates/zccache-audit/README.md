# zccache-audit

Durable audit schema types for embedded zccache integrations.

This crate owns the JSON-compatible audit data model that hosts can serialize
to JSONL or adapt into their own audit sinks. The `zccache` facade is expected
to re-export this crate as `zccache::audit` so existing audit paths keep
working.

It also owns the cache-root log-audit catalog used by perf and integration
teardown. `zccache-ci dump-registry` emits its checked-in registry snapshot.
