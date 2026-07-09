# Source

128-byte release marker footer ([`marker`](marker.rs)) plus per-crash sidecar
([`cache`](cache.rs)). Used by the `zccache-stamp` CI helper and by
`zccache symbols install` at runtime.
