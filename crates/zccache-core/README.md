# zccache-core

Internal crate for zccache's shared core types, path utilities, cache-root
configuration, lifecycle logging, crash reporting, and Windows Defender helpers.

The root `zccache` facade is expected to re-export this crate as
`zccache::core` so existing public module paths keep working.
