//! Namespaced blake3-keyed key/value store backed by content-addressed files.
//!
//! Lives next to [`ArtifactStore`](super::ArtifactStore). Every value is one
//! file at `<cache_dir>/kv/<namespace>/<hex>.bin`, written tempfile+rename so a
//! crash mid-write never publishes a partial value. Each file carries a small
//! header (magic, schema version, payload length, blake3 of the payload) so a
//! truncated or tampered file is detected on read rather than returned as data.
//! Hard cap [`MAX_VALUE_BYTES`].
//!
//! # Why there is no database here
//!
//! This store previously kept small values inline in a redb table
//! (`index.redb`) and spilled larger ones to sidecar files. redb takes an
//! **exclusive whole-file lock per `Database` handle** — by design, it is a
//! single-owner store. That made every `zccache kv` invocation take a process
//! lock, pay a write-transaction commit just to assert the table existed, and
//! fail opaquely if any other process had the file open.
//!
//! The access pattern never needed a database: keys are content-addressed, so
//! there are no range queries, no transactions spanning keys, and no secondary
//! indexes. The filesystem is already a content-addressed key/value store with
//! per-key locking granularity. Dropping redb removes the last exclusive file
//! lock from zccache and removes redb from the workspace entirely.
//!
//! The tradeoff: `list_namespace`, `stats`, and `total_bytes` are directory
//! walks rather than table scans. For a store whose namespaces hold tens to
//! thousands of keys this is comparable, and it removes a per-call fsync from
//! every read.
//!
//! **On-disk break.** Values written by the redb-backed version are not read by
//! this one. There is no migration: the store had no consumers, so there is no
//! data to carry forward. A stale `index.redb` is left untouched on disk;
//! remove it with `zccache clear` or by hand.

use std::path::Path;
use std::sync::Arc;

use zccache_core::NormalizedPath;

/// Windows long-path (`\\?\`) helpers. On non-Windows platforms every entry
/// point is a no-op pass-through.
mod long_path {
    use std::path::Path;

    use zccache_core::NormalizedPath;

    /// Normalize `dir` so that paths joined off it can exceed `MAX_PATH`
    /// without tripping the legacy Win32 path APIs used by transitive crates
    /// (notably `tempfile`'s rename-on-persist call into `MoveFileExW`).
    ///
    /// On Windows we canonicalize to a verbatim (`\\?\`-prefixed) form so that
    /// every `path.join(...)` we do downstream inherits the prefix. On Unix
    /// this is a pure clone — long paths are not a thing there.
    ///
    /// The dir must already exist; callers in this crate `create_dir_all`
    /// first.
    pub(super) fn ensure_long_path(dir: &Path) -> std::io::Result<NormalizedPath> {
        if kernal_api::platform::host::target_is_windows() {
            kernal_api::platform::fs::native_call_path(dir).map(NormalizedPath::new)
        } else {
            Ok(NormalizedPath::new(dir))
        }
    }
}

/// Historical inline-vs-spill boundary, retained as a size landmark.
///
/// When this store was redb-backed, values at or below this size lived inline
/// in a redb row and larger ones spilled to a sidecar file. **Every value is
/// now a file**, so this constant no longer changes storage behaviour. It is
/// kept because callers (notably the stress suite) use it as a convenient
/// "comfortably larger than a small value" boundary, and because removing a
/// public constant is a breaking change with no benefit.
pub const INLINE_THRESHOLD: usize = 4 * 1024;

/// Hard cap on a single value (64 MiB). Over-cap → [`KvError::TooLarge`].
pub const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;

const SCHEMA_VERSION: u32 = 2;
const NAMESPACE_MAX: usize = 64;

/// `b"ZCKV"`. Distinguishes a value file from anything else that lands in the
/// namespace directory.
const MAGIC: [u8; 4] = *b"ZCKV";

/// magic(4) + version(4) + payload len(8) + blake3(32).
const HEADER_LEN: usize = 4 + 4 + 8 + 32;

