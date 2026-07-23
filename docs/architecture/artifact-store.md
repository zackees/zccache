# Disk Artifact Cache

The artifact store persists compiled output files on disk, keyed by content-addressed blake3 hash. Uses redb for indexing and LRU eviction.

For how cache keys are computed see [overview.md](overview.md) (section 2.8). For crash recovery see [runtime.md](runtime.md).

---

## Immutable staged-output rollout

The default-on Rust staged-artifact lane makes the daemon's v2 generations
the authoritative source for Rust compiler misses. C/C++ compiler-output
staging is an explicit `ZCCACHE_STAGED_ARTIFACTS=c-cpp` or `all` opt-in. The compiler is
redirected into a private directory before spawn; after a successful compile,
all outputs are hashed and published as one digest-stamped generation, then
materialized to the requested paths. A failed publication can still salvage a
successful compile from the private files, but it never exposes a partial
cache hit.

`ZCCACHE_STAGED_ARTIFACTS=off` restores the legacy path as an immediate kill
switch. Narrow diagnostic values are `rust` (Rust single and multi-output
plans), `c-cpp` (ordinary single-object and single-PCH GCC/Clang plans
including user-owned `-MF`/`-MD` depfiles; MSVC flag rewriting is supported
for explicit `/Fo` object paths), or `all`. Ordinary multi-source GCC/Clang
and MSVC/clang-cl object compilations are split into private per-source
invocations. Default `-MD`/`-MMD` depfiles and MSVC `/Fo` output directories
are included in each unit's complete v2 output set. Shared or explicit `-MF`,
diagnostic JSON, PDB/PCH/listing, module/header-unit, save-temps, split-DWARF,
and dump outputs remain on the legacy path before compiler spawn, as do
unrewritable or undeclared linker outputs, opaque generic exec, and stdout
output. Explicit Rust `--emit=kind=path` outputs
are parsed and included in the complete cache-hit reverse map. Inferred
outputs for staticlibs, bins, proc macros, objects, assembly, LLVM IR/bitcode,
MIR, and dep-info use their actual rustc extensions.

The `off`, `rust`, `c-cpp`, `exec`, and `all` compatibility values remain accepted
for the full 1.13.x release window. The default does not implicitly enable linker staging:
linkers remain explicit `all` opt-ins, while exact generic execution accepts
`exec` or `all`, because their
side-effect inventories require the stricter contract described below.

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
stripped-PDB, and map paths are declared secondary outputs. Apple `-map` and
`-dependency_info` files are also declared and redirected. MSVC incremental
LTCG (`.iobj`), profile-generation (`.pgd`), and
Windows metadata (`.winmd`) products participate when their enabling switches
are active; known implicit names are converted to explicit private
destinations before spawn.

`dsymutil` is the supported directory producer. A normal one-input invocation
is redirected to a private sibling of the requested `.dSYM`, serialized as one
deterministic manifest payload, and published through the same complete v2
generation transaction as file outputs. A hit validates the complete payload,
unpacks it to another private sibling, restores directory/file permissions and
mtimes, then renames it into place. Existing bundles are replaced with the
platform atomic directory-exchange primitive on macOS and Linux. The manifest
rejects traversal, non-UTF-8 paths, symlinks, special files, and excessive
entry/byte counts. Bundle staging is on the destination filesystem so cold
miss publication and requested-path installation cannot fail with a
cross-volume rename.

The audited linker fallback inventory is intentionally bounded:

- MSVC `/IDLOUT`, `/TLBOUT`, and `/MIDL` remain pre-spawn fallbacks because
  module attributes can fan out into IDL, TLB, header, proxy, and IID files in
  tool-version-dependent locations.
- MSVC `/USEPROFILE` requires an explicit PGD path so the profile database is
  hashed as an input. Its implicit-PGD form and deprecated `/LTCG:PG*` modes
  remain pre-spawn fallbacks because output redirection changes their implicit
  database resolution and update semantics.
- Apple `-object_path_lto` and `-save-temps` remain pre-spawn fallbacks because
  retained temporary products depend on LTO decisions and linker version.
- LLVM `--stats=<file>` remains a pre-spawn fallback because it is not a
  portable ELF linker output contract across LLD tool families and versions.
- GNU/LLVM semantic output paths containing `%` or naming a directory remain
  pre-spawn fallbacks because the linker, rather than the invocation, selects
  the final file set.
- `dsymutil` flat/no-output/dump/update/reproducer/resource-embedding and
  `--codesign` modes remain pre-spawn fallbacks. Signing and notarization are
  independent post-publication operations and are never replayed from a
  mutable staged tree.

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

