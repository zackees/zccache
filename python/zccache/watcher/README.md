# `zccache.watcher`

Cross-platform Python watcher API backed by the Rust polling engine, so
callers get identical semantics on Linux, macOS, and Windows rather than the
platform-specific behaviour of a native OS notification API.

`__init__.py` is the whole public surface.
