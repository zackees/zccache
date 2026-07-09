//! Compiler executable hash memoization.
//!
//! Caches `(mtime, size) -> ContentHash` for compiler binaries to skip
//! the per-request blake3 over multi-MB executables.
//!
//! ## On-disk persistence (issue #517)
//!
//! Hashing a 150 MB rustc binary on the cold path costs ~50-60 ms (Linux,
//! blake3 ~3 GB/s), dominating the `rust-workspace-link Cold` overhead
//! measured in `benchmark-stats/latest.json`. The cache is persisted to
//! disk alongside `metadata.bin` so a daemon restart (CI runner restart,
//! Stop hook tear-down, soldr-driven daemon recycle) does not refill it
//! from zero. The stored `(path, mtime, size, hash)` quad is exactly the
//! in-memory shape; correctness on load relies on the same stat-verify
//! that the in-memory `get_or_hash_with` already enforces — a (mtime, size)
//! mismatch silently downgrades the loaded entry to a re-hash, so a stale
//! snapshot cannot poison the cache key.

use super::*;
use serde::{Deserialize, Serialize};
use std::io::Write as _;

/// On-disk format version for the persisted compiler-hash cache.
///
/// Bump on any layout change to the `Persisted*` types so the loader
/// rejects older / newer snapshots instead of mis-decoding them.
pub(super) const FORMAT_VERSION: u32 = 1;

/// Env override (milliseconds) for the `<compiler> -vV` identity probe
/// timeout. See [`rustc_probe_timeout`].
const RUSTC_PROBE_TIMEOUT_ENV: &str = "ZCCACHE_RUSTC_PROBE_TIMEOUT_MS";

/// Default `<compiler> -vV` probe timeout (ms). The probe is a ~10 ms cold-path
/// optimization; no legitimate `-vV` runs anywhere near this. A generous bound
/// is fine here (unlike a compile/link, `-vV` is tiny and fixed-cost) and stops
/// a hung compiler wrapper (a soldr shim, a ccache-style front-end, a stuck
/// rustc wrapper) from blocking cache-key computation forever (issue #972).
const RUSTC_PROBE_TIMEOUT_DEFAULT_MS: u64 = 30_000;

/// Resolve the `-vV` probe timeout from the environment.
fn rustc_probe_timeout() -> std::time::Duration {
    std::env::var(RUSTC_PROBE_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_millis(
            RUSTC_PROBE_TIMEOUT_DEFAULT_MS,
        ))
}

/// Loud + durable diagnostics when a `-vV` probe times out (forensics rule):
/// the probe is abandoned and the caller falls back to the file-content hash,
/// so the cache key stays well-defined — but the stall is recorded.
fn warn_probe_timeout(path: &Path, timeout: std::time::Duration) {
    tracing::warn!(
        event = "rustc_identity_probe_timeout",
        compiler = %path.display(),
        timeout_ms = timeout.as_millis() as u64,
        "`<compiler> -vV` identity probe exceeded its timeout — the compiler may be \
         a wrapper that hangs; abandoning the probe and falling back to hashing the \
         binary so cache-key computation is not blocked (issue #972)"
    );
    crate::core::lifecycle::write_event(
        "rustc_identity_probe_timeout",
        serde_json::json!({
            "compiler": path.display().to_string(),
            "timeout_ms": timeout.as_millis() as u64,
            "reason": "-vV probe timed out; fell back to file-content hash",
        }),
    );
}

/// Outcome of a bounded `-vV` probe — distinguishes a genuine timeout (log it)
/// from a spawn failure (expected for stub binaries in unit tests; don't log).
enum ProbeOutcome {
    Completed(std::process::Output),
    TimedOut,
    SpawnFailed,
}

