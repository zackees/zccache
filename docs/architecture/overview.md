# System Overview & Components

This document describes the high-level architecture and component responsibilities of zccache.

For related topics see: [data-flow.md](data-flow.md), [ipc.md](ipc.md), [metadata-cache.md](metadata-cache.md), [artifact-store.md](artifact-store.md), [runtime.md](runtime.md), [portability.md](portability.md).

---

## 1. System Overview

zccache intercepts C/C++ compiler invocations, computes a deterministic cache key from the compiler, flags, environment, and source content, and returns cached artifacts on a hit. It runs as a per-user background daemon that persists across compilations, maintaining an in-memory file metadata cache and a disk-backed artifact store.

### Component Diagram

```
+---------------------------+       +-------------------------------------------+
|          CLI              |       |               Daemon                      |
|                           |  IPC  |                                           |
|  zccache-cc / zccache-c++ |<----->|  +-------------+    +-----------------+  |
|  zccache wrap -- gcc ...  |       |  | IPC Server  |    | Compiler Manager|  |
|                           |       |  +------+------+    +--------+--------+  |
+---------------------------+       |         |                    |            |
                                    |         v                    v            |
                                    |  +------+------+    +-------+--------+   |
                                    |  |  Metadata   |    | Hashing Engine |   |
                                    |  |   Cache     |    +-------+--------+   |
                                    |  |  (DashMap)  |            |            |
                                    |  +------+------+    +-------+--------+   |
                                    |         |           | Artifact Store |   |
                                    |         v           | (disk + bincode)|   |
                                    |  +------+------+    +-------+--------+   |
                                    |  | File Watcher|            |            |
                                    |  | (notify)    |            |            |
                                    |  +-------------+            |            |
                                    +-----------------------------+------------+
                                              |                   |
                                    +---------v-------------------v-----------+
                                    |       Filesystem / Compilers            |
                                    +-----------------------------------------+
```

**CLI** — thin client that discovers the daemon, connects over IPC, sends a compilation request, and relays the result. If the daemon is not running, the CLI starts it.

**Daemon** — long-lived per-user process. Accepts IPC connections, manages the metadata cache, artifact store, file watcher, and compiler subprocess execution.

**Filesystem / Compilers** — external. The daemon reads source files, invokes compilers, and stores/retrieves artifacts on disk.

---

## 2. Component Descriptions

### 2.1 CLI

**Responsibility:** Entry point for the user. Parses the compiler command line, determines if the invocation is cacheable, and communicates with the daemon.

**Key interfaces:**
- `main()` — entry point. Parses argv to determine mode (wrapper or explicit `wrap` subcommand).
- `DaemonConnector` — locates the daemon socket, connects, starts the daemon if needed.

**Internal structure:**
- Compiler argument parsing: extracts compiler path, source files, output file, flags.
- Cacheability check: rejects linking, preprocessing-only (`-E`), multiple source files, and other non-cacheable invocations before contacting the daemon.
- A small tokio runtime (single-threaded) is created solely for the IPC exchange; the rest of the CLI is synchronous.
- If the invocation is non-cacheable, the CLI execs the compiler directly without contacting the daemon.

**Binary names:** `zccache-cc`, `zccache-c++`, `zccache-gcc`, `zccache-g++`, `zccache-clang`, `zccache-clang++`. Also invokable as `zccache wrap -- <compiler> <args...>`.

**Top-level flags (sccache-compatible):** `--clear` (wipe cache), `--show-stats` (print status).

**Session commands:** `session-start [--stats] [--log FILE]`, `session-stats <id>` (mid-build query), `session-end <id>` (finalize).

### 2.2 Daemon Core

**Responsibility:** Lifecycle management. Starts, shuts down, handles signals.

**Key interfaces:**
- `Daemon::start()` — acquires lock file, binds IPC socket, spawns subsystems.
- `Daemon::shutdown()` — drains in-flight requests, cleans up temp dirs, closes watcher.

**Internal structure:**
- Creates and owns all subsystem handles (IPC server, metadata cache, artifact store, file watcher, compiler manager).
- Runs the tokio multi-threaded runtime.
- Signal handling: SIGTERM/SIGINT trigger graceful shutdown. On Windows, uses `SetConsoleCtrlHandler`.

### 2.3 IPC Transport

**Responsibility:** Platform-abstracted bidirectional byte-stream transport between CLI and daemon.

**Key interfaces (`zccache-platform::ipc`):**
```rust
let endpoint = Endpoint::select(unix_path, windows_pipe_name);
let mut listener = Listener::bind(&endpoint)?;
let stream: Stream = connect(&endpoint).await?;
let (stream, peer): (Stream, PeerIdentity) = listener.accept().await?;
```

**Internal structure:**
- `zccache-platform` selects private Linux, macOS, or Windows concrete leaves once.
- Linux/macOS leaves wrap Tokio Unix listeners/streams and kernel peer credentials.
- Windows leaves wrap Tokio named pipes, owner-only DACLs, accept pooling, and busy backoff.
- `zccache-ipc` layers framing, product naming, timeouts, and diagnostics over the facade.