/// 32-byte content key. Stable hex form is always lowercase 64 chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub [u8; 32]);

impl Key {
    /// Wrap a [`kernal_api::hash::Blake3Digest`].
    #[must_use]
    pub fn from_hash(h: kernal_api::hash::Blake3Digest) -> Self {
        Self(*h.as_bytes())
    }

    /// Underlying 32-byte content.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase 64-char hex representation.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            out.push(hex_nibble(byte >> 4));
            out.push(hex_nibble(byte & 0x0f));
        }
        out
    }

    /// Parse a 64-char hex string. Accepts upper- or lower-case hex.
    pub fn from_hex(hex: &str) -> KvResult<Self> {
        if hex.len() != 64 {
            return Err(KvError::BadKey);
        }
        let bytes = hex.as_bytes();
        let mut out = [0u8; 32];
        for i in 0..32 {
            let hi = parse_nibble(bytes[2 * i]).ok_or(KvError::BadKey)?;
            let lo = parse_nibble(bytes[2 * i + 1]).ok_or(KvError::BadKey)?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!(),
    }
}

fn parse_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Errors returned by [`KvStore`].
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// IO error from disk or filesystem.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Namespace failed validation. See [`is_valid_namespace`].
    #[error("namespace must be 1..=64 chars of [a-z0-9-] without `::`")]
    BadNamespace,
    /// Hex-key parsing failed (length, character class).
    #[error("key must be 32 bytes (64 hex chars)")]
    BadKey,
    /// Stored value was malformed. Includes the offending key for debugging.
    #[error("corrupt entry for key {0}: {1}")]
    Corrupt(String, String),
    /// Value exceeded [`MAX_VALUE_BYTES`].
    #[error("value too large: {0} bytes (max {1})")]
    TooLarge(usize, usize),
    /// Tokio blocking task failed before returning the underlying result.
    #[error("blocking task join: {0}")]
    BlockingJoin(String),
}

/// Result type for KV operations.
pub type KvResult<T> = std::result::Result<T, KvError>;

/// Validate that `ns` matches `[a-z0-9-]{1,64}` and contains no `::`.
#[must_use]
pub fn is_valid_namespace(ns: &str) -> bool {
    if ns.is_empty() || ns.len() > NAMESPACE_MAX {
        return false;
    }
    if ns.contains("::") {
        return false;
    }
    ns.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_namespace(ns: &str) -> KvResult<()> {
    if is_valid_namespace(ns) {
        Ok(())
    } else {
        Err(KvError::BadNamespace)
    }
}

/// Serialize the on-disk header for a payload.
fn encode_header(payload: &[u8]) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
    header[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[16..48].copy_from_slice(::kernal_api::hash::blake3_bytes(payload).as_bytes());
    header
}

/// Validate a value file's header against its payload.
///
/// `label` names the entry in any [`KvError::Corrupt`] raised, so callers pass
/// something the operator can act on (`<namespace>::<hex>`).
fn decode_and_verify(label: &str, raw: &[u8]) -> KvResult<Vec<u8>> {
    if raw.len() < HEADER_LEN {
        return Err(KvError::Corrupt(
            label.to_string(),
            format!("truncated: {} bytes, header needs {HEADER_LEN}", raw.len()),
        ));
    }
    if raw[0..4] != MAGIC {
        return Err(KvError::Corrupt(
            label.to_string(),
            "bad magic (not a kv value file)".to_string(),
        ));
    }
    let version = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    if version != SCHEMA_VERSION {
        return Err(KvError::Corrupt(
            label.to_string(),
            format!("schema_version={version}"),
        ));
    }
    let declared_len = u64::from_le_bytes([
        raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
    ]);
    let payload = &raw[HEADER_LEN..];
    if payload.len() as u64 != declared_len {
        return Err(KvError::Corrupt(
            label.to_string(),
            format!(
                "length mismatch: got {}, expected {declared_len}",
                payload.len()
            ),
        ));
    }
    let expected = &raw[16..48];
    if ::kernal_api::hash::blake3_bytes(payload).as_bytes() != expected {
        return Err(KvError::Corrupt(
            label.to_string(),
            "blake3 mismatch".to_string(),
        ));
    }
    Ok(payload.to_vec())
}

/// Sharded per-key mutexes serializing the *rename* step of concurrent writes
/// to the same key within one process.
///
/// This is deliberately not a store lock. The shard is chosen from the
/// destination path, so writers to distinct keys never contend, and the guard
/// is held only across the rename — never across the payload write or its
/// fsync, which are the expensive parts.
///
/// Why it exists: `MOVEFILE_REPLACE_EXISTING` cannot start while another
/// handle is open on the destination, and 16 threads hammering one key
/// (`c1_thundering_herd_same_key`) keep it open essentially always, so the
/// bounded retry below alone cannot converge. Cross-process same-key writers
/// are still handled by that retry; they are far rarer than the in-process
/// case and do not sustain the same rename rate.
fn rename_shard(dest: &Path) -> &'static std::sync::Mutex<()> {
    use std::hash::{Hash, Hasher};

    const SHARDS: usize = 64;
    static LOCKS: std::sync::OnceLock<Vec<std::sync::Mutex<()>>> = std::sync::OnceLock::new();

    let locks = LOCKS.get_or_init(|| (0..SHARDS).map(|_| std::sync::Mutex::new(())).collect());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dest.hash(&mut hasher);
    &locks[(hasher.finish() as usize) % SHARDS]
}

