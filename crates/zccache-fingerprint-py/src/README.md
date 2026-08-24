## src/

PyO3 binding source for `zccache.fingerprint._native`.

`lib.rs` is the former `crates/zccache-fingerprint/src/python.rs`, moved
verbatim apart from its imports: it now reaches the engine through the public
`zccache_fingerprint::` path instead of `crate::`, and takes a direct
`zccache-hash` dependency for the hashing it used to get transitively.

Nothing but bindings belongs here. The fingerprint engine lives in
[`zccache-fingerprint`](../../zccache-fingerprint); see
[../README.md](../README.md) for why the cdylib is a separate crate
(zccache#1497).