### 2.4 Protocol

**Responsibility:** Serialization and framing of messages over the transport.

**Message format:** Length-prefixed bincode. Each message is:
```
[u32 little-endian: payload length][payload: bincode-serialized Message]
```

**Message types (simplified):**
```rust
enum Request {
    Ping,
    Shutdown,
    Status,
    Clear,
    SessionStart { client_pid, working_dir, log_file, track_stats },
    Compile { session_id, args, cwd, compiler, env },
    SessionEnd { session_id },
    SessionStats { session_id },           // query mid-session stats (non-destructive)
    CompileEphemeral { client_pid, working_dir, compiler, args, cwd, env },
    LinkEphemeral { client_pid, tool, args, cwd, env },
}

enum Response {
    Pong,
    ShuttingDown,
    Status(DaemonStatus),
    Cleared { artifacts_removed, metadata_cleared, ... },
    SessionStarted { session_id },
    CompileResult { exit_code, stdout, stderr, cached },
    SessionEnded { stats: Option<SessionStats> },
    SessionStatsResult { stats: Option<SessionStats> },  // mid-session snapshot
    LinkResult { exit_code, stdout, stderr, cached, warning },
    Error { message },
}
```

### 2.5 File Metadata Cache

**Responsibility:** Track file metadata in memory to avoid redundant `stat` and hash calls. Provide fast answers to "has this file changed since we last saw it?"

**Key interfaces:**
```rust
struct MetadataCache {
    entries: DashMap<CanonicalPath, MetadataEntry>,
}

struct MetadataEntry {
    mtime: SystemTime,
    size: u64,
    file_id: Option<FileId>,
    content_hash: Option<Blake3Hash>,
    confidence: Confidence,
    last_verified: Instant,
}

enum Confidence {
    High,    // recently stat-verified or digest-verified
    Medium,  // watcher says unchanged
    Low,     // stale, unverified, or after watcher overflow
}
```

**Internal structure:** See [metadata-cache.md](metadata-cache.md) for full details.

### 2.6 File Watcher

**Responsibility:** Monitor source and header directories for changes and feed events into the metadata cache.

**Key interfaces:**
```rust
struct FileWatcher {
    watcher: notify::RecommendedWatcher,
    tx: tokio::sync::mpsc::Sender<WatcherEvent>,
}
```

**Internal structure:** See [metadata-cache.md](metadata-cache.md) for full details.

### 2.7 Artifact Store

**Responsibility:** Persist and retrieve compiled output files, keyed by content-addressed hash.

**Key interfaces:**
```rust
struct ArtifactStore {
    root: PathBuf,
    index: DashMap<String, ArtifactIndex>,
    max_size: u64,
}

impl ArtifactStore {
    async fn lookup(&self, key: &Blake3Hash) -> Option<Artifact>;
    async fn store(&self, key: &Blake3Hash, artifact: Artifact) -> Result<()>;
    async fn evict_to_size(&self, target_size: u64) -> Result<()>;
}
```

**Internal structure:** See [artifact-store.md](artifact-store.md) for full details.

### 2.8 Hashing Engine

**Responsibility:** Compute deterministic cache keys and content digests using blake3.

**Key interfaces:**
```rust
fn compute_cache_key(
    compiler_id: &Blake3Hash,
    args: &[String],
    env: &[(String, String)],
    source_hash: &Blake3Hash,
    dep_hash: &Blake3Hash,
) -> Blake3Hash;

fn hash_file(path: &Path) -> Result<Blake3Hash>;
fn hash_compiler(path: &Path) -> Result<Blake3Hash>;
```

**Why blake3:** Fast (parallelizable, SIMD-optimized), cryptographic-strength collision resistance, single hash function for all purposes. The `blake3` crate is used directly.

**Cache key construction:**
1. Hash the compiler binary (or use a cached identity hash).
2. Sort and filter CLI args to only cache-relevant flags (strip `-o`, strip debug-only flags, normalize paths).
3. Sort and filter environment variables to only those that affect compilation output (e.g., `CPATH`, `C_INCLUDE_PATH`, `SDKROOT`).
4. Hash the source file content (or use cached digest at High confidence).
5. Hash dependency content. In MVP, this is the hash of the preprocessor output (`-E`). Future versions may use `-MD` dependency files to hash individual headers.
6. Concatenate all of the above into a blake3 hasher in a defined order and finalize.

### 2.9 Compiler Manager

**Responsibility:** Spawn compiler subprocesses, capture output, manage concurrency.

**Key interfaces:**
```rust
struct CompilerManager;

impl CompilerManager {
    async fn run_compiler(
        &self,
        compiler: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<CompilerOutput>;
}

struct CompilerOutput {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_files: Vec<PathBuf>,
}
```

**Internal structure:**
- Uses `tokio::process::Command` to spawn compiler processes.
- Captures stdout and stderr.
- Multiple compilations run concurrently as independent tokio tasks. No artificial concurrency limit in the daemon; the OS scheduler and build system manage parallelism.
