## src/

Rust watcher engine source for `zccache-watcher`.

The crate root preserves the former `zccache::watcher` module surface so the
main facade can re-export this crate as `zccache::watcher`. The optional
`python` feature keeps the PyO3 `_native` extension bindings in `python.rs`.
