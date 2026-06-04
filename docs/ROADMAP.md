# zccache Implementation Roadmap

This document describes the phased implementation plan for **zccache**, a high-performance local compiler cache daemon written in Rust. Each phase builds on the previous one and is designed to be independently shippable and testable. An engineer should be able to pick up any phase and start implementing once its predecessors are complete.

---

## Phase 0: Scaffolding and Standards

**Goals:** Establish project infrastructure. Every crate exists (even if empty), CI is green, coding standards are enforced, and foundational types are in place.

**Non-goals:** No runtime functionality yet. Nothing listens on a socket, nothing compiles code, nothing caches artifacts.

**Deliverables:**

- Rust workspace (`Cargo.toml` at root) with all crates stubbed out:
  - `zccache-core` -- shared types, error types, tracing setup, path abstractions
  - `zccache-hash` -- cache key computation (blake3)
  - `zccache-protocol` -- IPC message definitions and serialization
  - `zccache-fscache` -- file metadata cache
  - `zccache-artifact` -- content-addressed artifact store
  - `zccache-watcher` -- file-system watcher abstraction
  - `zccache-compiler` -- compiler argument parsing and wrapping
  - `zccache-ipc` -- platform-specific IPC transport (Unix domain socket / named pipe)
  - `zccache-daemon` -- daemon binary
  - `zccache-cli` -- CLI binary
  - `zccache-test-support` -- shared test utilities, fixtures, temp-dir helpers
