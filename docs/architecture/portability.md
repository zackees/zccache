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
| Staged delivery | reflink/hardlink/copy by volume capability | clonefile/reflink or copy by volume capability | ReFS block clone, registered NTFS hardlink, or copy |

The immutable staged-output transaction is platform independent: every
platform publishes a complete digest-stamped generation before requested-path
delivery. Only the final materialization tier differs. Session
`phase_profile.staged` reports the actual reflink, hardlink-shared, or copy
tier and copied bytes, so the cross-platform perf gate does not infer behavior
from the operating-system label.

`ZCCACHE_STAGED_ARTIFACTS=off` and the `rust`, `c-cpp`, `exec`, and `all` diagnostic
values are portable and retained through 1.13.x. The kill switch leaves legacy
v1/pack reads available on Linux, macOS, and Windows; v2-only entries become
misses rather than being reinterpreted.

### System include discovery: `cl.exe` reads `%INCLUDE%`, never a probe

For gcc and clang the daemon discovers the default `#include <...>` search
roots by spawning the compiler (`-v -E -x c++ NUL`, or the faster `-###` for
the clang family) and parsing the printed search list. **Microsoft's `cl.exe`
has no equivalent discovery mode**: it resolves `#include <...>` purely against
the `;`-separated `%INCLUDE%` that `vcvars` exports. Probing it spawns a
compiler that rejects the preprocessor flags and prints no search list, and the
resulting "process succeeded, zero paths" outcome is indistinguishable from a
degraded probe — which is exactly how the issue #1167 guard read it, sending
every MSVC compile down the uncached bypass and (because #1167 correctly never
memoizes an empty result) re-firing on every single compile. That was issue
#1530: a 2,300-compile MSVC build cached zero artifacts.

So `cl.exe` short-circuits discovery and reads `INCLUDE` out of the forwarded
client environment (`msvc_cl_system_includes` in the compile pipeline;
`zccache_depgraph::msvc_system_includes_from_env` does the parsing). Two
consequences worth keeping in mind:

- **The result is not memoized in the L1/L2 system-include cache.** `%INCLUDE%`
  belongs to the client shell — which `vcvars` ran, for which architecture —
  not to the compiler binary, so a per-compiler-path entry would leak one
  shell's roots into another's. Two shells cannot collide on one cache entry:
  the discovered roots are pushed into `ctx.include_search.system`
  (`daemon/server/rustc.rs`) and hashed **in order** into `ContextKey`
  (`depgraph/src/context/mod.rs`), so a different `%INCLUDE%` — different
  directories, or the same ones in a different order — yields a different key.
  (Note this is the keying path; the `INCLUDE` entry in
  `compile_journal/env.rs` is the durable-journal allowlist and does *not*
  feed the cache key.)
- **An absent `INCLUDE` is a known-empty result, not a degraded one.** `cl.exe`
  genuinely has no other search roots, and the miss path recovers the real
  header set from `/showIncludes` regardless.

`clang-cl` also classifies as `CompilerFamily::Msvc` (it speaks MSVC argument
syntax) but *is* a clang driver: it answers the probe and owns builtin header
directories that `%INCLUDE%` does not list, so it stays on the discovery path.
`zccache_compiler::is_msvc_cl` is the discriminator.

### `/showIncludes` is read from stdout, and stripped only when injected

MSVC's dependency mechanism is `/showIncludes`, which prints one
`Note: including file: <path>` line per header. Unlike `gcc -H` / `clang -H`
(stderr), **`cl.exe` and `clang-cl` write these notes to stdout.** The daemon
scanned stderr, found nothing, and recorded zero dependencies for every MSVC
compile — harmless only for as long as MSVC never cached at all. The moment the
`%INCLUDE%` fix above made MSVC cacheable, that turned into a stale hit: edit a
header, get the pre-edit object back. Both halves of issue #1530 therefore ship
together; the `%INCLUDE%` change is not safe to land alone.

Whether the notes are stripped from the caller's stdout depends on **who asked
for the flag**, tracked as `injected_show_includes` at the injection site in
`compile_exec.rs`:

- **Daemon injected it** (the caller passed no `/showIncludes`): the notes are
  internal bookkeeping and are filtered out, so the caller's stdout looks
  exactly as it would without zccache.
- **Caller passed it** (CMake + Ninja MSVC builds do, and parse the notes into
  their own depfiles): the notes are scanned but passed through byte-for-byte.
  Stripping them would silently break the build system's dependency tracking.

