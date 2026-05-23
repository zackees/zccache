## zccache

Transitional crate absorbing the historical 21-crate workspace
([issue #365](https://github.com/zackees/zccache/issues/365)). Renamed to
`zccache` once all waves land.

Each subdirectory under `src/` is one of the former crates (e.g. `src/core/`
is the old `zccache-core`). Cross-module references use `crate::<module>::*`
in place of the former `zccache_<module>::*`.
