# zccache-fingerprint-py

The PyO3 extension module `zccache.fingerprint._native`. Thin bindings over
[`zccache-fingerprint`](../zccache-fingerprint); all fingerprint logic lives
there.

Split out for the same reason as
[`zccache-watcher-py`](../zccache-watcher-py/README.md), which carries the full
explanation: a `cdylib` crate-type alongside `rlib` cannot be made conditional
in cargo, so every build needing the rlib also linked a dynamic-CRT artifact —
which breaks the Windows release binaries under `+crt-static` on x64
(zccache#1497).

`[lib] name = "zccache_fingerprint"` keeps the output filename unchanged for the
packaging step.