- CI pipeline (GitHub Actions):
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo fmt --all -- --check`
  - `cargo test --workspace`
  - MSRV check (1.94.1)
  - Matrix: Linux (ubuntu-latest), macOS (macos-latest), Windows (windows-latest)
- `.rustfmt.toml` with project conventions
- Clippy configuration (`clippy.toml` or workspace-level `Cargo.toml` lint settings)
- `cargo-deny` setup: license checks, advisory database audit, duplicate crate detection
- `ARCHITECTURE.md`, `DESIGN_DECISIONS.md`, `ROADMAP.md` (this file)
- Core error types using `thiserror` in `zccache-core`
- Tracing setup (`tracing` + `tracing-subscriber`) with structured logging, env-filter, and a default subscriber initializer in `zccache-core`
- Cross-platform path abstraction types in `zccache-core` for normalizing and comparing paths consistently across Linux, macOS, and Windows

**Key tests:**

- Workspace compiles on all three platforms (Linux, macOS, Windows)
- CI pipeline passes end-to-end
- Core error types can be constructed and formatted
- Tracing subscriber initializes without panic

**Risks:** None significant. This phase is pure infrastructure.

---

## Phase 1: Minimal Daemon + CLI + IPC

**Goals:** CLI can start the daemon, send a ping, and get a response. This phase establishes the foundation for all future communication between CLI and daemon.

**Non-goals:** No caching, no compilation, no file watching, no artifact storage.

**Deliverables:**

- Daemon binary (`zccache-daemon`) that listens on:
  - Unix domain socket (Linux / macOS)
  - Named pipe (Windows)
- CLI binary (`zccache-cli`) that:
  - Discovers a running daemon (checks well-known socket/pipe path)
  - Auto-starts the daemon if none is running
  - Sends a ping request and prints the pong response
- IPC transport abstraction in `zccache-ipc`:
  - `Transport` trait with `connect`, `send`, `recv` methods
  - `UnixTransport` implementation (tokio `UnixStream`)
  - `NamedPipeTransport` implementation (tokio named pipe on Windows)
- Protocol message types in `zccache-protocol`:
  - `Ping` / `Pong`
  - `Shutdown`
  - `Status` (daemon uptime, version, connection count)
  - Length-prefixed framing with serde/bincode serialization
- Daemon lifecycle management:
  - Startup: bind socket/pipe, write lock file (containing PID)
  - Lock file: prevent multiple daemon instances
  - Stale socket/lock detection: if lock file PID is dead, clean up and take over
  - Idle timeout: shut down after configurable period with no connections
  - Signal handling: graceful shutdown on SIGTERM/SIGINT (Unix) and Ctrl-C (Windows)

**Key tests:**

- Daemon starts and accepts connections on all platforms
- CLI auto-starts daemon when no daemon is running
- Ping/pong round-trip completes successfully
- Multiple CLI instances can connect concurrently
- Stale socket/lock file is detected and cleaned up
- Daemon shuts down gracefully on signal
- Daemon shuts down after idle timeout
- Cross-platform IPC works (verified by CI matrix)

**Risks:** Platform-specific IPC bugs, especially Windows named pipes with tokio (less mature than Unix domain sockets). **Mitigation:** Test on all three platforms in CI from day one; keep the transport trait narrow so platform-specific code is isolated and replaceable.

---

## Phase 2: Local Artifact Caching

**Goals:** Store and retrieve compilation artifacts by cache key. No real compiler integration yet -- use synthetic test data to exercise the full store/lookup path.

**Non-goals:** No real compiler wrapping, no file watching, no metadata cache.

**Deliverables:**

- blake3-based cache key computation in `zccache-hash`:
  - Deterministic key builder that accepts ordered inputs (compiler identity, flags, source hash)
  - Key type: 32-byte blake3 digest, hex-encoded for filesystem paths
- Content-addressed artifact store in `zccache-artifact`:
  - `write(key, artifact_data) -> Result<()>`: store artifact bytes under key
  - `read(key) -> Result<Option<ArtifactData>>`: retrieve artifact bytes by key
  - `exists(key) -> bool`: check for cached artifact
  - Storage layout: `<cache_root>/objects/<first-2-hex>/<remaining-hex>/`
  - Atomic writes: write to temp dir under `<cache_root>/tmp/`, then rename into place
  - Manifest file per artifact: output file(s), stdout, stderr, exit code, timestamp, size, checksum
- `redb` index for:
  - Cache key to artifact path mapping
  - Access time tracking (for LRU eviction)
  - Total cache size tracking
- LRU eviction:
  - Configurable max cache size (default: 10 GB)
  - Eviction triggered when cache exceeds high-water mark (e.g., 95% of max)
  - Evicts least-recently-accessed entries until below low-water mark (e.g., 80% of max)
- Corruption detection:
  - Manifest includes blake3 checksum of artifact data
  - On read, verify checksum; if mismatch, delete entry and return miss
- Protocol messages in `zccache-protocol`:
  - `Store { key, artifact }` / `StoreResponse { success }`
  - `Lookup { key }` / `LookupResponse { hit, artifact? }`
  - `CacheStats` / `CacheStatsResponse { total_size, entry_count, hit_count, miss_count }`
- Daemon integration: handle `Store`, `Lookup`, `CacheStats` requests
- CLI commands:
  - `zccache stats`: display cache statistics
  - `zccache clear`: purge all cached artifacts

**Key tests:**

- Store an artifact and retrieve it correctly
- Atomic write prevents partial/corrupt artifacts on crash (simulate with process kill)
- Concurrent stores of the same artifact key are safe (no corruption, last-write-wins or dedup)
- Eviction removes oldest entries when cache exceeds threshold
- Corrupt artifacts (tampered data) are detected and removed on read
- `zccache stats` reports accurate numbers
- `zccache clear` removes all entries and resets stats
- Cache survives daemon restart (redb persistence)
- Cache key is deterministic: same inputs always produce same key

**Risks:** `redb` edge cases under highly concurrent access (many simultaneous writes to the same table). **Mitigation:** Run stress tests with 50+ concurrent writers; wrap `redb` access behind a service with controlled concurrency; keep the `redb` schema simple (two tables: keys, access times).

---

## Phase 3: File Metadata Cache

**Goals:** Maintain a fast in-memory cache of file stat metadata to reduce redundant filesystem stat calls during cache key computation. This is a building block for Phase 4 (watcher) and Phase 5 (compiler wrapper).

**Non-goals:** No file watcher yet (metadata is updated only by explicit stat calls). No content hashing integration (digest is stored opportunistically but not computed automatically).

**Deliverables:**

- File metadata types in `zccache-fscache`:
  - `FileMetadata { path, mtime, size, file_id, content_digest: Option<blake3::Hash>, confidence, last_verified }
  - `file_id`: inode number (Unix) or file index (Windows) for detecting file replacement
