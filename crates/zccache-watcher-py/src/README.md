## src/

PyO3 binding source for `zccache.watcher._native`.

`lib.rs` is the former `crates/zccache-watcher/src/python.rs`, moved verbatim
apart from its imports: it now reaches the engine through the public
`zccache_watcher::` path instead of `crate::`.

Nothing but bindings belongs here. Watcher behaviour lives in
[`zccache-watcher`](../../zccache-watcher), so this crate stays a thin
`cdylib` — see [../README.md](../README.md) for why the cdylib cannot sit on
the engine crate itself (zccache#1497).
