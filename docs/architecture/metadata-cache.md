# File Metadata Cache & Watcher

The in-memory metadata cache and file watcher work together to track file changes efficiently. The watcher provides early warning; the cache provides fast lookups; stat verification provides ground truth.

For the correctness model see [runtime.md](runtime.md). For platform-specific watcher behavior see [portability.md](portability.md).

---

## Data Model

```rust
struct MetadataCache {
    entries: DashMap<CanonicalPath, MetadataEntry>,
}

struct CanonicalPath(PathBuf);  // always absolute, canonical

struct MetadataEntry {
    mtime: SystemTime,
    size: u64,
    file_id: Option<FileId>,   // inode on Unix, file index on Windows
    content_hash: Option<Blake3Hash>,
    confidence: Confidence,
    last_verified: Instant,    // monotonic clock
}

enum FileId {
    Unix { dev: u64, ino: u64 },
    Windows { volume_serial: u32, file_index: u64 },
}

enum Confidence {
    High,    // stat-verified or digest-verified within current session
    Medium,  // watcher reports no changes since last High
    Low,     // unverified: initial state, after watcher overflow, or stale
}
```

## Lookup and Update Flow

When the daemon needs a file's content hash:

1. **Look up** `CanonicalPath` in `DashMap`.
2. **If not found:** stat the file, hash it, insert entry at `High` confidence. Register the file's parent directory with the file watcher. Return hash.
3. **If found at High confidence:** `last_verified` is recent (within current compilation batch). Return cached `content_hash` if present, else hash and update.
4. **If found at Medium confidence:** Watcher says unchanged, but we must verify. Stat the file. Compare `(mtime, size, file_id)` with entry:
   - **Match:** Promote to `High`. Return cached `content_hash`. (No need to re-hash; the metadata match plus watcher-no-event gives high assurance.)
   - **Mismatch:** File changed. Re-hash, update entry at `High`.
5. **If found at Low confidence:** Stat the file. Compare metadata:
   - **Match:** Upgrade to `High` but still re-hash the file (we have low trust). Update entry.
   - **Mismatch:** Re-hash, update entry at `High`.

## Invalidation Strategy

Three layers, from fast to slow:

1. **File watcher** (fast, async): watcher event on a path → set entry to `Medium` (not `High`, because watcher events can coalesce or be delayed — we must stat to verify). If the event is a `Remove` or `Rename`, set to `Low`.
2. **Stat verification** (synchronous, per-lookup): compare `(mtime, size, file_id)`. Cheap kernel call. Catches changes the watcher might have missed or coalesced.
3. **Content hash** (expensive, on-demand): only re-computed when stat metadata changes or confidence is `Low`.

## Race Condition Analysis

**Race: file changes between stat and read-for-hashing.**
- After hashing, stat the file again. If `(mtime, size)` changed between the pre-hash stat and the post-hash stat, discard the hash and retry (up to 3 times). If still unstable, mark the file as uncacheable for this compilation.

**Race: watcher event arrives between stat-verify and confidence promotion.**
- The `DashMap` entry is updated via `entry.and_modify()`. The watcher also calls `entry.and_modify()`. Because DashMap operations on the same key are serialized (per-shard lock), exactly one wins. If the watcher downgrades confidence after we promoted it, the next lookup will re-verify — safe, just slightly wasteful.

**Race: file replaced by a different file with same mtime/size.**
- The `file_id` field (inode/file-index) detects file replacement. If `file_id` changes, the entry is treated as a cache miss regardless of `mtime`/`size` match. On platforms where `file_id` is unavailable (rare), we accept this as a known limitation; `mtime` granularity is the sole guard.

## Sharding Approach

`DashMap` internally shards by key hash. Default shard count is `num_cpus * 4`. No additional manual sharding is needed. The per-shard lock scope is small (hash-map bucket operations), so contention is low even under high concurrency.

---

## File Watcher Integration

### Watcher Setup

- Uses the `notify` crate with `RecommendedWatcher` (selects inotify on Linux, FSEvents on macOS, ReadDirectoryChangesW on Windows).
- The watcher runs on a **dedicated OS thread** (not a tokio task) to avoid blocking the async runtime with synchronous `notify` internals.
- Events are sent from the watcher thread to the daemon's tokio runtime via a `tokio::sync::mpsc::channel`.
- Fallback: if the native watcher fails to initialize (e.g., inotify watch limit exhausted), fall back to `notify::PollWatcher` with a 2-second interval.

