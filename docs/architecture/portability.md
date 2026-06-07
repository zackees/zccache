# Portability & Future Extensions

Cross-platform differences and planned extension points.

---

## Platform Differences

| Aspect | Linux | macOS | Windows |
|---|---|---|---|
| IPC | Unix domain socket | Unix domain socket | Named pipe |
| Socket path | `$XDG_RUNTIME_DIR/zccache/sock` | `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-{uid}/sock` | `\\.\pipe\zccache-{username}` |
| File watcher backend | inotify | FSEvents | ReadDirectoryChangesW |
| File ID | `st_dev` + `st_ino` | `st_dev` + `st_ino` | `dwVolumeSerialNumber` + `nFileIndex{High,Low}` |
| Atomic rename | `rename(2)` | `rename(2)` | `MoveFileExW` |
| Lock file PID check | `kill(pid, 0)` | `kill(pid, 0)` | `OpenProcess(SYNCHRONIZE, pid)` |
| Cache root | `~/.zccache` | `~/.zccache` | `~/.zccache` |
| Daemon spawn | `fork` + `setsid` + `exec` | `fork` + `setsid` + `exec` | `CreateProcessW` (detached) |

## Path Handling

**Canonicalization:** All paths stored in the metadata cache are canonicalized (`std::fs::canonicalize`). This resolves symlinks and relative components, ensuring that `/home/user/./foo.c` and `/home/user/foo.c` map to the same entry.

