# Disk Artifact Cache

The artifact store persists compiled output files on disk, keyed by content-addressed blake3 hash. Uses redb for indexing and LRU eviction.

For how cache keys are computed see [overview.md](overview.md) (section 2.8). For crash recovery see [runtime.md](runtime.md).

---

## Directory Layout

```
{cache_root}/
  artifacts/
    ab/                          # first 2 hex chars of hash
      cd/                        # next 2 hex chars of hash
        abcdef0123456789.../     # full hash (64 hex chars)
          manifest.json
          output.o               # cached output file(s)
          stdout                 # captured stdout (may be empty)
          stderr                 # captured stderr (may be empty)
  tmp/
    {random}/                    # in-progress writes
  index.redb                     # redb database
```

**cache_root** defaults to `~/.zccache` on all platforms.

## Content Addressing

The artifact directory name is the full blake3 hash (64 hex characters) of the cache key. The two-level prefix directory structure (`ab/cd/`) limits the number of entries per directory, avoiding filesystem performance degradation on large caches.

## Atomic Writes

To prevent partially-written artifacts from being read:

1. Create a temporary directory under `{cache_root}/tmp/{uuid}`.
2. Write all output files and the manifest into the temp directory.
3. `fsync` the temp directory (and files, on Linux, where `fsync` semantics require it).
4. Rename the temp directory to its final path under `artifacts/`. On POSIX, `rename()` is atomic within the same filesystem. On Windows, `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` provides equivalent semantics for directories.
5. Insert into the redb index within a write transaction.

If the daemon crashes between steps 2 and 4, the temp directory is orphaned. On startup, the daemon deletes all entries under `{cache_root}/tmp/`.

## Manifest Format

```json
{
  "version": 1,
  "cache_key": "abcdef0123456789...",
  "compiler": "/usr/bin/gcc",
  "compiler_hash": "...",
  "args_hash": "...",
  "source": "foo.c",
  "source_hash": "...",
  "dep_hash": "...",
  "output_files": [
    { "name": "output.o", "size": 12345, "blake3": "..." }
  ],
  "created_at": "2026-03-08T12:00:00Z",
  "exit_code": 0
}
```

The manifest exists primarily for debugging and corruption detection. The cache key is the directory name; the manifest records what went into it.

## redb Index Schema

The redb database contains two tables:

**Table `artifacts`:**
```
Key:   [u8; 32]        // blake3 hash bytes
Value: ArtifactMeta    // bincode-serialized
```

```rust
struct ArtifactMeta {
    total_size: u64,            // sum of all files in artifact dir
    last_access: SystemTime,    // updated on each lookup
    created_at: SystemTime,
    output_file_count: u32,
}
```

**Table `stats`:**
```
Key:   &str             // stat name
Value: u64              // counter
```

Tracks: `hits`, `misses`, `evictions`, `total_bytes_written`, `total_bytes_evicted`.

redb is chosen for its properties:
- Pure Rust, no external dependencies.
- ACID transactions — the index survives crashes without corruption.
- Single-file database.
- Good read concurrency (multiple concurrent readers, single writer).

## Eviction Policy

**Policy:** LRU by `last_access` time.

**Trigger:** After each artifact store operation, check total cache size. If it exceeds `max_size` (configurable, default 10 GB):

1. Open a redb read transaction.
2. Iterate all entries, sorted by `last_access` ascending.
3. Collect entries to evict until projected size is at most `max_size * 0.9` (evict to 90% to avoid thrashing).
4. Open a redb write transaction.
5. For each entry to evict:
   a. Delete the artifact directory from disk.
   b. Remove the entry from the redb `artifacts` table.
   c. Increment the `evictions` counter.
6. Commit the transaction.

Eviction runs as a background tokio task and does not block lookups or stores.

## Corruption Detection

On artifact lookup:
1. Verify the artifact directory exists.
2. Verify `manifest.json` exists and is parseable.
3. Verify each output file listed in the manifest exists and its size matches.
4. (Optional, not default) Verify blake3 hashes of output files match manifest.

If any check fails, remove the artifact directory and its redb entry, and treat as a cache miss. Log a warning.

On startup, the daemon does NOT do a full integrity scan (too slow for large caches). Corruption is detected lazily on lookup.

## Capability-driven COW materialization

The daemon probes operations instead of trusting filesystem names. The first
materialization for a `(cache volume, target volume)` pair attempts a throwaway
reflink and hardlink and caches the resulting `VolumeCaps`. Cross-volume pairs
short-circuit to the copy tier.

The ordered tiers are:

1. **Reflink:** a new file with shared extents and kernel-enforced COW. The
   daemon restores the blob's stored mtime because clone metadata is separate.
2. **Hardlink COW-lite:** the link is recorded by native file identity, the blob
   and output are read-only, and mediated compiler/tool writes copy-detach.
   Each stored blob carries a durable digest so a restarted daemon can rebuild
   the in-memory ledger safely even when prior aliases were deleted. Watcher
   changes mark entries suspect; the next hit hashes the blob and refuses a
   mismatch with warning and durable lifecycle forensics.
3. **Copy:** used when neither sharing primitive is available. The destination
   is independent and writable.

Windows identity uses `GetFileInformationByHandleEx(FileIdInfo)` and its native
128-bit ID, with the legacy index as a pre-Windows-8 fallback. Link counts are
checked before creation so exhaustion degrades to copy. Eviction and `clear`
remove read-only attributes before deletion.

`ZCCACHE_DISABLE_REFLINK=1` disables cloning and `ZCCACHE_COW_READONLY=0`
disables read-only enforcement. Neither setting adds an IPC roundtrip.