/// Sharded per-namespace gates ordering puts against `clear_namespace` within
/// one process: puts share the gate, a clear takes it exclusively.
///
/// Without it a clear removed the namespace directory between a put's
/// `create_dir_all` and its tempfile create or rename, and the put failed with
/// `NotFound` (#1648). Keyed by the namespace directory, so stores opened
/// separately over one cache dir share a gate, and puts never exclude each
/// other.
fn namespace_gate(dir: &Path) -> &'static std::sync::RwLock<()> {
    use std::hash::{Hash, Hasher};

    const SHARDS: usize = 16;
    static GATES: std::sync::OnceLock<Vec<std::sync::RwLock<()>>> = std::sync::OnceLock::new();

    let gates = GATES.get_or_init(|| (0..SHARDS).map(|_| std::sync::RwLock::new(())).collect());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dir.hash(&mut hasher);
    &gates[(hasher.finish() as usize) % SHARDS]
}

/// One attempt at publishing `value` at `dest` inside `dir`: tempfile, fsync,
/// atomic rename.
fn write_value(dir: &Path, dest: &Path, value: &[u8]) -> KvResult<()> {
    use std::io::Write;

    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&encode_header(value))?;
    tmp.write_all(value)?;
    tmp.as_file().sync_all()?;
    persist_atomically(tmp, dest)
}

/// Publish `tmp` at `dest`, retrying the transient Windows sharing failures.
///
/// The rename itself is atomic on every platform we support. What is *not*
/// guaranteed on Windows is that it can start: `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING` fails with `ERROR_ACCESS_DENIED` (5) or
/// `ERROR_SHARING_VIOLATION` (32) while another handle is open on the
/// destination — a concurrent reader, another writer replacing the same key,
/// or an antivirus scanner mid-scan.
///
/// The redb-backed implementation never hit this because every write to a key
/// was serialized through a single database write transaction. File-per-key
/// removes that serialization, which is the point — but it means same-key
/// writers now race at the rename. `c1_thundering_herd_same_key` in
/// `tests/stress/artifact_kv_stress.rs` (16 threads x 100 writes to one key) fails
/// without this retry.
///
/// Bounded so a genuine permissions error still surfaces rather than hanging:
/// ~1 s total across exponentially-growing sleeps, then the real error.
fn persist_atomically(mut tmp: tempfile::NamedTempFile, dest: &Path) -> KvResult<()> {
    const MAX_ELAPSED: std::time::Duration = std::time::Duration::from_secs(1);

    // Held across the rename only. The payload write and fsync already
    // happened in the caller, outside this critical section.
    let _shard = rename_shard(dest)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let started = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(1);
    loop {
        match tmp.persist(dest) {
            Ok(_) => return Ok(()),
            Err(e) if is_transient_share_error(&e.error) && started.elapsed() < MAX_ELAPSED => {
                // `persist` gives the temp file back on failure, so the retry
                // republishes the same already-fsynced bytes.
                tmp = e.file;
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(64));
            }
            Err(e) => return Err(KvError::Io(e.error)),
        }
    }
}

