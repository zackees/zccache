# Disk Artifact Cache

The artifact store persists compiled output files on disk, keyed by content-addressed blake3 hash. Uses redb for indexing and LRU eviction.

For how cache keys are computed see [overview.md](overview.md) (section 2.8). For crash recovery see [runtime.md](runtime.md).

---

## Immutable staged-output rollout

The default-on staged-artifact lane makes the daemon's v2 generations
the authoritative source for supported compiler misses. The compiler is
redirected into a private directory before spawn; after a successful compile,
all outputs are hashed and published as one digest-stamped generation, then
materialized to the requested paths. A failed publication can still salvage a
successful compile from the private files, but it never exposes a partial
cache hit.

`ZCCACHE_STAGED_ARTIFACTS=off` restores the legacy path as an immediate kill
switch. Narrow diagnostic values are `rust` (Rust single and multi-output
plans), `c-cpp` (ordinary single-object and single-PCH GCC/Clang plans
including user-owned `-MF`/`-MD` depfiles; MSVC flag rewriting is supported
only for explicit `/Fo` object paths), or
`all`. Unsupported shapes—including multi-source compiler invocations, C++
modules, unrewritable or undeclared linker outputs, opaque generic exec, and
stdout output—remain on the
legacy path before compiler spawn. Explicit Rust `--emit=kind=path` outputs
are parsed and included in the complete cache-hit reverse map. Inferred
outputs for staticlibs, bins, proc macros, objects, assembly, LLVM IR/bitcode,
MIR, and dep-info use their actual rustc extensions.

Pure archive invocations with one output and no linker side effects use the
same private transaction by default.

Linker invocations remain `all`-gated because opaque tools can create
undeclared siblings. They participate when the parser's primary
and declared secondary destinations can all be rewritten before spawn. The
private directory is checked for undeclared files/bundles after the linker
exits, and the requested output directory is checked for external side
effects. Either condition prevents cache publication; declared outputs are
independently salvaged to preserve a successful link.
Explicit GNU/LLVM map and dependency-file paths and active MSVC PDB, ILK,
stripped-PDB, and map paths are declared secondary outputs. Implicit names,
GNU semantic map destinations (`%` or a directory), and conditional
LTCG/PGO/embedded-IDL/Windows-metadata outputs remain on the legacy path
before spawn.

Generic tool execution participates by default when every declared output is
an exact argument token. Those paths are rewritten into a private staging
directory before spawn and independently materialized after the run. Generic
tools whose output paths are embedded in opaque arguments, environment
variables, or undeclared side effects retain the legacy path.

Published v2 files are always independent copies or true reflinks of private
compiler files. Hardlinks are never used between compiler staging and the
backend. Requested-output delivery may use the hardlink-shared tier only for
parser-authorized rustc metadata and `lib`/`rlib` archives; a `.rlib` suffix
without the matching rustc crate type is not authorization. SQLite, databases,
incremental state, depfiles, executables, and unknown outputs remain on
reflink/copy because an NTFS hardlink is a shared mutable inode, not COW.

Every v2 output carries the same durable COW digest sidecar used by the legacy
hardlink registry. This lets restart verification reject a mutate-then-delete
alias attack before the backend is served. Read-only enforcement, watcher
suspicion, file-identity registration, link-count limits, and copy fallback
remain mandatory for the narrow semantic allowlist.

Private compiler/linker files live under a per-daemon `{cache_root}/staging/`
directory, outside the clearable artifact store. An advisory lock protects
each live daemon's directory: startup cleanup reclaims only unlocked crash
debris, while cache clear and eviction cannot delete outputs still needed for
publication salvage or requested-path materialization.

The v2 transaction is visible only after the complete generation and manifest
are written and the per-key pointer is switched. Readers validate the pointer,
manifest, sizes, and every output digest before serving a hit. Startup removes
abandoned staging directories and pointer temporary files. The current flat
v1 and pack formats remain readable during rollout.

Publication holds a shared store lock plus an exclusive per-key lock. Cleanup
and cache Clear hold the store lock exclusively, so neither can remove an
active transaction. If a valid generation already exists and the same cache
key produces different bytes, publication fails closed, preserves the first
generation, and emits a durable `staged_publication_conflict` lifecycle event.
An invalid/corrupt prior generation may be replaced and is recorded as
`staged_publication_replaces_invalid_generation`.

Mixed-format lookup is explicit during migration: v2 is attempted first,
then flat v1 payloads, then pack payloads. Disabling staged artifacts (or
downgrading to a reader without v2 support) leaves v1/pack entries readable
and treats v2-only entries as cache misses; v2 bytes are never reinterpreted
as a legacy format. Re-enabling a v2-aware reader makes those generations
available again. Disk eviction groups coexisting v1/pack/v2 storage by cache
key, accounts for all physical bytes, and removes the logical artifact once.

Session phase profiles include a bounded `staged` summary. The compile-miss
lane populates planning, compiler staging, hashing, publication, salvage, and
requested-path materialization. V2 file hits report the tier that actually
succeeded (reflink, hardlink-shared, or copy), copied bytes, failures, and
elapsed ns. Archive, declared-linker, and exact-exec misses use the same
planning, private-tool execution, complete-generation publication/index commit,
salvage, and materialization accounting. Exact exec persists staged output paths
as v2 generations before requested-path materialization; it no longer converts
that lane back into asynchronous flat-v1 payload writes. Publication,
salvage, and materialization use path-scoped, one-shot test faults at commit and
per-output edges. Task-local mirroring attributes compile observations to the
owning tracked session while preserving daemon aggregates; concurrent sessions
and unscoped ephemeral requests cannot cross-contaminate staged totals. The
summary reports counters, nanosecond totals, copied-byte
totals, and stable failure reason IDs. Labels are daemon-owned constants:
paths, argv, cache keys, and raw OS errors are never metric keys. Bincode
protocol v18 carries this summary; the protobuf schema adds it as an optional
message so older protobuf readers continue to ignore it safely. Clear resets
these totals with the existing phase profiler. The additive protobuf field
advances that lane to protocol v19. Salvage and requested-path materialization
failures also emit durable lifecycle records.

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
Unsupported shapes—including multi-source compiler invocations, C++ modules,
unrewritable/undeclared linker outputs, opaque generic exec, and stdout
output—remain on the
legacy path before compiler spawn. Explicit Rust `--emit=kind=path`
destinations are included in the complete cache-hit reverse map. Output
families without complete-set plans select the legacy path before spawn; they
are never partially staged.