/// Spawn `cmd` and wait up to `timeout`, killing the child on timeout. Used to
/// bound the sync `-vV` probe. `-vV` output is tiny (well under any pipe
/// buffer), so polling `try_wait` cannot deadlock on an undrained pipe.
fn output_within(mut cmd: std::process::Command, timeout: std::time::Duration) -> ProbeOutcome {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return ProbeOutcome::SpawnFailed,
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return match child.wait_with_output() {
                    Ok(output) => ProbeOutcome::Completed(output),
                    Err(_) => ProbeOutcome::SpawnFailed,
                };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ProbeOutcome::TimedOut;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return ProbeOutcome::SpawnFailed,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct CompilerHashEntry {
    pub(super) mtime: std::time::SystemTime,
    pub(super) size: u64,
    pub(super) hash: ContentHash,
}

#[derive(Serialize, Deserialize)]
struct PersistedCompilerHashes {
    version: u32,
    entries: Vec<(NormalizedPath, CompilerHashEntry)>,
}

#[derive(Default)]
pub(super) struct CompilerHashCache {
    pub(super) entries: DashMap<NormalizedPath, CompilerHashEntry>,
}

impl CompilerHashCache {
    pub(super) fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drain entries from a freshly loaded `CompilerHashCache` into `self`
    /// using `DashMap::insert` (which is `&self`).
    ///
    /// Issue #784: lets a background `spawn_blocking` task load the on-disk
    /// snapshot AFTER the daemon has written its readiness lockfile, then
    /// populate the live cache without holding up bind. Readers during the
    /// merge window either see no entry (cold-path miss — safe; the next
    /// call to `get_or_hash_with` re-hashes) or a loaded entry (stat-verify
    /// at the call site rejects stale (mtime, size) before trusting the
    /// hash, so a partially-loaded snapshot cannot poison cache keys).
    pub(super) fn merge_from(&self, other: Self) {
        for (k, v) in other.entries {
            self.entries.insert(k, v);
        }
    }

    pub(super) fn get_or_hash_with<F>(&self, path: &Path, hasher: F) -> Option<ContentHash>
    where
        F: FnOnce(&Path) -> Option<ContentHash>,
    {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        let key = NormalizedPath::new(path);

        if let Some(entry) = self.entries.get(&key) {
            if entry.mtime == mtime && entry.size == size {
                return Some(entry.hash);
            }
        }

        let hash = hasher(path)?;
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries
            .insert(key, CompilerHashEntry { mtime, size, hash });
        Some(hash)
    }

    pub(super) async fn get_or_hash_with_async<F, Fut>(
        &self,
        path: &Path,
        hasher: F,
    ) -> Option<ContentHash>
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = Option<ContentHash>>,
    {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        let key = NormalizedPath::new(path);

        if let Some(entry) = self.entries.get(&key) {
            if entry.mtime == mtime && entry.size == size {
                return Some(entry.hash);
            }
        }

        let hash = hasher(path.to_path_buf()).await?;
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries
            .insert(key, CompilerHashEntry { mtime, size, hash });
        Some(hash)
    }

    /// Persist the cache to `path` as a versioned bincode snapshot.
    ///
    /// Atomic on success: writes to `<path>.tmp-<pid>`, then renames over
    /// `path`. Empty snapshots short-circuit without touching disk. Stale
    /// entries on disk are harmless: `get_or_hash_with` re-stats every key
    /// before trusting the hash, so a mismatch silently downgrades to a
    /// re-hash. See module-level doc.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from `create_dir_all`, `write`, `rename`, or
    /// bincode serialization.
    pub(super) fn save_to_disk(&self, path: &Path) -> std::io::Result<()> {
        let entries: Vec<(NormalizedPath, CompilerHashEntry)> = self
            .entries
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();

        if entries.is_empty() {
            tracing::debug!(
                path = %path.display(),
                "compiler hash cache flush: 0 entries, skipping write"
            );
            return Ok(());
        }

        let entry_count = entries.len();
        let snapshot = PersistedCompilerHashes {
            version: FORMAT_VERSION,
            entries,
        };
        let bytes = bincode::serialize(&snapshot)
            .map_err(|e| std::io::Error::other(format!("bincode serialize: {e}")))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "compiler_hash.bin".into());
        let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));

        let result = write_atomic_durable(&tmp, path, &bytes);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        if result.is_ok() {
            tracing::info!(
                path = %path.display(),
                entries = entry_count,
                bytes = bytes.len(),
                "compiler hash cache flushed to disk"
            );
        }
        result
    }

    /// Load a previously persisted snapshot from `path`.
    ///
    /// Returns an empty cache when the file is absent (first run). Any
    /// other I/O error, bincode decode failure, or version mismatch is
    /// surfaced as `Err`; the daemon caller is expected to log and start
    /// empty. Stat-verification at the `get_or_hash_with` call site re-checks
    /// every loaded entry before use, so a stale on-disk snapshot cannot
    /// produce an incorrect cache key.
    ///
    /// # Errors
    ///
    /// Any I/O error other than `NotFound`, any bincode decode failure,
    /// or any version mismatch.
    pub(super) fn load_from_disk(path: &Path) -> std::io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %path.display(),
                    "compiler hash cache file not found, starting empty"
                );
                return Ok(Self::new());
            }
            Err(e) => return Err(e),
        };

        let snapshot: PersistedCompilerHashes = bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::other(format!("bincode deserialize: {e}")))?;
        if snapshot.version != FORMAT_VERSION {
            return Err(std::io::Error::other(format!(
                "compiler hash cache version mismatch: file={}, expected={}",
                snapshot.version, FORMAT_VERSION
            )));
        }

        let entries = DashMap::with_capacity(snapshot.entries.len());
        let entry_count = snapshot.entries.len();
        for (key, value) in snapshot.entries {
            entries.insert(key, value);
        }
        tracing::info!(
            path = %path.display(),
            entries = entry_count,
            "compiler hash cache restored from disk"
        );
        Ok(Self { entries })
    }
}

