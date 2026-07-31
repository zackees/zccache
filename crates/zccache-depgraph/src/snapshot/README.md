## snapshot/

On-disk persistence of the dependency graph. `mod.rs` exposes the public
API (`save_to_file`, `load_from_file`, `classify_load`); `persistence.rs`
handles the file I/O; `quarantine.rs` moves a snapshot this build cannot
read aside (instead of letting the next shutdown overwrite it) and loads
back a sidecar written by this build's own `DEPGRAPH_VERSION`; `tests/`
(cfg(test)-only) splits per concern — roundtrip, persistence, behavioral.