- `DashMap`-based metadata cache:
  - `lookup(path) -> Option<FileMetadata>`: return cached metadata if confidence >= threshold
  - `stat_and_update(path) -> FileMetadata`: stat the file, update cache, return fresh metadata
  - `invalidate(path)`: remove entry from cache
  - `invalidate_prefix(dir)`: remove all entries under a directory
- Stat verification logic:
  - Compare `(mtime, size, file_id)` from cache against actual stat result
  - If all three match: entry is still valid, bump confidence to `High`
  - If any differ: entry is stale, replace with fresh stat, clear `content_digest`
- Confidence levels:
  - `High`: recently verified by explicit stat (within last N seconds)
  - `Medium`: watcher reports no change (not used yet, reserved for Phase 4)
  - `Low`: stale, needs re-verification before use
- Confidence decay: entries degrade from `High` to `Low` over time (configurable decay interval, default 30 seconds) without re-verification
- Opportunistic content digest storage: when a caller computes a file's blake3 digest, it can store it alongside the metadata for future lookups
- Cache introspection: entry count, hit/miss counters, average confidence distribution

**Key tests:**

- Metadata lookup returns correct `mtime`, `size`, `file_id` values
- Changed file (different mtime or size) is detected on re-stat
- Replaced file (same path, different inode/file_id) is detected
- Confidence levels degrade from `High` to `Low` over time
- `invalidate(path)` removes the entry
- `invalidate_prefix(dir)` removes all entries under that directory
- Concurrent access from multiple threads is safe and correct
- Large caches (100k+ entries) maintain acceptable lookup latency (< 1 microsecond p99)
- Opportunistic digest is cleared when file metadata changes

**Risks:** Subtle race conditions between calling stat and the actual file state changing (TOCTOU). **Mitigation:** The metadata cache is never the sole source of truth for correctness -- always re-stat before trusting a cached digest for cache key computation. The metadata cache is a performance optimization, not a correctness mechanism.

---

## Phase 4: File Watcher Integration

**Goals:** A file watcher updates the metadata cache in near-real-time, reducing the need for explicit stat calls during builds. The watcher boosts metadata confidence but is never the sole source of truth.

**Non-goals:** The watcher is not relied upon for correctness. No header dependency tracking. No recursive deep-watch of entire filesystem.

**Deliverables:**

- Watcher abstraction trait in `zccache-watcher`:
  - `Watcher` trait: `watch(path)`, `unwatch(path)`, `recv() -> WatchEvent`
  - `WatchEvent { path, kind: Created | Modified | Removed }`
- `notify`-based implementation:
  - Linux: inotify
  - macOS: FSEvents
  - Windows: ReadDirectoryChangesW
- Polling fallback mode:
  - Used when native watcher is unavailable or unreliable
  - Configurable poll interval (default: 2 seconds)
  - Detects changes via stat comparison against cached metadata
- Watcher-to-metadata-cache integration:
  - On `Modified` / `Created` event: set entry confidence to `Medium`, clear cached `content_digest`
  - On `Removed` event: invalidate entry
  - "No news is good news": if watcher is healthy and reports no event for a path, confidence stays at current level (does not decay as fast)
- Overflow handling:
  - If the OS event queue overflows (e.g., Linux inotify queue full, Windows buffer overflow): downgrade all watched entries to `Low` confidence
  - Log a warning with `tracing`
  - Re-establish watches
- Watch scope management:
  - `add_watch(directory)`: start watching a directory (non-recursive by default, recursive opt-in)
  - `remove_watch(directory)`: stop watching
  - Daemon tracks which directories are relevant based on compilation requests
- Watcher lifecycle:
  - Watcher runs on a dedicated OS thread (not tokio runtime) to avoid blocking async tasks
  - Events forwarded to daemon via `tokio::sync::mpsc` channel
  - Watcher restarts automatically after transient errors

**Key tests:**

