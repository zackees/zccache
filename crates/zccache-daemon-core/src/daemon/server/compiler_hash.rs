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
pub(super) const FORMAT_VERSION: u32 = 2;

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
        crate::core::lifecycle::EVENT_RUSTC_IDENTITY_PROBE_TIMEOUT,
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
    Completed(ProbeOutput),
    TimedOut,
    SpawnFailed,
}

struct ProbeOutput {
    success: bool,
    stdout: Vec<u8>,
}

/// Spawn `cmd` and wait up to `timeout`, killing the child on timeout. Used to
/// bound the sync `-vV` probe. Capture, descendant containment, reader
/// cancellation, and the aggregate byte limit all belong to running-process.
fn output_within(cmd: std::process::Command, timeout: std::time::Duration) -> ProbeOutcome {
    const PROBE_OUTPUT_LIMIT: usize = 1024 * 1024;

    // zccache#1562: running-process owns spawn *and* wait here, so the shared
    // materialization/spawn guard has to bracket the whole bounded run rather
    // than the spawn alone. A shared guard only blocks materialization (the
    // exclusive side), never other spawns; this probe is a cold path taken
    // once per compiler identity, normally ~10 ms, and bounded by `timeout`.
    let _spawn_guard = crate::daemon::spawn_exclusion::spawn_shared();
    match running_process::run_std_command_bounded(cmd, Some(timeout), PROBE_OUTPUT_LIMIT) {
        Ok(output) => ProbeOutcome::Completed(ProbeOutput {
            success: output.exit_code == 0,
            stdout: output.stdout,
        }),
        Err(running_process::ProcessError::Timeout) => ProbeOutcome::TimedOut,
        Err(_) => ProbeOutcome::SpawnFailed,
    }
}

async fn output_within_async(
    cmd: &mut tokio::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    crate::daemon::process::tokio_command_output_with_priority_timeout(
        cmd,
        crate::daemon::process::CompilePriority::Normal,
        timeout,
    )
    .await
}