/// Whether `error` is a Windows sharing/locking failure that a retry may clear.
///
/// On Unix `rename(2)` over an open file succeeds, so this is always false and
/// the retry loop never engages.
fn is_transient_share_error(error: &std::io::Error) -> bool {
    kernal_api::platform::fs::replacement::is_transient_share_error(error)
}

/// Payload length recorded in a value file's header, without reading the body.
///
/// Used by the listing/stats walks so they cost one `read` of the header
/// instead of the whole value. Returns `Ok(None)` for anything that is not a
/// well-formed value file.
fn read_declared_len(path: &Path) -> std::io::Result<Option<u64>> {
    use std::io::Read;

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut header = [0u8; HEADER_LEN];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if header[0..4] != MAGIC {
        return Ok(None);
    }
    Ok(Some(u64::from_le_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14],
        header[15],
    ])))
}

/// Namespaced key/value store over content-addressed files.
///
/// Cheap to clone (one `Arc<NormalizedPath>`); intended to be passed across
/// threads. Writes are durable before the call returns (payload fsync +
/// atomic rename). Unlike the previous redb-backed implementation this takes
/// no process-wide or cross-process lock, so concurrent readers and writers on
/// distinct keys never contend.
#[derive(Clone)]
pub struct KvStore {
    cache_dir: Arc<NormalizedPath>,
}

impl KvStore {
    /// Open under the canonical zccache root (`daemon_state_dir()`).
    pub fn open_default() -> KvResult<Self> {
        let dir = zccache_core::config::daemon_state_dir();
        Self::open(dir)
    }

    /// Open at an explicit dir. Creates the dir if missing.
    ///
    /// Opening is now just directory setup — there is no database file to
    /// create and no lock to acquire, so two processes may hold a `KvStore`
    /// over the same directory concurrently.
    pub fn open<P: AsRef<Path>>(dir: P) -> KvResult<Self> {
        let mut dir = NormalizedPath::new(dir.as_ref());
        std::fs::create_dir_all(&dir)?;
        // On Windows, normalize to a `\\?\`-prefixed (verbatim) form so that
        // every value path joined off `cache_dir` exceeds `MAX_PATH` safely.
        // No-op on Unix.
        dir = long_path::ensure_long_path(dir.as_path())?;
        Ok(Self {
            cache_dir: Arc::new(dir),
        })
    }

    fn kv_root(&self) -> NormalizedPath {
        self.cache_dir.join("kv")
    }

    fn namespace_dir(&self, namespace: &str) -> NormalizedPath {
        self.kv_root().join(namespace)
    }

    fn value_path(&self, namespace: &str, key: &Key) -> NormalizedPath {
        self.namespace_dir(namespace)
            .join(format!("{}.bin", key.to_hex()))
    }

    fn label(namespace: &str, key: &Key) -> String {
        let mut s = String::with_capacity(namespace.len() + 2 + 64);
        s.push_str(namespace);
        s.push_str("::");
        s.push_str(&key.to_hex());
        s
    }

