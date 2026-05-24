## snapshot/

On-disk persistence of the dependency graph. `mod.rs` exposes the public
API (`save_to_file`, `load_from_file`, `classify_load`); `persistence.rs`
handles the file I/O; `tests/` (cfg(test)-only) splits per concern —
roundtrip, persistence, behavioral.