Private compiler/linker files normally live under a per-daemon
`{cache_root}/staging/` directory, outside the clearable artifact store.
Directory producers use a hidden private sibling beside the requested bundle
to preserve same-filesystem atomic installation. An advisory lock protects
each live daemon's directory: startup cleanup reclaims only unlocked crash
debris, while cache clear and eviction cannot delete outputs still needed for
publication salvage or requested-path materialization.

Rust staging adds
`--remap-path-prefix=<private-root>=/__zccache_staged_output__`, and depfiles
plus captured stdout/stderr use the same stable logical marker before hashing
or publication. Miss and hit delivery rehydrate marker paths to the current
requested destinations. Two equivalent compiles can therefore publish
byte-identical generations even when their private staging directories differ;
the conflict guard below remains reserved for genuinely different output.

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

## Thin-v3 durable ownership

`save_rust_plan_local` and `restore_rust_plan_local` are the public durable
export/materialization API for Rust-plan bundles. Callers supply a plan and
cache root; they never derive bundle storage paths. A cache-schema-v3 plan may
select `thin-v3-lifetime-partition-v1` with either `cook-partitioned-v1` or
`zccache-all-v1`. In the partitioned mode, only artifacts explicitly owned by
zccache are exported; cook-owned or unknown artifacts are excluded. The
fallback mode exports all compiler outputs as zccache-owned.

Each manifest entry carries its verified content hash, original mtime, and
durable owner. Restore verifies those fields and rejects a bundle whose owner
metadata does not match the requested mode before materializing any output.
Save/restore summaries expose exported bytes and the stable skip reasons
`cook_owned_artifact_excluded_from_durable_export` and `ownership_unknown`.

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

## Daemon-owned retention policy

The long-lived daemon is the primary maintenance owner. It applies one shared
policy to the exact effective cache root used by that daemon, whether the
service is standalone or embedded. It never discovers or scans a parent,
sibling product directory, another version root, or a global root registry.
No systemd, launchd, or Windows Task Scheduler installation is required.

The artifact-store budget covers allocated artifact files plus pending artifact
writes. Small root-local state such as indexes, depgraphs, logs, journals, and
daemon metadata is outside this budget. The default is 5% of filesystem
capacity, clamped to 40-200 GiB. The resolved budget is reduced when necessary
to preserve the recovery free-space reserve. On small filesystems the reserve
is capped at half the volume, avoiding a zero or 10 GiB-and-below default on
ordinary 30-40 GiB development volumes. Operators can select one
mutually-exclusive override:

- `ZCCACHE_CACHE_SIZE_BYTES=<positive bytes>` for a standalone daemon;
- `ZCCACHE_CACHE_SIZE_PERCENT=<1..100>` for a standalone daemon;
- `ZccacheService::start_with_disk_limits` plus
  `DiskCacheLimits::{max_cache_bytes,max_cache_percent}` for an embedded
  service.

Maintenance counts allocated physical blocks on Unix and uses
`GetCompressedFileSizeW` on Windows, then deduplicates files by native identity
so hardlinks inside the owned root are not double-counted. Pending writes
are charged to usage before a pass chooses a pressure tier. Cache hits refresh
the in-memory access time and checkpoint it to the artifact index at most once
per hour, preserving recent use across restarts without a write per hit.

| Tier | Trigger | Eligible entries | Target |
|---|---|---|---|
| None | Usage below 85% and free space healthy | A full pass still expires entries older than 30 days | No pressure eviction |
| Soft | Usage at least 85% of budget | LRU entries older than 4 days | 70% of budget |
| Hard | Usage at least 100%, or free space below `min(capacity, max(5%, 20 GiB))` | LRU regardless of age | 80% of budget and `min(capacity / 2, max(8%, 30 GiB))` free where feasible |

The daemon runs a pressure pass at startup and every five minutes. A full age
pass is due every 24 hours. Successful full passes write
`.disk-maintenance-last-full-v1` inside the exact cache root; a missing, corrupt,
or overdue marker causes startup catch-up. Consequently the 30-day expiry is
eventual even when no new compile requests arrive while the daemon remains
alive, and restarts catch up after daemon downtime.

Each selected logical key is removed across legacy flat/pack files and staged
v2 generations. The live artifact map, persistent artifact index, and depgraph
references are invalidated only after file removal succeeds. Reports rescan the
owned root, so `usage_after_bytes` and `bytes_reclaimed` reflect what is no
longer present rather than an eviction estimate. The embedded
`ZccacheService::maintain_disk` method lets a host request an immediate pressure
or full pass against that same root; it does not broaden ownership.

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
Unsupported shapes—including shared multi-source side outputs, C++ modules,
unrewritable/undeclared linker outputs, opaque generic exec, and stdout
output—remain on the legacy path before compiler spawn. Explicit Rust `--emit=kind=path`
destinations are included in the complete cache-hit reverse map. Output
families without complete-set plans select the legacy path before spawn; they
are never partially staged.