**Case sensitivity:**
- Linux: case-sensitive. No special handling.
- macOS: case-insensitive by default (HFS+/APFS). Canonicalization via `realpath` returns the filesystem's canonical casing. The metadata cache key uses the canonicalized form, which is consistent regardless of the case the user provided.
- Windows: case-insensitive. Paths are canonicalized and stored in the case returned by `GetFinalPathNameByHandleW` (via Rust's `std::fs::canonicalize`).

**UNC paths (Windows):** `std::fs::canonicalize` on Windows returns UNC-prefixed paths (`\\?\C:\...`). These are stored as-is in the metadata cache. The artifact store uses only the cache root (a local path), so UNC paths do not appear in artifact paths.

**Path separators:** Internally, all paths use the platform's native separator. Cache keys hash the **canonicalized path bytes**, so the same file always produces the same hash on a given platform. Cross-platform cache sharing is not a goal.

## Path Remap Auto Precedence

`ZCCACHE_PATH_REMAP=auto` injects compiler remap flags only when the daemon has
an auto-detected or explicitly configured worktree root. For GCC/Clang-family
compile misses, zccache injects `-ffile-prefix-map=<root>=.` and, when the
compile cwd differs from the root, `-ffile-prefix-map=<cwd>=.`. For Rust,
zccache injects a root-covering `--remap-path-prefix <root>=.`.

User-supplied remaps remain authoritative. An exact user
`-ffile-prefix-map=<root>=...` or Rust `--remap-path-prefix=<root>=...`
suppresses the matching root auto-remap. An exact user
`-ffile-prefix-map=<cwd>=...` suppresses the cwd auto-remap. The check is
per-flag and per-path: `-fdebug-prefix-map`, `-fmacro-prefix-map`,
`-fcoverage-prefix-map`, and `-fprofile-prefix-map` do not suppress an auto
`-ffile-prefix-map`, because they do not all cover the same emitted path scopes.

When zccache does inject an auto remap, it prepends the auto remap before the
caller-provided arguments. This makes the auto remap a broad fallback rule:
if the caller also provided a narrower overlapping remap, the compiler sees
the caller's remap later and it remains the winning rule.

### Cache identity is target-dir-shape independent

`CARGO_TARGET_DIR` does not contribute to the rustc request fingerprint or
context key. Two worktrees pointing at the same source tree, the same zccache
cache, and `ZCCACHE_PATH_REMAP=auto` share rustc cache hits even when each
picks a different relative target-dir leaf name (for example
`.claude/worktrees/parent-cache-main-target` vs
`.claude/worktrees/parent-cache-sub-target`). The filter is mechanically
identical to the existing `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH` filter
(issue #139): output-placement and crate-location state are stripped from the
cache key. The constant of record is `VOLATILE_CARGO_ENV_VARS` in
`depgraph::context` — the request fingerprint mirrors the same set so the
fast-path miss/hit decision does not diverge from the slow-path key
computation (issue #396). Cargo `--out-dir`, `-L`, and `--extern` directory
prefixes derived from `CARGO_TARGET_DIR` are already non-cache-key state.
Target directories outside `ZCCACHE_WORKTREE_ROOT` are intentionally not
rewritten into root-relative request-key paths; they remain distinct at the
request-fingerprint layer unless a later depgraph validation proves the
artifact is safe to reuse.

## File Identity

`FileId` is obtained via:
- **Unix:** `std::fs::metadata()` → `std::os::unix::fs::MetadataExt` → `dev()`, `ino()`.
- **Windows:** Open file with `CreateFileW(OPEN_EXISTING, FILE_READ_ATTRIBUTES)`, call `GetFileInformationByHandle`, extract `dwVolumeSerialNumber` and `nFileIndexHigh`/`nFileIndexLow`.

If obtaining the file ID fails (e.g., permission denied, network filesystem that doesn't support it), `file_id` is set to `None` and the entry falls back to `(path, mtime, size)` identity only.

## Diagnosing wrapper-CWD anomalies (`ZCCACHE_DIAG_CWD`)

Set `ZCCACHE_DIAG_CWD=1` to make every `zccache` wrapper invocation print one
tab-separated diagnostic line to stderr **before** the wrapper releases its CWD
handle to `temp_dir()`. The line is tagged `ZCCACHE_DIAG_CWD` and carries:

- `pid` — the wrapper process ID
- `cwd` — the result of `std::env::current_dir()` at process entry (this is the
  value the wrapper will send to the daemon as `Request::Compile.cwd`)
- `tmp` — `std::env::temp_dir()` (the directory the wrapper will chdir to)
- `argv0` — the wrapper's own argv[0] path
- `args` — the wrapped tool + tool args, as the wrapper received them

Useful when the journal shows a cache miss recording a `cwd` that doesn't match
the directory the build system thinks it invoked the compiler from (issue
#683). Because the daemon writes the journal `cwd` field straight from
`Request::Compile.cwd`, this diagnostic captures the truth at the source — if
the diagnostic line shows the wrong directory, an outer shim/build system has
already chdir'd before exec'ing `zccache.exe`. If the line shows the *right*
directory but the journal still shows the wrong one, that points at a daemon
bug.

The diagnostic is gated, single-line, and writes to stderr — it adds no
roundtrips and does not affect exit status.

## Watcher Behavior Differences

- **inotify (Linux):** Per-directory watches. Recursive watching requires registering each subdirectory. The `notify` crate handles this. Watch limit: `/proc/sys/fs/inotify/max_user_watches` (default 8192 or 65536 depending on distro). If exhausted, fall back to polling.
- **FSEvents (macOS):** Stream-based, naturally recursive. Low overhead. May deliver events with a slight delay (latency configurable, set to 100ms). Delivers `MustScanSubDirs` on overflow.
- **ReadDirectoryChangesW (Windows):** Per-directory, can be recursive. Buffer overflow possible under heavy I/O; `notify` reports this as an error.

---

## Future Extension Points

### Remote / Shared Cache

The artifact store interface can be extended with a `RemoteStore` backend:

```rust
#[async_trait]
trait ArtifactBackend {
    async fn lookup(&self, key: &Blake3Hash) -> Option<Artifact>;
    async fn store(&self, key: &Blake3Hash, artifact: Artifact) -> Result<()>;
}
```

A `ChainedStore` would check local first, then remote. Remote candidates: S3-compatible object storage, HTTP server, or a custom protocol. The content-addressed design makes this natural — the cache key is the same regardless of where the artifact is stored.

### Distributed Build Cache

Multiple machines on a team could share a remote artifact store. Requirements:
- Compiler identity must include target triple and relevant system header hashes.
- Environment normalization must be stricter (filter more variables).
- Artifact format must be verified more carefully (hash verification on download).

### Additional Compilers

The compiler argument parser is pluggable. Each compiler family (GCC, Clang, MSVC) has its own arg parser implementing a common trait:

```rust
trait CompilerArgParser {
    fn parse(&self, args: &[String]) -> Result<ParsedCompilation>;
    fn is_cacheable(&self, parsed: &ParsedCompilation) -> bool;
    fn cache_relevant_args(&self, parsed: &ParsedCompilation) -> Vec<String>;
    fn cache_relevant_env(&self, parsed: &ParsedCompilation) -> Vec<(String, String)>;
}
```

Adding a new compiler (e.g., MSVC `cl.exe`, `nvcc`) requires implementing this trait.

### Preprocessor Integration

The MVP hashes preprocessor output as the dependency hash. This is correct but slow (runs the preprocessor on every compilation). Future improvements:

1. **Dependency file parsing:** After a cache miss, parse the `-MD`-generated `.d` file to discover the exact set of headers used. Cache this set. On subsequent compilations with the same source, hash only the individual headers instead of running the preprocessor.
2. **Include scanning:** Parse `#include` directives without running the preprocessor. Faster but less accurate (misses conditional includes).
3. **Persistent dependency graph:** Store the source-to-headers mapping in redb. Invalidate edges when headers change.

### Persistent Metadata Cache

The in-memory metadata cache could be serialized to disk on shutdown and loaded on startup, avoiding the cold-start cost of stat-verifying all files. Implementation:
- Serialize to a file in the cache root on graceful shutdown.
- On startup, load the file, but set all entries to `Low` confidence (we don't know what changed while the daemon was down).
- The watcher-based promotion to Medium and stat-based promotion to High proceed as normal.

This trades a small amount of startup I/O for faster warm-up on the first build after daemon restart.

### Build System Integration

Direct integration with build systems (CMake, Meson, Bazel) could provide richer information:
- Exact dependency lists without preprocessing.
- Compiler version and target triple from build system configuration.
- Output path and intermediate file management.

This is a non-goal for the initial implementation but the daemon's IPC interface can be extended to accept richer requests.
