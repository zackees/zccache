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
        if crate::platform::host::is_windows() {
            crate::platform::fs::path::verbatim_path(dir).map(NormalizedPath::new)
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
    /// Wrap a [`blake3::Hash`].
    #[must_use]
    pub fn from_hash(h: blake3::Hash) -> Self {
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
    header[16..48].copy_from_slice(::blake3::hash(payload).as_bytes());
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
    if ::blake3::hash(payload).as_bytes() != expected {
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
/// `tests/artifact_kv_stress.rs` (16 threads x 100 writes to one key) fails
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
    crate::platform::fs::replace::is_transient_share_error(error)
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
        tokio::task::spawn_blocking(move || store.get(&namespace, &key))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Last-writer-wins. Writes via tempfile + rename so a crash mid-write
    /// leaves either the previous value or none — never a partial one.
    pub fn put(&self, namespace: &str, key: &Key, value: &[u8]) -> KvResult<usize> {
        use std::io::Write;

        check_namespace(namespace)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(KvError::TooLarge(value.len(), MAX_VALUE_BYTES));
        }
        let path = self.value_path(namespace, key);
        let dir = self.namespace_dir(namespace);
        std::fs::create_dir_all(&dir)?;

        let mut tmp = tempfile::NamedTempFile::new_in(dir.as_path())?;
        tmp.write_all(&encode_header(value))?;
        tmp.write_all(value)?;
        tmp.as_file().sync_all()?;
        persist_atomically(tmp, path.as_path())?;
        Ok(value.len())
    }

    /// Async wrapper for [`Self::put`] that runs file I/O and fsync on Tokio's
    /// blocking pool.
    pub async fn put_async(&self, namespace: &str, key: &Key, value: &[u8]) -> KvResult<usize> {
        let store = self.clone();
        let namespace = namespace.to_string();
        let key = *key;
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || store.put(&namespace, &key, &value))
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
        tokio::task::spawn_blocking(move || store.remove(&namespace, &key))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Drop every entry under `namespace`. Other namespaces are untouched.
    pub fn clear_namespace(&self, namespace: &str) -> KvResult<()> {
        check_namespace(namespace)?;
        let dir = self.namespace_dir(namespace);
        match std::fs::remove_dir_all(dir.as_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(KvError::Io(e)),
        }
    }

    /// Async wrapper for [`Self::clear_namespace`] that runs the directory
    /// removal on Tokio's blocking pool.
    pub async fn clear_namespace_async(&self, namespace: &str) -> KvResult<()> {
        let store = self.clone();
        let namespace = namespace.to_string();
        tokio::task::spawn_blocking(move || store.clear_namespace(&namespace))
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
        tokio::task::spawn_blocking(move || store.list_namespace(&namespace))
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
        tokio::task::spawn_blocking(move || store.namespace_bytes(&namespace))
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
        tokio::task::spawn_blocking(move || store.total_bytes())
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
        tokio::task::spawn_blocking(move || store.stats())
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, KvStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = KvStore::open(dir.path()).unwrap();
        (dir, s)
    }

    fn key_from(seed: &[u8]) -> Key {
        Key::from_hash(::blake3::hash(seed))
    }

    fn value_file(dir: &tempfile::TempDir, ns: &str, k: &Key) -> std::path::PathBuf {
        dir.path()
            .join("kv")
            .join(ns)
            .join(format!("{}.bin", k.to_hex()))
    }

    // ---- F1: round-trip across size boundaries ----
    #[test]
    fn f1_round_trip_sizes() {
        let (_d, s) = store();
        let sizes = [0, 1, 100, 4095, 4096, 4097, 64 * 1024];
        for (i, n) in sizes.iter().enumerate() {
            let k = key_from(&i.to_le_bytes());
            let val: Vec<u8> = (0..*n).map(|j| (j % 251) as u8).collect();
            assert_eq!(s.put("ns", &k, &val).unwrap(), val.len());
            let got = s.get("ns", &k).unwrap().unwrap();
            assert_eq!(got, val, "size {n} round-trip mismatch");
        }
    }

    // ---- F2: miss returns Ok(None) ----
    #[test]
    fn f2_miss_returns_none() {
        let (_d, s) = store();
        let k = key_from(b"nope");
        assert!(s.get("ns", &k).unwrap().is_none());
    }

    // ---- F3: overwrite ----
    #[test]
    fn f3_overwrite() {
        let (_d, s) = store();
        let k = key_from(b"ow");
        s.put("ns", &k, b"v1").unwrap();
        s.put("ns", &k, b"v2").unwrap();
        assert_eq!(s.get("ns", &k).unwrap().unwrap(), b"v2");
    }

    // ---- F4: remove + idempotent ----
    #[test]
    fn f4_remove() {
        let (_d, s) = store();
        let k = key_from(b"r");
        s.put("ns", &k, b"x").unwrap();
        s.remove("ns", &k).unwrap();
        assert!(s.get("ns", &k).unwrap().is_none());
        s.remove("ns", &k).unwrap();
    }

    // ---- F5: clear_namespace isolation ----
    #[test]
    fn f5_clear_namespace_isolation() {
        let (_d, s) = store();
        let k = key_from(b"k");
        s.put("a", &k, b"in-a").unwrap();
        s.put("b", &k, b"in-b").unwrap();
        s.clear_namespace("a").unwrap();
        assert!(s.get("a", &k).unwrap().is_none());
        assert_eq!(s.get("b", &k).unwrap().unwrap(), b"in-b");
    }

    // ---- F5b: clearing a namespace that was never written is a no-op ----
    #[test]
    fn f5b_clear_absent_namespace_is_ok() {
        let (_d, s) = store();
        s.clear_namespace("never-written").unwrap();
    }

    // ---- F6: list_namespace sorted, lengths correct ----
    #[test]
    fn f6_list_sorted_and_lengths() {
        let (_d, s) = store();
        let mut keys: Vec<Key> = (0u32..5).map(|i| key_from(&i.to_le_bytes())).collect();
        let mut expected: std::collections::HashMap<String, u64> = Default::default();
        for (i, k) in keys.iter().enumerate() {
            let n = if i % 2 == 0 { 10 } else { 4196 };
            s.put("ns", k, &vec![i as u8; n]).unwrap();
            expected.insert(k.to_hex(), n as u64);
        }
        let listed = s.list_namespace("ns").unwrap();
        assert_eq!(listed.len(), 5);
        keys.sort_by_key(|k| k.to_hex());
        for (i, (k, len)) in listed.iter().enumerate() {
            assert_eq!(k.to_hex(), keys[i].to_hex(), "list not sorted at {i}");
            assert_eq!(*len, expected[&k.to_hex()], "payload length for entry {i}");
        }
    }

    // ---- F6b: listing an absent namespace is empty, not an error ----
    #[test]
    fn f6b_list_absent_namespace_is_empty() {
        let (_d, s) = store();
        assert!(s.list_namespace("absent").unwrap().is_empty());
    }

    // ---- F7: total_bytes == sum of namespace_bytes ----
    #[test]
    fn f7_total_eq_sum() {
        let (_d, s) = store();
        for ns in &["a", "b", "c"] {
            for i in 0..3 {
                let k = key_from(format!("{ns}-{i}").as_bytes());
                s.put(ns, &k, &vec![0u8; 50 + i]).unwrap();
            }
        }
        let total = s.total_bytes().unwrap();
        let sum: u64 = ["a", "b", "c"]
            .iter()
            .map(|ns| s.namespace_bytes(ns).unwrap())
            .sum();
        assert_eq!(total, sum);
        // Header bytes must not be counted: 3 namespaces x (50 + 51 + 52).
        assert_eq!(total, 3 * (50 + 51 + 52));
    }

    // ---- F8: every value is a file of header + payload ----
    #[test]
    fn f8_values_are_files() {
        let (d, s) = store();
        let small = key_from(b"small");
        let large = key_from(b"large");
        s.put("ns", &small, &[1u8; 16]).unwrap();
        s.put("ns", &large, &vec![2u8; 40_000]).unwrap();

        for (k, payload) in [(&small, 16usize), (&large, 40_000usize)] {
            let path = value_file(&d, "ns", k);
            assert!(path.exists(), "value must be on disk at {}", path.display());
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                (payload + HEADER_LEN) as u64,
                "file is header + payload"
            );
        }
    }

    // ---- F9: tampered payload → Corrupt ----
    #[test]
    fn f9_tampered_payload_detected() {
        let (d, s) = store();
        let k = key_from(b"corrupt");
        s.put("ns", &k, &vec![7u8; 4196]).unwrap();
        let path = value_file(&d, "ns", &k);
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a payload byte, leaving the header's recorded blake3 intact.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let err = s.get("ns", &k).unwrap_err();
        match err {
            KvError::Corrupt(_, msg) => assert!(msg.contains("blake3 mismatch"), "msg={msg}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    // ---- F9b: truncated payload → Corrupt, not a short read ----
    #[test]
    fn f9b_truncated_file_detected() {
        let (d, s) = store();
        let k = key_from(b"trunc");
        s.put("ns", &k, &vec![3u8; 1000]).unwrap();
        let path = value_file(&d, "ns", &k);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
        let err = s.get("ns", &k).unwrap_err();
        match err {
            KvError::Corrupt(_, msg) => assert!(msg.contains("length mismatch"), "msg={msg}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    // ---- F9c: a header-only truncation is still Corrupt ----
    #[test]
    fn f9c_header_truncation_detected() {
        let (d, s) = store();
        let k = key_from(b"hdr");
        s.put("ns", &k, b"payload").unwrap();
        std::fs::write(value_file(&d, "ns", &k), b"ZCKV").unwrap();
        let err = s.get("ns", &k).unwrap_err();
        match err {
            KvError::Corrupt(_, msg) => assert!(msg.contains("truncated"), "msg={msg}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    // ---- F9d: a file that is not ours at all is Corrupt, not silently data ----
    #[test]
    fn f9d_foreign_magic_detected() {
        let (d, s) = store();
        let k = key_from(b"foreign");
        s.put("ns", &k, b"payload").unwrap();
        std::fs::write(value_file(&d, "ns", &k), vec![0u8; HEADER_LEN + 8]).unwrap();
        let err = s.get("ns", &k).unwrap_err();
        match err {
            KvError::Corrupt(_, msg) => assert!(msg.contains("bad magic"), "msg={msg}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    // ---- F10: hex round-trip + bad inputs ----
    #[test]
    fn f10_key_hex_round_trip() {
        let h = ::blake3::hash(b"hello");
        let k = Key::from_hash(h);
        let hex = k.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_eq!(Key::from_hex(&hex).unwrap(), k);
        assert_eq!(Key::from_hex(&hex.to_ascii_uppercase()).unwrap(), k);

        assert!(matches!(Key::from_hex(""), Err(KvError::BadKey)));
        assert!(matches!(Key::from_hex("zz"), Err(KvError::BadKey)));
        assert!(matches!(
            Key::from_hex(&"a".repeat(63)),
            Err(KvError::BadKey)
        ));
        assert!(matches!(
            Key::from_hex(&"a".repeat(65)),
            Err(KvError::BadKey)
        ));
        let mut bad = "a".repeat(64);
        bad.replace_range(0..1, "g");
        assert!(matches!(Key::from_hex(&bad), Err(KvError::BadKey)));
    }

    // ---- F11: namespace validator ----
    #[test]
    fn f11_namespace_validator() {
        assert!(is_valid_namespace("a"));
        assert!(is_valid_namespace("0"));
        assert!(is_valid_namespace("library-selection"));
        assert!(is_valid_namespace(&"x".repeat(64)));

        assert!(!is_valid_namespace(""));
        assert!(!is_valid_namespace("A"));
        assert!(!is_valid_namespace("name with space"));
        assert!(!is_valid_namespace("a/b"));
        assert!(!is_valid_namespace("日本語"));
        assert!(!is_valid_namespace(&"x".repeat(65)));
        assert!(!is_valid_namespace("a::b"));
    }

    // ---- F12: schema_version mismatch surfaces as Corrupt ----
    #[test]
    fn f12_schema_version_mismatch() {
        let (d, s) = store();
        let k = key_from(b"sv");
        s.put("ns", &k, b"hi").unwrap();
        let path = value_file(&d, "ns", &k);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let err = s.get("ns", &k).unwrap_err();
        match err {
            KvError::Corrupt(_, msg) => assert!(msg.contains("schema_version="), "msg={msg}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    // ---- F13: a foreign file in the namespace dir is ignored by listing ----
    #[test]
    fn f13_foreign_files_are_skipped() {
        let (d, s) = store();
        let k = key_from(b"real");
        s.put("ns", &k, b"value").unwrap();
        let ns_dir = d.path().join("kv").join("ns");
        std::fs::write(ns_dir.join("README.txt"), b"not a value").unwrap();
        std::fs::write(ns_dir.join("deadbeef.bin"), b"bad hex name").unwrap();

        let listed = s.list_namespace("ns").unwrap();
        assert_eq!(listed.len(), 1, "only the real value is listed");
        assert_eq!(listed[0].0.to_hex(), k.to_hex());
    }

    // ---- I1..I4: namespace edge cases via put ----
    #[test]
    fn i1_empty_namespace_rejected() {
        let (_d, s) = store();
        let k = key_from(b"x");
        assert!(matches!(s.put("", &k, b"v"), Err(KvError::BadNamespace)));
    }

    #[test]
    fn i2_namespace_at_limit_ok() {
        let (_d, s) = store();
        let k = key_from(b"x");
        let ns = "a".repeat(64);
        s.put(&ns, &k, b"v").unwrap();
    }

    #[test]
    fn i3_namespace_too_long_rejected() {
        let (_d, s) = store();
        let k = key_from(b"x");
        let ns = "a".repeat(65);
        assert!(matches!(s.put(&ns, &k, b"v"), Err(KvError::BadNamespace)));
    }

    #[test]
    fn i4_namespace_with_double_colon_rejected() {
        let (_d, s) = store();
        let k = key_from(b"x");
        assert!(matches!(
            s.put("a::b", &k, b"v"),
            Err(KvError::BadNamespace)
        ));
    }

    // ---- I6: max value bytes (allocates 64 MiB, runs only under --full) ----
    #[test]
    #[ignore = "allocates 64 MiB; see tests/artifact_kv_stress.rs for max-cap coverage"]
    fn i6_too_large_rejected() {
        let (_d, s) = store();
        let k = key_from(b"big");
        let oversized = MAX_VALUE_BYTES + 1;
        let v = vec![0u8; oversized];
        let err = s.put("ns", &k, &v).unwrap_err();
        assert!(matches!(err, KvError::TooLarge(n, m) if n == oversized && m == MAX_VALUE_BYTES));
    }

    // ---- I7: same key, different namespaces are independent ----
    #[test]
    fn i7_namespaces_are_independent() {
        let (_d, s) = store();
        let k = key_from(b"shared");
        s.put("a", &k, b"a-val").unwrap();
        s.put("b", &k, b"b-val").unwrap();
        assert_eq!(s.get("a", &k).unwrap().unwrap(), b"a-val");
        assert_eq!(s.get("b", &k).unwrap().unwrap(), b"b-val");
    }

    // ---- P8 / I8: reopen sees prior writes ----
    #[test]
    fn p8_reopen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let k = key_from(b"persist");
        {
            let s = KvStore::open(dir.path()).unwrap();
            s.put("ns", &k, &vec![3u8; 4106]).unwrap();
        }
        let s = KvStore::open(dir.path()).unwrap();
        assert_eq!(s.get("ns", &k).unwrap().unwrap(), vec![3u8; 4106]);
    }

    // ---- P3: case-insensitive key parsing means UPPER and lower collide ----
    #[test]
    fn p3_case_insensitive_key_parses_to_same_key() {
        let k = Key::from_hash(::blake3::hash(b"x"));
        let lower = k.to_hex();
        let upper = lower.to_ascii_uppercase();
        assert_eq!(
            Key::from_hex(&lower).unwrap(),
            Key::from_hex(&upper).unwrap()
        );
    }

    // ---- #1352: two stores over one directory coexist ----
    //
    // This is the regression the redb-backed implementation could not pass: a
    // second `Database::create` on the same file returned
    // `DatabaseAlreadyOpen`, which is the whole reason this store dropped its
    // database. Two `KvStore`s are now independent handles onto a shared
    // directory, so this must simply work.
    #[test]
    fn concurrent_stores_over_one_dir_do_not_lock_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let a = KvStore::open(dir.path()).unwrap();
        let b = KvStore::open(dir.path()).unwrap();

        let ka = key_from(b"from-a");
        let kb = key_from(b"from-b");
        a.put("ns", &ka, b"a-wrote").unwrap();
        b.put("ns", &kb, b"b-wrote").unwrap();

        // Each store sees the other's committed write.
        assert_eq!(b.get("ns", &ka).unwrap().unwrap(), b"a-wrote");
        assert_eq!(a.get("ns", &kb).unwrap().unwrap(), b"b-wrote");
        assert_eq!(a.list_namespace("ns").unwrap().len(), 2);
    }
}