- File modification is detected by watcher within reasonable time (< 1 second on native, < poll interval on polling)
- Metadata cache confidence is updated to `Medium` on watcher event
- Content digest is cleared on modification event
- File removal triggers cache invalidation
- Overflow event causes all watched entries to downgrade to `Low`
- Polling fallback detects changes when native watcher is unavailable
- Watcher restarts after transient error
- Adding and removing watch scopes works correctly
- Cross-platform watcher works (verified by CI matrix)
- Watcher handles rapid successive changes to the same file (debouncing or last-event-wins)

**Risks:** Platform-specific watcher quirks -- macOS FSEvents coalesces events and may deliver them late; Windows `ReadDirectoryChangesW` can silently overflow its buffer; Linux inotify has a per-user watch limit. **Mitigation:** Polling fallback is always available; the defensive confidence model means watcher bugs cause at worst a performance regression (extra stat calls), not correctness bugs.

---

## Phase 5: Compiler Wrapper MVP

**Goals:** Actually cache gcc/clang C compilations end-to-end. A user can run `zccache wrap -- gcc -c foo.c -o foo.o` and get transparent caching.

**Non-goals:** C++ support (complex template/header interactions), header dependency tracking beyond what the preprocessor provides, MSVC (`cl.exe`) support.

**Deliverables:**

- Compiler argument parser in `zccache-compiler`:
  - Detect compiler type from argv[0] or explicit flag: `gcc`, `cc`, `clang`
  - Extract key fields: source file, output file (`-o`), include paths (`-I`), defines (`-D`), optimization level, warning flags, standard (`-std=`), target (`-target`, `-march`)
  - Classify flags as: affects-output (part of cache key), does-not-affect-output (ignored), unknown (makes invocation non-cacheable or pass-through)
- Cacheability check -- reject and pass through:
  - Linking (no `-c` flag, or `-shared`, `-o a.out`)
  - Preprocessing-only (`-E`, `-M`, `-MM`)
  - Multiple source files in one invocation
  - Assembly output (`-S`)
  - Reading from stdin (`-`)
  - Invocations with unknown/unrecognized flags (conservative: pass through rather than risk incorrect caching)
- Cache key computation:
  - Compiler identity: blake3 hash of compiler binary (by path), plus `compiler --version` output
  - Sorted relevant compiler flags (normalized)
  - Relevant environment variables (`CPATH`, `C_INCLUDE_PATH`, `SDKROOT`, etc.)
  - Preprocessed source hash: blake3 of `compiler -E <source>` output (captures all included headers)
- Preprocessor integration:
  - Run `compiler -E -fdirectives-only <args> <source>` to get preprocessed output
  - Hash the preprocessed output as the source component of the cache key
  - Use metadata cache (Phase 3) to skip re-preprocessing when source and includes haven't changed (future optimization)
- Wrapper entrypoint:
  - `zccache wrap -- gcc -c foo.c -o foo.o`
  - Parses args, checks cacheability, computes cache key, queries daemon
- On cache hit:
  - Copy cached artifact (object file) to output path
  - Replay cached stdout to stdout
  - Replay cached stderr to stderr
  - Return cached exit code
- On cache miss:
  - Execute the real compiler with original arguments
  - Capture stdout, stderr, exit code
  - If exit code == 0: store artifact (object file + stdout + stderr + exit code) in cache via daemon
  - If exit code != 0: do not cache, pass through output as-is
- Debug mode (`zccache wrap --debug -- gcc ...`):
  - Print cacheability decision and reason
  - Print cache key inputs and final key
  - Print hit/miss result and timing breakdown
- Protocol messages in `zccache-protocol`:
  - `CompileRequest { cache_key, compiler_args, source_path, output_path }`
  - `CompileResponse { hit, artifact?, exit_code, stdout, stderr, timing }`

**Key tests:**

- Simple C file (`hello.c`) compiles and the result is cached
- Second compilation of the same file is a cache hit with identical output
- Changing source file content causes a cache miss
- Changing a compiler flag (e.g., `-O0` to `-O2`) causes a cache miss
- Changing an included header causes a cache miss (preprocessor output changes)
- Non-cacheable invocations (linking, `-E`, multiple sources) pass through to the real compiler without error
- Concurrent compilations of different files work correctly
- Concurrent compilations of the same file work correctly (no corruption)
- Exit code is propagated correctly (both 0 and non-zero)
- Stderr from the compiler is replayed on cache hit
- Debug mode prints useful, accurate information
- Wrapper works when the daemon is not running (auto-starts it)