Because the argument parser consumes `/showIncludes` without recording it as a
cache-relevant flag, those two shapes would otherwise hash to the *same*
`ContextKey` while storing different stdout — so a stripped entry could be
replayed to a Ninja caller as an empty depfile. `keys::msvc_show_includes_key_flags`
salts the key with who asked for the flag, keeping the populations separate. It
costs no hits: every compile in a given build is spelled the same way.

The flag is matched case-insensitively with either prefix (`cl.exe` accepts
`-showincludes` for `/showIncludes`). clang-cl's suffixed
`/showIncludes:user` deliberately omits system headers, so it is *not* treated
as a usable dependency set: the daemon neither injects a competing
`/showIncludes` nor trusts the partial trace — it leaves the strategy
`Unsupported`, falls back to its own header scanner, and leaves the caller's
stdout untouched.

`StderrFilter::reads_stdout()` / `strips_scanned_lines()` in
`daemon/compile_output.rs` encode this for the streaming path; the buffered
path in `compile_exec.rs` mirrors it. `clang -H` header traces keep reading
stderr and keep being stripped unconditionally — they are only ever injected.

---

## Host-Platform Boundary (`zccache-platform`)

Host mechanics are selected in exactly one place: `crates/zccache-platform/src/lib.rs`,
through `std::cfg_select!` (Rust 1.95.0). Every other production crate consumes the
neutral facades re-exported there and aliases the leaf as:

```rust
pub(crate) use zccache_platform as platform;
```

```
crates/zccache-platform/src/lib.rs        the one host selector (no fallback arm)
crates/zccache-platform/src/platform.rs   neutral facade root
crates/zccache-platform/src/platform/     process | fs | ipc | executable | host
crates/zccache-platform/src/platform_win.rs    private concrete Windows tree
crates/zccache-platform/src/platform_linux.rs  private concrete Linux tree
crates/zccache-platform/src/platform_macos.rs  private concrete macOS tree
```

Rules:

- **One selector, five facades.** Concrete trees are private and cannot be
  named downstream; unsupported host OSes fail compilation at the selector.
  Linux and macOS stay separate trees even where they share call sites.
- **Leaf crate.** zccache-platform never depends on a `zccache-*` crate and
  never carries product types (`NormalizedPath`, `Config`, protocol messages,
  audit events, …). Callers translate primitive results into product types
  and diagnostics.
- **Host is not compiler target.** This crate answers "what OS is this
  zccache process running on?" Compiler/build-target decisions — `rustc
  --target` parsing, MSVC/GNU/Apple linker modes, output extensions from an
  explicit triple — stay in zccache-compiler. A Linux-hosted
  `--target …-windows-…` build uses Linux host mechanics and Windows artifact
  naming; only the no-target fallback consults the host facade.
- **Enforcement.** The `enforce_platform_boundary` Dylint inspects
  pre-expansion source (inactive host branches included) and rejects host
  cfg/cfg_attr/cfg!, native imports (`std::os::*`, `libc`, `windows-sys`),
  and concrete-module references outside this crate. A transitional
  exact-occurrence baseline grandfathers pre-migration sites and ratchets to
  zero; new occurrences fail immediately.
- **Publish.** `ci/publish_amalgamate.py` copies the crate into the published
  `zccache` crate as a private `platform` module (`zccache_platform::` paths
  become `crate::platform::`). It is not a public crates.io API.

Phase order: #1366 bootstrap/toolchain/lint → #1367 `fs` → #1368 `ipc` →
#1369 `process` → executable/host → zero-baseline cleanup.

## Path Handling

Private compiler outputs default to `{cache_root}/staging`. Set
`ZCCACHE_STAGING_DIR` to a shorter base when a platform tool cannot consume
the cache-root-derived path. zccache creates and cleans only a
`zccache-staging` child beneath that base. The override does not relocate
durable artifacts or disable staged publication; each daemon still owns a
locked child and removes it at shutdown. Embedded services should pass the
same base explicitly through `ZccacheStartOptions::staging_root` so concurrent
instances remain independent.

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
3. **Persistent dependency graph:** Store the source-to-headers mapping in a persistent graph. Invalidate edges when headers change. *(Implemented — see `zccache-depgraph`, which snapshots the graph with rkyv rather than a database.)*

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
