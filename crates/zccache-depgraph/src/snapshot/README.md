## snapshot/

On-disk persistence of the dependency graph. `mod.rs` exposes the public
API (`save_to_file`, `load_from_file`, `classify_load`); `persistence.rs`
handles the file I/O; `quarantine.rs` moves a snapshot this build cannot
read aside (instead of letting the next shutdown overwrite it) and loads
back a sidecar written by this build's own `DEPGRAPH_VERSION`; `tests/`
(cfg(test)-only) splits per concern — roundtrip, persistence, behavioral.

Format (v8, zccache#1661): `ZCDG` magic + `DEPGRAPH_VERSION` + payload
length header, then a bincode 1 payload streamed with `serialize_into` /
`deserialize_from`. rkyv was removed: its 32-bit relative pointers capped a
snapshot at 2 GiB and overflowed with a panic. A v7 (rkyv) file is a version
mismatch and costs one cold start.

Bounding:

- **TTL `GC_TTL` = 7 days.** Each context persists `last_accessed_unix_ms`,
  so ages survive restarts (under soldr the daemon restarts 34-138x/day; the
  old load re-stamped `Instant::now()`, so nothing was ever trimmed). Seven
  days keeps weekly-used projects warm now that the age is real.
- **Budget `SNAPSHOT_BUDGET_BYTES` = 256 MiB.** Over budget, least-recently
  used contexts are evicted (`DepGraph::evict_contexts`) until it fits.
- A failed save never panics: it returns `Err`, deletes the tmp file and
  leaves the previous `depgraph.bin` untouched. `SaveOptions::fail_injection`
  and `inject_save_failures_for_tests` are the test seams.

`tests/bounding_1661.rs` covers all of the above.
