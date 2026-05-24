## fingerprint/

Per-file mtime → blake3 fingerprint cache (`TwoLayerCache`) and aggregate
file-set hash (`HashCache`). Used by the `zccache-fp` bin and the
`zccache.fingerprint` Python wrapper. See `mod.rs` for the public API.