fn write_atomic_durable(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, target)?;
    if let Some(parent) = target.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Compute a content hash that uniquely identifies a rustc /
/// clippy-driver / rustfmt build, preferring `<compiler> -vV` output
/// over a full blake3 over the binary. `-vV` prints the toolchain
/// version + commit hash + LLVM version + host triple — all the bits
/// the cache key must vary on — and runs in ~10 ms vs ~50-60 ms for
/// the ~150 MB binary blake3 (issue #517).
///
/// Falls back to the file-content hash on spawn failure, non-zero
/// exit, or empty stdout so cache keys are still well-defined for
/// stubbed binaries (unit tests) or broken toolchains.
pub(super) fn hash_rustc_identity(path: &Path) -> Option<ContentHash> {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("-vV");
    // Suppress the Windows console window this cold-path probe would
    // otherwise flash — the daemon runs detached, so a console-subsystem
    // child spawned without CREATE_NO_WINDOW pops a visible window.
    crate::daemon::process::suppress_child_console(&mut cmd);
    let timeout = rustc_probe_timeout();
    match output_within(cmd, timeout) {
        ProbeOutcome::Completed(output) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        // A hung wrapper compiler: log it, then fall through to the
        // file-content hash so cache-key computation is never blocked (#972).
        ProbeOutcome::TimedOut => {
            warn_probe_timeout(path, timeout);
            crate::hash::hash_file(path).ok()
        }
        // Spawn failure (stub binaries in unit tests), non-zero exit, or empty
        // stdout — fall through to the file-content hash so keys stay
        // well-defined. Not logged: these are expected, not stalls.
        _ => crate::hash::hash_file(path).ok(),
    }
}

pub(super) async fn hash_rustc_identity_async(path: std::path::PathBuf) -> Option<ContentHash> {
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("-vV");
    // Same CREATE_NO_WINDOW suppression as the sync variant above.
    crate::daemon::process::suppress_child_console_tokio(&mut cmd);
    // Reap the child if we abandon the probe on timeout (issue #972) so a hung
    // wrapper compiler is not left running.
    cmd.kill_on_drop(true);
    let timeout = rustc_probe_timeout();
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        // Timeout: a wrapper compiler that hung. Log it (the `cmd.output()`
        // future is dropped → kill_on_drop reaps the child), then fall through
        // to the file-content hash so cache-key computation is not blocked.
        Err(_) => {
            warn_probe_timeout(&path, timeout);
            crate::hash::hash_file(&path).ok()
        }
        // Spawn error, non-zero exit, or empty stdout — fall through to the
        // file-content hash so cache keys stay well-defined for stubbed
        // binaries (unit tests) and broken toolchains.
        _ => crate::hash::hash_file(&path).ok(),
    }
}

#[cfg(test)]
mod probe_timeout_tests {
    //! Issue #972: the `<compiler> -vV` identity probe must be bounded so a
    //! hung wrapper compiler cannot block cache-key computation.
    use super::{output_within, ProbeOutcome};
    use std::time::Duration;

    fn slow_cmd() -> std::process::Command {
        #[cfg(windows)]
        {
            let mut c = std::process::Command::new("cmd");
            // ~30 s: 31 pings ~1 s apart.
            c.args(["/c", "ping -n 31 127.0.0.1 >nul"]);
            c
        }
        #[cfg(unix)]
        {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "sleep 30"]);
            c
        }
    }

    fn fast_cmd() -> std::process::Command {
        #[cfg(windows)]
        {
            let mut c = std::process::Command::new("cmd");
            c.args(["/c", "echo hi"]);
            c
        }
        #[cfg(unix)]
        {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "echo hi"]);
            c
        }
    }

    #[test]
    fn times_out_on_hung_compiler() {
        // A probe that would run ~30 s is abandoned in ~200 ms.
        let start = std::time::Instant::now();
        let outcome = output_within(slow_cmd(), Duration::from_millis(200));
        assert!(
            matches!(outcome, ProbeOutcome::TimedOut),
            "a slow probe must time out"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timeout did not bound the wait (took {:?})",
            start.elapsed()
        );
    }

    #[test]
    fn completes_fast_command() {
        match output_within(fast_cmd(), Duration::from_secs(30)) {
            ProbeOutcome::Completed(output) => assert!(output.status.success()),
            other => panic!(
                "expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn spawn_failed_for_missing_binary() {
        let cmd = std::process::Command::new("zzz-nonexistent-compiler-xyz-972");
        assert!(matches!(
            output_within(cmd, Duration::from_secs(5)),
            ProbeOutcome::SpawnFailed
        ));
    }
}