### Degradation and Re-arm (issue #1156)

Arming can still fail outright — inotify `max_user_watches` exhaustion on a large tree is common and usually transient. A watcher-less daemon is a performance cliff, not a correctness problem: both fast hit tiers bail on `watcher_active == false`, and `verify_registered_blob` fully re-hashes every shared hardlink blob on every read. Left unhandled, one failed init at startup degrades the daemon for its entire lifetime.

`daemon/server/run.rs` therefore treats a failed arm as temporary:

- **Loud degradation.** `record_watcher_degradation` emits a `tracing::warn!`, writes a durable `watcher_degraded` lifecycle event, and bumps a monotonic counter surfaced as `Watcher:` in `zccache status` (`DaemonStatus::watcher_active` / `watcher_degradations`, protocol v21). Degradation is visible without trawling startup logs.
- **Bounded re-arm.** A background task retries `arm_watcher_pipeline` starting at 60 s with exponential backoff capped at 600 s, until it succeeds or the daemon shuts down. Success flips `watcher_active` / `WATCHER_AVAILABLE` back on, starts the settle + consumer tasks, and writes a `watcher_rearmed` event. The retry is cheap enough to be always-on, so no config knob is exposed.

The degradation counter is deliberately *not* reset on recovery: `active (recovered from N degradation(s))` in status is the post-hoc signal that the host is near its watch limit.

### Event Processing

A dedicated tokio task receives events from the channel and processes them:

```rust
async fn process_watcher_events(
    mut rx: mpsc::Receiver<notify::Event>,
    cache: Arc<MetadataCache>,
) {
    while let Some(event) = rx.recv().await {
        match event.kind {
            EventKind::Modify(_) | EventKind::Create(_) => {
                for path in &event.paths {
                    cache.set_confidence(path, Confidence::Medium);
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    cache.set_confidence(path, Confidence::Low);
                }
            }
            _ => {}
        }
    }
}
```

**Why Medium, not High, on modify events:** Watcher events may be batched, delayed, or refer to intermediate states. Only a stat call against the actual filesystem provides High confidence. Medium means "something happened, check before trusting."

### Overflow Handling

When the OS event queue overflows (inotify queue full, FSEvents `kFSEventStreamEventFlagMustScanSubDirs`), `notify` delivers a `Rescan` or error event. On overflow:

1. **Downgrade all entries** in the metadata cache to `Low` confidence.
2. **Log a warning** indicating watcher overflow.
3. **Re-register watches** if the watcher was in a recoverable state.
4. Subsequent lookups will stat-verify every file — expensive but correct.

**Scoped hardlink-registry recovery (issue #1156).** Overflow means "the event queue saturated", not "every file changed" — the usual triggers are a wide `git checkout` or a burst of parallel build output. Marking the whole hardlink registry suspect turns that momentary burst into a full blake3 re-hash of every shared blob on its next read. Instead, `mark_changed_registered_links_suspect` re-verifies each record against the cheap `(mtime, size)` signature captured when its `expected_hash` was computed:

- **Signature unchanged** → the record stays trusted and skips the re-hash. Writing through any hardlink alias mutates the shared inode, so an unchanged signature on the blob path is positive evidence no alias touched the bytes — the same signal the metadata cache already trusts at `Medium` confidence.
- **Signature changed, missing, or unstattable** → marked suspect, so the read path still falls back to the full re-hash and rejects/evicts a poisoned blob.

The signature is captured *before* hashing, never after: a stat-after would record the post-change value and could vouch for a torn read. Correctness is unchanged — the cheap check only ever *skips* work where there is evidence the bytes are intact, and never accepts bytes that failed a hash check. The sweep's `links_unchanged` / `links_suspect` split is recorded in a `watcher_overflow` lifecycle event.

### Scope Management

The watcher does not watch the entire filesystem. Watched directories:

- **Project root:** detected from the `cwd` of the first compilation request. Watched recursively.
- **Include directories:** discovered from `-I` flags in compilation requests. Watched non-recursively (only the specified directory, not subdirectories, unless the same directory is a project subdirectory — in which case the project root's recursive watch covers it).
- **System include directories** (e.g., `/usr/include`): NOT watched. These change rarely and watching them would be expensive and noisy. Files in unwatched directories always start at `Low` confidence and are stat-verified on every lookup.

Watch registrations are accumulated over the daemon's lifetime. Directories are never unwatched (the cost of maintaining a watch is negligible compared to the risk of missing an event).
