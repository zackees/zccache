# DepGraph Serialization — Design Spec

> **Superseded by [#1661](https://github.com/zackees/zccache/issues/1661).**
> The rkyv zero-copy format described below was replaced by a streamed
> bincode 1 (serde) snapshot, and rkyv was removed from the workspace. See
> `docs/architecture/runtime.md` "Dep Graph Recovery" for the current format
> (v8), the 7-day TTL and the 256 MiB LRU budget.

## Summary

Persist the in-memory `DepGraph` to disk so the daemon can restore warm
contexts across restarts without cold-scanning every file. Uses **rkyv**
(zero-copy) with an independent `DEPGRAPH_VERSION` for format versioning.

## Decisions (from interview)

| Question | Answer |
|----------|--------|
| When to save | Graceful shutdown **and** periodic (background) |
| When to load | Daemon startup; CLI arg `--no-depgraph-cache` discards file |
| On crash | Accept cold start (no WAL) |
| Which maps | Both `files` + `contexts` (see rationale below) |
| `last_file_hashes` | **Yes** — survive restart so warm contexts serve hits immediately |
| Startup budget | < 200 ms total (drives format choice toward zero-copy) |
| Version mismatch | Log warning in yellow, discard file, cold start |
| `zccache status` | Show `depgraph_version: u32` in output |
| File location | Cache dir (wiped by `zccache clear`) |
| Disk limit | 5 GB max; GC entries older than 1 day |
| Zero-copy query | Not needed — load into DashMap on startup |
| Cross-language | No — Rust only |

## Format Choice: rkyv

**Why rkyv over bincode:**
- 200 ms startup budget is tight for large graphs. Bincode deserializes at
  ~300 MB/s, so a 60 MB graph takes ~200 ms — right at the limit with no
  headroom. rkyv's zero-copy access + bulk `deserialize()` is ~1.5x faster,
  and we can mmap + iterate without allocating the full payload.
- Both lack schema evolution, so versioning is identical effort.
- rkyv 0.8 is mature (90M downloads, actively maintained).

**Why not Cap'n Proto / FlatBuffers:**
- No cross-language need eliminates their main advantage.
- Both require external codegen tooling across 5 CI targets.
- Cap'n Proto: 82 ns access vs rkyv's 1.24 ns.
- FlatBuffers: slowest serializer in benchmarks (1,034 us).

## Versioning: Independent `DEPGRAPH_VERSION`

A `u32` constant in `zccache-depgraph`, **not** coupled to the daemon
version or `PROTOCOL_VERSION`.

**Rationale:**
- Graph schema changes rarely (est. 2-3 times during development).
- Daemon version bumps every release — coupling means every `1.0.x` patch
  nukes the user's warm graph cache for no reason.
- Same proven pattern as `PROTOCOL_VERSION` in `zccache-protocol`.
- Constant lives next to the structs it guards — easy to remember.

**Not content-hash based:**
- Fragile (doc comments, field reordering trigger false invalidation).
- Complex to implement correctly.
- Harder to debug ("why did my cache invalidate?").

## File Format

```
Offset  Size  Field
0       4     Magic: "ZCDG" (0x5A434447)
4       4     DEPGRAPH_VERSION (LE u32)
8       8     Payload length (LE u64)
16      N     rkyv archived payload (DepGraphSnapshot)
```

File path: `<cache_dir>/depgraph.bin`

## Serializable Snapshot

The live `DepGraph` uses `DashMap` and `Instant` which aren't serializable.
We define a serializable mirror:

```rust
/// Version constant. Bump when DepGraphSnapshot layout changes.
pub const DEPGRAPH_VERSION: u32 = 1;

/// Magic bytes: "ZCDG"
pub const DEPGRAPH_MAGIC: [u8; 4] = [0x5A, 0x43, 0x44, 0x47];

#[derive(Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct DepGraphSnapshot {
    pub files: Vec<FileEntrySnapshot>,
    pub contexts: Vec<ContextEntrySnapshot>,
    pub stats: SnapshotStats,
}

#[derive(Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct FileEntrySnapshot {
    pub path: String,              // PathBuf → String (lossy but portable)
    pub includes: Vec<IncludeDirectiveSnapshot>,
}

#[derive(Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IncludeDirectiveSnapshot {
    pub kind: u8,                  // 0=Quoted, 1=AngleBracket, 2=Computed
    pub path: String,
    pub line: u32,
}

#[derive(Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ContextEntrySnapshot {
    pub context_key: [u8; 32],     // ContextKey → raw blake3 bytes
    pub source_file: String,
    pub include_search: IncludeSearchPathsSnapshot,
    pub defines: Vec<String>,
    pub flags: Vec<String>,
    pub force_includes: Vec<String>,
    pub unknown_flags: Vec<String>,
    pub resolved_includes: Vec<String>,
    pub unresolved_includes: Vec<String>,
    pub has_computed_includes: bool,
    pub artifact_key: Option<[u8; 32]>,
    pub last_file_hashes: Vec<(String, [u8; 32])>,
    pub state: u8,                 // 0=Cold, 1=Warm, 2=Stale
}

#[derive(Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IncludeSearchPathsSnapshot {
    pub iquote: Vec<String>,
    pub user: Vec<String>,
    pub system: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapshotStats {
    pub saved_at_epoch_ns: u64,    // SystemTime → epoch nanos
    pub file_count: u64,
    pub context_count: u64,
}
```

### Which maps to serialize — both

| Map | Cost to rebuild | Benefit of persisting |
|-----|----------------|----------------------|
| `contexts` | Expensive: full preprocessor run per TU | Warm hits immediately on restart |
| `files` | Moderate: re-scan every header on disk | Avoids I/O burst at startup; enables `check()` to work without rescanning |

**Verdict:** Serialize both. The `files` map is cheap in size (just include
directives per file, no hashes) and prevents an I/O storm on startup when
the daemon tries to rescan thousands of headers.

### Fields dropped on serialization

- `FileEntry.scanned_at` → `Instant` is opaque. On load, set to `Instant::now()`.
  This means all file entries will be "fresh" on restart, which is correct
  because we immediately verify via the watcher/fscache layer.
- `ContextEntry.last_accessed` → Set to `Instant::now()` on load. Trim timer
  restarts from load time.
- `DepGraph.checks/hits/misses` → Atomic counters reset to 0 on load.
  Lifetime stats could be persisted in `SnapshotStats` if desired later.

### State on load

All loaded contexts start as `Warm` (their serialized state). On the first
`check()`, the watcher/fscache layer will verify freshness. If files changed
while the daemon was down, `check()` returns `HeadersChanged` → `Stale`,
which triggers a rescan. This is correct: we trust `last_file_hashes` to
detect drift without needing to mark everything `Stale` upfront.

## Persistence Lifecycle

### Save — shutdown

```
daemon shutdown signal received
  → take DashMap snapshots (iterate both maps)
  → build DepGraphSnapshot
  → rkyv::to_bytes()
  → write to <cache_dir>/depgraph.bin.tmp
  → atomic rename to depgraph.bin
```

### Save — periodic

```
every N minutes (configurable, default 5 min):
  → spawn background tokio task
  → snapshot + serialize (same as shutdown)
  → write tmp + atomic rename
  → log "depgraph saved: {file_count} files, {context_count} contexts"
```

Periodic save does NOT block the hot path. Snapshotting iterates DashMap
(lock-free reads) and serializes in a background task.

### Load — startup

```
daemon start:
  if --no-depgraph-cache:
    delete depgraph.bin if exists
    start with empty DepGraph
  else:
    read depgraph.bin
    check magic → mismatch: warn + cold start
    check DEPGRAPH_VERSION → mismatch: warn (yellow) + cold start
    rkyv validate + deserialize
    populate DashMap from snapshot
    set Instant::now() for all time fields
    reset atomic counters to 0
    log "depgraph loaded: {file_count} files, {context_count} contexts in {elapsed_ms}ms"
```

### GC — garbage collection

```
on periodic save (or separate GC timer):
  for each context entry:
    if last_accessed > 1 day ago: remove
  for each file entry:
    if no context references it: remove
  if total serialized size > 5 GB:
    evict oldest-accessed contexts until under limit
```

## Protocol Changes

Add to `DaemonStatus`:
```rust
pub dep_graph_version: u32,     // DEPGRAPH_VERSION constant
pub dep_graph_disk_size: u64,   // depgraph.bin file size in bytes
```

This requires a `PROTOCOL_VERSION` bump (per project convention).

## CLI Changes

`zccache status` output gains:
```
Dep graph:     1,234 contexts, 5,678 files (v1, 12.3 MB on disk)
```

`zccache start --no-depgraph-cache` flag (or env var).

## Implementation Plan

- [ ] Step 1: Add `rkyv` to workspace dependencies
- [ ] Step 2: Define snapshot types + `DEPGRAPH_VERSION` in `zccache-depgraph`
- [ ] Step 3: Implement `DepGraph::to_snapshot()` and `DepGraph::from_snapshot()`
- [ ] Step 4: Implement `save_to_file()` and `load_from_file()` with magic/version/envelope
- [ ] Step 5: Wire into daemon startup + shutdown
- [ ] Step 6: Add periodic save (background tokio task)
- [ ] Step 7: Add GC (1-day TTL, 5 GB cap)
- [ ] Step 8: Protocol extension (`dep_graph_version`, `dep_graph_disk_size`)
- [ ] Step 9: CLI `--no-depgraph-cache` flag + status display
- [ ] Step 10: Unit tests (roundtrip, version mismatch, corrupt file, GC)
- [ ] Step 11: Integration test (daemon restart preserves warm contexts)

## Risks

| Risk | Mitigation |
|------|------------|
| rkyv 0.8→0.9 breaking format | Pin `rkyv = "0.8"` in Cargo.toml; format stable within minor |
| Snapshot too large for 200ms load | Monitor in CI; fallback: load in background, serve cold until ready |
| `PathBuf` → `String` lossy on non-UTF8 | All supported platforms use UTF-8 paths in practice; `to_string_lossy()` is fine |
| Periodic save contention | DashMap iteration is lock-free; serialization happens on cloned data |