/// The provenance of a cached compiler identity.
///
/// A rustc `-vV` identity is the only rustc flavor that may survive a
/// daemon restart. File-content fallbacks are intentionally ephemeral: a
/// transient probe failure must not pin an otherwise healthy toolchain into
/// a second cache-key space (#1167).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum CompilerIdentityFlavor {
    Generic,
    RustcVv,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct CompilerHashEntry {
    pub(super) mtime: std::time::SystemTime,
    pub(super) size: u64,
    pub(super) hash: ContentHash,
    pub(super) flavor: CompilerIdentityFlavor,
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

        let previous = self.entries.get(&key).map(|entry| entry.clone());
        if let Some(entry) = &previous {
            if entry.mtime == mtime && entry.size == size {
                return Some(entry.hash);
            }
        }

        let hash = hasher(path)?;
        if previous.is_some_and(|entry| entry.hash != hash) {
            crate::daemon::compile_journal::record_miss_reason(
                crate::daemon::compile_journal::miss_reason::VERSION_SKEW,
            );
        }
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries.insert(
            key,
            CompilerHashEntry {
                mtime,
                size,
                hash,
                flavor: CompilerIdentityFlavor::Generic,
            },
        );
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

        let previous = self.entries.get(&key).map(|entry| entry.clone());
        if let Some(entry) = &previous {
            if entry.mtime == mtime && entry.size == size {
                return Some(entry.hash);
            }
        }

        let hash = hasher(path.to_path_buf()).await?;
        if previous.is_some_and(|entry| entry.hash != hash) {
            crate::daemon::compile_journal::record_miss_reason(
                crate::daemon::compile_journal::miss_reason::VERSION_SKEW,
            );
        }
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries.insert(
            key,
            CompilerHashEntry {
                mtime,
                size,
                hash,
                flavor: CompilerIdentityFlavor::Generic,
            },
        );
        Some(hash)
    }

    /// Resolve a rustc identity while only caching a confirmed `-vV` result.
    ///
    /// A file hash is safe for the request that observed a failed or timed-out
    /// probe, but caching it would make that transient outcome a durable cache
    /// split. The persisted `RustcVv` flavor is therefore also a preference:
    /// once a binary has produced a valid `-vV` result, restarts reuse that
    /// result; a fallback is always retried on the next request.
    pub(super) fn get_or_hash_rustc_with<F>(&self, path: &Path, hasher: F) -> Option<ContentHash>
    where
        F: FnOnce(&Path) -> Option<RustcIdentity>,
    {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        let key = NormalizedPath::new(path);

        let previous = self.entries.get(&key).map(|entry| entry.clone());
        if let Some(entry) = &previous {
            if entry.mtime == mtime
                && entry.size == size
                && entry.flavor == CompilerIdentityFlavor::RustcVv
            {
                return Some(entry.hash);
            }
        }

        let identity = hasher(path)?;
        let hash = identity.hash();
        if previous.is_some_and(|entry| {
            entry.flavor == CompilerIdentityFlavor::RustcVv && entry.hash != hash
        }) {
            crate::daemon::compile_journal::record_miss_reason(
                crate::daemon::compile_journal::miss_reason::VERSION_SKEW,
            );
        }
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries.insert(
            key,
            CompilerHashEntry {
                mtime,
                size,
                hash,
                flavor: if identity.is_verified_vv() {
                    CompilerIdentityFlavor::RustcVv
                } else {
                    // Keep the fallback value stable for a broken/stub
                    // compiler and across a restart, but never fast-hit it:
                    // the `RustcVv`-only early return above retries `-vV`
                    // on every request and upgrades this marker on success.
                    CompilerIdentityFlavor::Generic
                },
            },
        );
        Some(hash)
    }

    pub(super) fn get_or_hash_rustc_identity(&self, path: &Path) -> Option<ContentHash> {
        self.get_or_hash_rustc_with(path, rustc_identity)
    }

    pub(super) async fn get_or_hash_rustc_identity_async(
        &self,
        path: &Path,
    ) -> Option<ContentHash> {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        let key = NormalizedPath::new(path);

        let previous = self.entries.get(&key).map(|entry| entry.clone());
        if let Some(entry) = &previous {
            if entry.mtime == mtime
                && entry.size == size
                && entry.flavor == CompilerIdentityFlavor::RustcVv
            {
                return Some(entry.hash);
            }
        }

        let identity = rustc_identity_async(path.to_path_buf()).await?;
        let hash = identity.hash();
        if previous.is_some_and(|entry| {
            entry.flavor == CompilerIdentityFlavor::RustcVv && entry.hash != hash
        }) {
            crate::daemon::compile_journal::record_miss_reason(
                crate::daemon::compile_journal::miss_reason::VERSION_SKEW,
            );
        }
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries.insert(
            key,
            CompilerHashEntry {
                mtime,
                size,
                hash,
                flavor: if identity.is_verified_vv() {
                    CompilerIdentityFlavor::RustcVv
                } else {
                    CompilerIdentityFlavor::Generic
                },
            },
        );
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

/// A rustc identity together with whether it was proven by `-vV`.
pub(super) enum RustcIdentity {
    VerifiedVv(ContentHash),
    FileFallback(ContentHash),
}

impl RustcIdentity {
    fn hash(&self) -> ContentHash {
        match self {
            Self::VerifiedVv(hash) | Self::FileFallback(hash) => *hash,
        }
    }

    fn is_verified_vv(&self) -> bool {
        matches!(self, Self::VerifiedVv(_))
    }
}

fn warn_rustc_identity_fallback(path: &Path, reason: &'static str) {
    tracing::warn!(
        event = "compiler_identity_fallback_flavor",
        compiler = %path.display(),
        compiler_family = "rustc",
        reason,
        "rustc identity probe did not yield `-vV`; using an uncached file-hash fallback for this request"
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_COMPILER_IDENTITY_FALLBACK_FLAVOR,
        serde_json::json!({
            "compiler": path.display().to_string(),
            "compiler_family": "rustc",
            "reason": reason,
        }),
    );
}

fn rustc_identity(path: &Path) -> Option<RustcIdentity> {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("-vV");
    let timeout = rustc_probe_timeout();
    match output_within(cmd, timeout) {
        ProbeOutcome::Completed(output) if output.success && !output.stdout.is_empty() => Some(
            RustcIdentity::VerifiedVv(crate::hash::hash_bytes(&output.stdout)),
        ),
        ProbeOutcome::TimedOut => {
            warn_probe_timeout(path, timeout);
            warn_rustc_identity_fallback(path, "probe_timeout");
            crate::hash::hash_file(path)
                .ok()
                .map(RustcIdentity::FileFallback)
        }
        ProbeOutcome::SpawnFailed => {
            warn_rustc_identity_fallback(path, "probe_spawn_failed");
            crate::hash::hash_file(path)
                .ok()
                .map(RustcIdentity::FileFallback)
        }
        ProbeOutcome::Completed(_) => {
            warn_rustc_identity_fallback(path, "probe_degenerate_output");
            crate::hash::hash_file(path)
                .ok()
                .map(RustcIdentity::FileFallback)
        }
    }
}

async fn rustc_identity_async(path: std::path::PathBuf) -> Option<RustcIdentity> {
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("-vV");
    let timeout = rustc_probe_timeout();
    match output_within_async(&mut cmd, timeout).await {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => Some(
            RustcIdentity::VerifiedVv(crate::hash::hash_bytes(&output.stdout)),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            warn_probe_timeout(&path, timeout);
            warn_rustc_identity_fallback(&path, "probe_timeout");
            crate::hash::hash_file(&path)
                .ok()
                .map(RustcIdentity::FileFallback)
        }
        Err(_) => {
            warn_rustc_identity_fallback(&path, "probe_spawn_failed");
            crate::hash::hash_file(&path)
                .ok()
                .map(RustcIdentity::FileFallback)
        }
        Ok(_) => {
            warn_rustc_identity_fallback(&path, "probe_degenerate_output");
            crate::hash::hash_file(&path)
                .ok()
                .map(RustcIdentity::FileFallback)
        }
    }
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
#[allow(dead_code)] // Direct helper retained for the identity-output unit test.
pub(super) fn hash_rustc_identity(path: &Path) -> Option<ContentHash> {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("-vV");
    // The running-process boundary makes this cold-path probe consoleless.
    let timeout = rustc_probe_timeout();
    match output_within(cmd, timeout) {
        ProbeOutcome::Completed(output) if output.success && !output.stdout.is_empty() => {
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

/// Compute a content hash that uniquely identifies a C/C++ compiler build
/// (clang, gcc, MSVC `cl.exe`), mirroring [`hash_rustc_identity`] for issue
/// #1166: the C/C++ compile cache key did not previously vary on compiler
/// binary identity, so an in-place toolchain upgrade (same path, new
/// binary content) could serve stale object files from cache.
///
/// Prefers `<compiler> --version` output over a full blake3 over the
/// binary, for the same cold-path cost reasons as the rustc `-vV` probe
/// (issue #517). Falls back to the file-content hash on spawn failure,
/// non-zero exit, or empty stdout so cache keys are still well-defined for
/// stubbed binaries (unit tests) and broken toolchains, or for compilers
/// (e.g. some `cl.exe` invocations) that only print version info to
/// stderr — see the TODO below.
pub(super) fn hash_cc_identity(path: &Path) -> Option<ContentHash> {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("--version");
    let timeout = rustc_probe_timeout();
    match output_within(cmd, timeout) {
        ProbeOutcome::Completed(output) if output.success && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        ProbeOutcome::TimedOut => {
            warn_probe_timeout(path, timeout);
            crate::hash::hash_file(path).ok()
        }
        // Spawn failure (stub binaries in unit tests), non-zero exit, or
        // empty stdout (e.g. MSVC `cl.exe`, which prints its banner to
        // stderr and errors without input) — fall through to the
        // file-content hash so keys stay well-defined.
        //
        // TODO(#1167): this degenerate-probe fallback reuses
        // `hash_rustc_identity`'s existing shape as-is. Issue #1167 will
        // define stricter fallback/degenerate-result policy (e.g.
        // distinguishing "compiler present but --version unsupported"
        // from "compiler missing entirely"); do not add new policy here.
        _ => crate::hash::hash_file(path).ok(),
    }
}

/// Async sibling of [`hash_cc_identity`]. See that function's doc comment
/// for the rationale (issue #1166).
pub(super) async fn hash_cc_identity_async(path: std::path::PathBuf) -> Option<ContentHash> {
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("--version");
    let timeout = rustc_probe_timeout();
    match output_within_async(&mut cmd, timeout).await {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            warn_probe_timeout(&path, timeout);
            crate::hash::hash_file(&path).ok()
        }
        // TODO(#1167): see the sync `hash_cc_identity` fallback arm above.
        _ => crate::hash::hash_file(&path).ok(),
    }
}

/// Identify a generated Dylint driver by its semantic version rather than its
/// byte-for-byte executable image.
///
/// Cargo Dylint builds the driver in a fresh temporary package. Equivalent
/// rebuilds can therefore differ in debug paths or linker build IDs even
/// though `dylint-driver -V` and behavior are unchanged. Hashing the complete
/// image prevents cache reuse across those rebuilds. The inner rustc identity
/// and every loaded lint-library content hash are independently included in
/// the Dylint cache-input fingerprint.
pub(super) async fn hash_dylint_driver_identity_async(
    path: std::path::PathBuf,
    client_env: Vec<(String, String)>,
) -> Option<ContentHash> {
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("-V").envs(client_env);
    let timeout = rustc_probe_timeout();
    match output_within_async(&mut cmd, timeout).await {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            warn_probe_timeout(&path, timeout);
            crate::hash::hash_file(&path).ok()
        }
        _ => crate::hash::hash_file(&path).ok(),
    }
}

#[allow(dead_code)] // The cache calls `rustc_identity_async` to retain provenance.
pub(super) async fn hash_rustc_identity_async(path: std::path::PathBuf) -> Option<ContentHash> {
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("-vV");
    let timeout = rustc_probe_timeout();
    match output_within_async(&mut cmd, timeout).await {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
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
        if crate::platform::host::is_windows() {
            let mut c = std::process::Command::new("cmd");
            // ~30 s: 31 pings ~1 s apart.
            c.args(["/c", "ping -n 31 127.0.0.1 >nul"]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "sleep 30"]);
            c
        }
    }

    fn fast_cmd() -> std::process::Command {
        if crate::platform::host::is_windows() {
            let mut c = std::process::Command::new("cmd");
            c.args(["/c", "echo hi"]);
            c
        } else {
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
            ProbeOutcome::Completed(output) => assert!(output.success),
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

    #[test]
    fn timeout_is_not_held_open_by_escaped_descendant_pipes() {
        if !crate::platform::host::is_linux() {
            return;
        }
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "setsid sh -c 'sleep 3' & sleep 30"]);

        let start = std::time::Instant::now();
        let outcome = output_within(cmd, Duration::from_millis(100));

        assert!(
            matches!(outcome, ProbeOutcome::TimedOut),
            "the outer probe must time out"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "an escaped descendant retained a reader thread for {:?}",
            start.elapsed()
        );
    }
}