**Risks:** Compiler argument parsing is complex and varies between compiler versions. Unrecognized flags could lead to incorrect caching if not handled conservatively. **Mitigation:** Start with a small, curated set of recognized flags; treat all unrecognized flags as making the invocation non-cacheable (pass through). This is safe: it can only cause cache misses, never incorrect cache hits. Expand the recognized flag set incrementally based on real-world usage.

---

## Phase 5.5: Build System Configure Cache (`zccache meson configure`)

**Goal:** Skip the meson `setup` (configure) phase on identical inputs. The compile cache (Phase 5) already handles the per-TU step; this addresses the once-per-build-cycle configure cost that the compile cache cannot reach.

**Motivation:** Issue #627. Field measurement on a large FastLED checkout: the configure phase alone is 50 s on a cold run (683 build targets, 1434 ninja rules). The same checkout on a warm cache restores in 8 s — a 6.2× speedup, 42 s saved per build cycle. Multiply by N rebuilds per day × M developers.

**Deliverable (shipped):** `zccache meson configure --source-dir SRC --build-dir BUILD [-- meson-args...]`. Implemented in `crates/zccache/src/cli/commands/meson_cache.rs`.

**Cache key (blake3 with domain tag `"zccache-meson-cache-v1"`):**
- `[meson.build, meson.options, meson_options.txt]` discovered recursively under source dir, each (relative-path, content) pair sorted
- `meson --version` output
- Source-dir + build-dir absolute paths (same-build-dir restriction)
- Selected env vars (`CC`, `CXX`, `CFLAGS`, `CXXFLAGS`, `LDFLAGS`, `PKG_CONFIG_PATH` always; extras via `--input-env`)
- Trailing `meson_args` verbatim

**Storage:** flat hand-rolled archive (`[u32 path_len][path][u64 content_len][content]` repeated) at `~/.cache/zccache/meson-configure/<key>/build-dir.tar` plus cached stdout/stderr sidecars.

**Same-build-dir restriction.** Build-dir-portable caching would require rewriting absolute paths meson scatters through `meson-info/` and `meson-private/` — see issue #627 open question 2. The current scope solves the common dev-loop case (stable build dir per developer) and the CI matrix case (each tuple converges to a per-tuple cache entry on its second invocation per platform).

**Failure modes:**
- Meson exit code != 0 → do NOT cache. A re-run after the fix gives meson a fresh chance.
- Cache restore I/O error → log warning, fall through to a fresh meson setup. Self-healing.

**Tests:** `crates/zccache/tests/cli_meson_configure_cache.rs` — TDD-pinned MISS → HIT → invalidate-on-meson.build-change.

**Future work (not in v1):**
- CMake / Bazel equivalents (same shape, different file set)
- `--reconfigure` interaction — currently we always serve a cache hit if the key matches; meson's own `--reconfigure` decision happens *after* our hit check by definition. If a user relies on meson's reconfigure heuristics outside the meson.build content (e.g. environment-only changes), they should add those vars to `--input-env`.
- Build-dir-portable mode via on-restore path rewriting

---

## Phase 6: Correctness Hardening and Benchmarks

**Goals:** Ensure the system is correct under stress, measure performance, and make the system observable and debuggable. This phase turns the MVP into production-quality software.

**Non-goals:** Exotic optimizations, distributed caching, or support for additional compilers.

**Deliverables:**

- Stress tests:
  - 100+ concurrent compilations with random file mutations
  - Rapid file changes during compilation (verify no stale artifacts returned)
  - Daemon restart during active compilations (verify graceful recovery)
  - Cache eviction under pressure (verify no data loss or corruption)
- Property-based tests (`proptest` or `quickcheck`):
  - Cache key determinism: same inputs always produce same key, regardless of insertion order
  - Metadata cache correctness: for any sequence of file operations and cache operations, the cache never returns stale data that leads to incorrect cache hits
  - Argument parser round-trip: parsed args reconstruct to equivalent command line