    /// Return the value for `(namespace, key)`, or `Ok(None)` on miss.
    pub fn get(&self, namespace: &str, key: &Key) -> KvResult<Option<Vec<u8>>> {
        check_namespace(namespace)?;
        let path = self.value_path(namespace, key);
        let raw = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(KvError::Io(e)),
        };
        decode_and_verify(&Self::label(namespace, key), &raw).map(Some)
    }

    /// Async wrapper for [`Self::get`] that runs file I/O on Tokio's blocking
    /// pool.
    pub async fn get_async(&self, namespace: &str, key: &Key) -> KvResult<Option<Vec<u8>>> {
        let store = self.clone();
        let namespace = namespace.to_string();
        let key = *key;
        kernal_api::async_engine::launch_blocking(move || store.get(&namespace, &key))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Last-writer-wins. Writes via tempfile + rename so a crash mid-write
    /// leaves either the previous value or none — never a partial one.
    ///
    /// A put racing [`Self::clear_namespace`] lands either before the clear
    /// (and is dropped) or after it (and survives); it never fails. In one
    /// process the namespace gate orders the two. Across processes a clear
    /// can still remove the directory under an in-flight put, which then
    /// surfaces as `NotFound` and is retried from the top (#1648).
    pub fn put(&self, namespace: &str, key: &Key, value: &[u8]) -> KvResult<usize> {
        const ATTEMPTS: usize = 8;

        check_namespace(namespace)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(KvError::TooLarge(value.len(), MAX_VALUE_BYTES));
        }
        let path = self.value_path(namespace, key);
        let dir = self.namespace_dir(namespace);
        let _gate = namespace_gate(dir.as_path())
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut attempt = 1;
        loop {
            match write_value(dir.as_path(), path.as_path(), value) {
                Err(KvError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    if attempt == ATTEMPTS {
                        return Err(KvError::Io(e));
                    }
                    attempt += 1;
                }
                result => return result.map(|()| value.len()),
            }
        }
    }

    /// Async wrapper for [`Self::put`] that runs file I/O and fsync on Tokio's
    /// blocking pool.
    pub async fn put_async(&self, namespace: &str, key: &Key, value: &[u8]) -> KvResult<usize> {
        let store = self.clone();
        let namespace = namespace.to_string();
        let key = *key;
        let value = value.to_vec();
        kernal_api::async_engine::launch_blocking(move || store.put(&namespace, &key, &value))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Idempotent: missing key returns `Ok(())`.
    pub fn remove(&self, namespace: &str, key: &Key) -> KvResult<()> {
        check_namespace(namespace)?;
        let path = self.value_path(namespace, key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(KvError::Io(e)),
        }
    }

    /// Async wrapper for [`Self::remove`] that runs file I/O on Tokio's
    /// blocking pool.
    pub async fn remove_async(&self, namespace: &str, key: &Key) -> KvResult<()> {
        let store = self.clone();
        let namespace = namespace.to_string();
        let key = *key;
        kernal_api::async_engine::launch_blocking(move || store.remove(&namespace, &key))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Drop every entry under `namespace`. Other namespaces are untouched.
    ///
    /// Holds the namespace gate exclusively, so no put from this process is
    /// mid-write while the directory is removed. A put from another process
    /// can still drop a file in during the walk, which makes the final
    /// `rmdir` fail with `DirectoryNotEmpty`; the walk is retried (#1648).
    pub fn clear_namespace(&self, namespace: &str) -> KvResult<()> {
        const ATTEMPTS: usize = 8;

        check_namespace(namespace)?;
        let dir = self.namespace_dir(namespace);
        let _gate = namespace_gate(dir.as_path())
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut attempt = 1;
        loop {
            match std::fs::remove_dir_all(dir.as_path()) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e)
                    if e.kind() == std::io::ErrorKind::DirectoryNotEmpty && attempt < ATTEMPTS =>
                {
                    attempt += 1;
                }
                Err(e) => return Err(KvError::Io(e)),
            }
        }
    }

    /// Async wrapper for [`Self::clear_namespace`] that runs the directory
    /// removal on Tokio's blocking pool.
    pub async fn clear_namespace_async(&self, namespace: &str) -> KvResult<()> {
        let store = self.clone();
        let namespace = namespace.to_string();
        kernal_api::async_engine::launch_blocking(move || store.clear_namespace(&namespace))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Sorted by hex-key. Returns `(key, value-len)` pairs.
    ///
    /// Files that are not well-formed value files (foreign files, leftover
    /// temp files from an interrupted `put`) are skipped rather than failing
    /// the listing — the directory is not exclusively ours in the way a
    /// database table was.
    pub fn list_namespace(&self, namespace: &str) -> KvResult<Vec<(Key, u64)>> {
        check_namespace(namespace)?;
        let dir = self.namespace_dir(namespace);
        let entries = match std::fs::read_dir(dir.as_path()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(KvError::Io(e)),
        };
        let mut out: Vec<(Key, u64)> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(hex) = name.strip_suffix(".bin") else {
                continue;
            };
            let Ok(key) = Key::from_hex(hex) else {
                continue;
            };
            if let Some(len) = read_declared_len(&entry.path())? {
                out.push((key, len));
            }
        }
        out.sort_by_key(|(key, _len)| key.to_hex());
        Ok(out)
    }

    /// Async wrapper for [`Self::list_namespace`] that keeps the directory walk
    /// off Tokio runtime threads.
    pub async fn list_namespace_async(&self, namespace: &str) -> KvResult<Vec<(Key, u64)>> {
        let store = self.clone();
        let namespace = namespace.to_string();
        kernal_api::async_engine::launch_blocking(move || store.list_namespace(&namespace))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Sum of value lengths in `namespace`. Does not include header overhead.
    pub fn namespace_bytes(&self, namespace: &str) -> KvResult<u64> {
        let entries = self.list_namespace(namespace)?;
        Ok(entries.iter().map(|(_, l)| *l).sum())
    }

    /// Async wrapper for [`Self::namespace_bytes`] that keeps the directory
    /// walk off Tokio runtime threads.
    pub async fn namespace_bytes_async(&self, namespace: &str) -> KvResult<u64> {
        let store = self.clone();
        let namespace = namespace.to_string();
        kernal_api::async_engine::launch_blocking(move || store.namespace_bytes(&namespace))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Namespace directory names under `kv/`, sorted lexically.
    fn namespaces(&self) -> KvResult<Vec<String>> {
        let entries = match std::fs::read_dir(self.kv_root().as_path()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(KvError::Io(e)),
        };
        let mut out: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_valid_namespace(name) {
                out.push(name.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Sum of value lengths across every namespace.
    pub fn total_bytes(&self) -> KvResult<u64> {
        let mut total: u64 = 0;
        for ns in self.namespaces()? {
            total += self.namespace_bytes(&ns)?;
        }
        Ok(total)
    }

    /// Async wrapper for [`Self::total_bytes`] that keeps the directory walk
    /// off Tokio runtime threads.
    pub async fn total_bytes_async(&self) -> KvResult<u64> {
        let store = self.clone();
        kernal_api::async_engine::launch_blocking(move || store.total_bytes())
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Per-namespace statistics. Returned namespaces are sorted lexically.
    pub fn stats(&self) -> KvResult<Vec<(String, u64)>> {
        let mut out: Vec<(String, u64)> = Vec::new();
        for ns in self.namespaces()? {
            let bytes = self.namespace_bytes(&ns)?;
            out.push((ns, bytes));
        }
        Ok(out)
    }

    /// Async wrapper for [`Self::stats`] that keeps the directory walk off
    /// Tokio runtime threads.
    pub async fn stats_async(&self) -> KvResult<Vec<(String, u64)>> {
        let store = self.clone();
        kernal_api::async_engine::launch_blocking(move || store.stats())
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }
}

#[cfg(test)]
mod tests;
