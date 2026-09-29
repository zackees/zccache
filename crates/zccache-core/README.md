# zccache-core

Internal crate for zccache's shared core types, path utilities, cache-root
configuration, lifecycle logging, crash reporting, Windows Defender helpers, and
the `mtime` module: the one owner of materialized-output mtime policy (#1771).

The root `zccache` facade is expected to re-export this crate as
`zccache::core` so existing public module paths keep working.