- Benchmarks (`criterion`):
  - Cache key computation latency (target: < 100 microseconds for typical invocation)
  - Metadata cache lookup latency (target: < 1 microsecond p99)
  - Artifact store write/read latency (target: < 5 ms for 1 MB artifact)
  - IPC round-trip latency (target: < 500 microseconds for ping/pong)
  - End-to-end cache-hit compilation (target: < 10 ms overhead vs. direct compiler invocation)
- Observability improvements:
  - `tracing` spans on all critical paths: IPC handling, cache lookup, artifact read/write, compiler execution, preprocessor execution
  - Cache hit/miss reasons in structured log fields
  - Timing breakdown in logs: total time, preprocessing time, hashing time, IPC time, artifact copy time
  - Daemon status endpoint: active connections, cache stats, watcher health
- CLI diagnostic commands:
  - `zccache inspect <key>`: show what is stored for a given cache key (manifest, artifact size, creation time, last access time)
  - `zccache explain -- gcc -c foo.c -o foo.o`: dry-run that shows cacheability decision, computed cache key, and whether it would be a hit or miss, without actually compiling
  - `zccache health`: check daemon status, cache integrity sample, watcher status
- Documentation:
  - Crate-level `//!` docs for every crate
  - `/// ` docs for all public types and functions
  - Usage guide: installation, configuration, basic usage, troubleshooting
  - Performance tuning guide: cache size, watcher configuration, confidence intervals

**Key tests:**

- All stress tests pass on all platforms
- All property tests pass with at least 10,000 cases
- All benchmarks run without regression vs. established baselines
- `zccache inspect` returns accurate data
- `zccache explain` output matches actual compilation behavior
- `zccache health` detects and reports known-broken states

**Risks:** Performance regressions as new features are added. **Mitigation:** Run `criterion` benchmarks in CI; track results over time; set regression thresholds that fail the build if exceeded (e.g., > 10% regression on any benchmark).

---

## Phase 7 (Future): Advanced Features

**Goals:** Post-MVP improvements that extend zccache to cover more use cases and improve performance. These are not committed to specific timelines.

**Non-goals:** Not committed to implementing all items. Each item will be evaluated independently based on user demand and engineering capacity.

**Possible deliverables:**

- **C++ support**: Handle C++ compilation, including template-heavy code and precompiled headers (PCH). Requires expanded argument parser and careful cache key computation for PCH dependencies.
- **MSVC (`cl.exe`) support**: Windows-native compiler with a very different flag syntax (`/c`, `/Fo`, `/I`, `/D`). Requires a separate argument parser module.
- **Header dependency tracking**: Use `-MD` / `-MMD` or compiler-generated `.d` dependency files to track which headers a source file includes. Enables smarter cache invalidation without running the full preprocessor on every build.
- **Artifact compression**: Compress cached artifacts with zstd before writing to disk. Reduces cache storage footprint by 50-70% for typical object files. Decompress on read.
- **Remote artifact storage**: Push/pull artifacts to/from remote backends (S3, GCS, Azure Blob). Enables cache sharing across machines.
- **Distributed cache sharing**: Multiple developers or CI machines share a cache. Requires content trust model, access control, and network-aware eviction.
- **Persistent metadata cache**: Serialize the file metadata cache to disk on daemon shutdown; reload on startup. Reduces cold-start stat overhead after daemon restart.
- **IDE integration**: Provide a language-server-compatible interface or editor plugin that shows cache status, hit rates, and diagnostics inline.
- **Build system plugins**: Native integration with CMake (custom compiler launcher), Meson (compiler wrapper config), and Bazel (remote cache protocol).
- **Configuration hot-reload**: Reload `zccache.toml` configuration without restarting the daemon. Useful for changing cache size limits, watch scopes, or log levels at runtime.
- **Metrics export**: Expose Prometheus-compatible metrics endpoint from the daemon for monitoring cache hit rates, latency distributions, cache size, and watcher health in production environments.

**Key tests:** Defined per feature as they are picked up.

**Risks:** Scope creep; each feature should be evaluated for complexity vs. user value before committing. **Mitigation:** Treat each item as an independent mini-project with its own design doc, deliverables, and acceptance criteria before implementation begins.
