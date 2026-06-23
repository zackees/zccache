//! Namespaced blake3-keyed key/value store backed by redb.
//!
//! Lives next to [`ArtifactStore`](super::ArtifactStore) and reuses the same
//! redb file (`~/.zccache/index.redb`) via a separate `kv` table. Values
//! ≤ [`INLINE_THRESHOLD`] bytes are stored inline in redb; larger values
//! spill to `~/.zccache/kv/<namespace>/<hex>.bin` with a blake3 corruption
//! check on read. Hard cap [`MAX_VALUE_BYTES`].
//!
//! The on-disk row format carries a `schema_version`; opening a row with
//! a higher version than this binary supports is a hard error
//! ([`KvError::Corrupt`]) rather than silent garbage.

use std::path::Path;
use std::sync::Arc;

use crate::core::NormalizedPath;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

/// Windows long-path (`\\?\`) helpers. On non-Windows platforms every entry
/// point is a no-op pass-through.
mod long_path {
    use std::path::Path;

    use crate::core::NormalizedPath;

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
        #[cfg(windows)]
        {
            // `fs::canonicalize` on Windows returns the verbatim form
            // (`\\?\C:\...` or `\\?\UNC\...`), which is exactly what we need.
            // If the path already starts with `\\?\` we keep it as-is.
            if starts_with_verbatim(dir) {
                return Ok(NormalizedPath::new(dir));
            }
            std::fs::canonicalize(dir).map(NormalizedPath::new)
        }
        #[cfg(not(windows))]
        {
            Ok(NormalizedPath::new(dir))
        }
    }

    #[cfg(windows)]
    fn starts_with_verbatim(p: &Path) -> bool {
        // OsStr equality is byte-wise on Windows for the ASCII prefix; using
        // `to_string_lossy` is fine here because we only inspect the leading
        // four ASCII bytes.
        let s = p.as_os_str().to_string_lossy();
        s.starts_with(r"\\?\")
    }
}

const KV_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");

/// Inline-vs-spill threshold. Values ≤ this stay in redb; larger values
/// spill to a sidecar file under `kv/<namespace>/<hex>.bin`.
pub const INLINE_THRESHOLD: usize = 4 * 1024;

/// Hard cap on a single value (64 MiB). Over-cap → [`KvError::TooLarge`].
pub const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;

const SCHEMA_VERSION: u32 = 1;
const NAMESPACE_MAX: usize = 64;

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
    /// Underlying redb error (commit, transaction, table).
    #[error("redb: {0}")]
    Redb(String),
    /// Namespace failed validation. See [`is_valid_namespace`].
    #[error("namespace must be 1..=64 chars of [a-z0-9-] without `::`")]
    BadNamespace,
    /// Hex-key parsing failed (length, character class).
    #[error("key must be 32 bytes (64 hex chars)")]
    BadKey,
    /// Stored row was malformed. Includes the offending key for debugging.
    #[error("corrupt entry for key {0}: {1}")]
    Corrupt(String, String),
    /// Value exceeded [`MAX_VALUE_BYTES`].
    #[error("value too large: {0} bytes (max {1})")]
    TooLarge(usize, usize),
    /// Tokio blocking task failed before returning the underlying result.
    #[error("blocking task join: {0}")]
    BlockingJoin(String),
}

impl From<redb::Error> for KvError {
    fn from(e: redb::Error) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::DatabaseError> for KvError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::TransactionError> for KvError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::TableError> for KvError {
    fn from(e: redb::TableError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::StorageError> for KvError {
    fn from(e: redb::StorageError) -> Self {
        Self::Redb(e.to_string())
    }
}

impl From<redb::CommitError> for KvError {
    fn from(e: redb::CommitError) -> Self {
        Self::Redb(e.to_string())
    }
}

/// Result type for KV operations.
pub type KvResult<T> = std::result::Result<T, KvError>;

#[derive(Debug, Serialize, Deserialize)]
struct KvRow {
    schema_version: u32,
    body: KvBody,
}

#[derive(Debug, Serialize, Deserialize)]
enum KvBody {
    Inline(Vec<u8>),
    Spilled { len: u64, blake3: [u8; 32] },
}

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

fn composite(ns: &str, key: &Key) -> String {
    let mut s = String::with_capacity(ns.len() + 2 + 64);
    s.push_str(ns);
    s.push_str("::");
    s.push_str(&key.to_hex());
    s
}

/// Namespaced key/value store backed by redb plus optional spilled side files.
///
/// Cheap to clone (`Arc<Database>` + `Arc<NormalizedPath>`); intended to be passed
/// across threads. All writes are committed before the call returns
/// (single-fsync per `put`/`remove`/`clear_namespace`).
#[derive(Clone)]
pub struct KvStore {
    db: Arc<Database>,
    cache_dir: Arc<NormalizedPath>,
}

impl KvStore {
    /// Open under the canonical zccache root (`default_cache_dir()`).
    pub fn open_default() -> KvResult<Self> {
        let dir = crate::core::config::default_cache_dir();
        Self::open(dir)
    }

    /// Open at an explicit dir. Creates the dir + redb file if missing.
    pub fn open<P: AsRef<Path>>(dir: P) -> KvResult<Self> {
        let mut dir = NormalizedPath::new(dir.as_ref());
        std::fs::create_dir_all(&dir)?;
        // On Windows, normalize to a `\\?\`-prefixed (verbatim) form so that
        // every spill path joined off `cache_dir` exceeds `MAX_PATH` safely.
        // No-op on Unix.
        dir = long_path::ensure_long_path(dir.as_path())?;
        let db_path = dir.join("index.redb");
        let db = Database::create(&db_path).map_err(|e| KvError::Redb(e.to_string()))?;
        let store = Self {
            db: Arc::new(db),
            cache_dir: Arc::new(dir),
        };
        store.ensure_table()?;
        Ok(store)
    }

    /// Share an already-open redb database with the caller (typically
    /// [`ArtifactStore`](super::ArtifactStore)). Both stores see each other's
    /// commits; spill files live under `cache_dir/kv/...`.
    pub fn from_database<P: AsRef<Path>>(db: Arc<Database>, cache_dir: P) -> KvResult<Self> {
        let mut cache_dir = NormalizedPath::new(cache_dir.as_ref());
        std::fs::create_dir_all(&cache_dir)?;
        // Match the verbatim normalization done by [`KvStore::open`] so callers
        // who route long paths through `from_database` get the same long-path
        // safety on Windows.
        cache_dir = long_path::ensure_long_path(cache_dir.as_path())?;
        let store = Self {
            db,
            cache_dir: Arc::new(cache_dir),
        };
        store.ensure_table()?;
        Ok(store)
    }

    fn ensure_table(&self) -> KvResult<()> {
        let txn = self.db.begin_write()?;
        {
            let _t = txn.open_table(KV_TABLE)?;
        }
        txn.commit()?;
        Ok(())
    }

    fn spill_path(&self, namespace: &str, key: &Key) -> NormalizedPath {
        self.cache_dir
            .join("kv")
            .join(namespace)
            .join(format!("{}.bin", key.to_hex()))
    }

    /// Return the value for `(namespace, key)`, or `Ok(None)` on miss.
    pub fn get(&self, namespace: &str, key: &Key) -> KvResult<Option<Vec<u8>>> {
        check_namespace(namespace)?;
        let composite = composite(namespace, key);
        let txn = self.db.begin_read()?;
        let table = txn.open_table(KV_TABLE)?;
        let raw = match table.get(composite.as_str())? {
            Some(v) => v.value().to_vec(),
            None => return Ok(None),
        };
        let row: KvRow = bincode::deserialize(&raw)
            .map_err(|e| KvError::Corrupt(composite.clone(), format!("bincode: {e}")))?;
        if row.schema_version != SCHEMA_VERSION {
            return Err(KvError::Corrupt(
                composite,
                format!("schema_version={}", row.schema_version),
            ));
        }
        match row.body {
            KvBody::Inline(bytes) => Ok(Some(bytes)),
            KvBody::Spilled { len, blake3 } => {
                let path = self.spill_path(namespace, key);
                let bytes = std::fs::read(&path)?;
                if bytes.len() as u64 != len {
                    return Err(KvError::Corrupt(
                        composite,
                        format!(
                            "spill length mismatch: got {}, expected {}",
                            bytes.len(),
                            len
                        ),
                    ));
                }
                let actual = *::blake3::hash(&bytes).as_bytes();
                if actual != blake3 {
                    return Err(KvError::Corrupt(
                        composite,
                        "spill blake3 mismatch".to_string(),
                    ));
                }
                Ok(Some(bytes))
            }
        }
    }

    /// Last-writer-wins. Writes spill side files via tempfile + rename so a
    /// crash mid-write leaves no dangling state in the redb row.
    pub fn put(&self, namespace: &str, key: &Key, value: &[u8]) -> KvResult<usize> {
        check_namespace(namespace)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(KvError::TooLarge(value.len(), MAX_VALUE_BYTES));
        }
        let composite = composite(namespace, key);

        let body = if value.len() <= INLINE_THRESHOLD {
            // If a previous spill file exists for this key, drop it; we're
            // about to inline.
            let prev_path = self.spill_path(namespace, key);
            if prev_path.exists() {
                let _ = std::fs::remove_file(&prev_path);
            }
            KvBody::Inline(value.to_vec())
        } else {
            let path = self.spill_path(namespace, key);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Tempfile + rename so partial writes never become visible.
            let dir = path
                .parent()
                .expect("spill path always has a parent because we joined kv/<ns>/");
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            use std::io::Write;
            tmp.write_all(value)?;
            tmp.as_file().sync_all()?;
            tmp.persist(&path).map_err(|e| KvError::Io(e.error))?;
            let blake3 = *::blake3::hash(value).as_bytes();
            KvBody::Spilled {
                len: value.len() as u64,
                blake3,
            }
        };
        let row = KvRow {
            schema_version: SCHEMA_VERSION,
            body,
        };
        let bytes =
            bincode::serialize(&row).map_err(|e| KvError::Redb(format!("serialize row: {e}")))?;

        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(KV_TABLE)?;
            table.insert(composite.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(value.len())
    }

    /// Async wrapper for [`Self::put`] that runs redb commit/fsync and spill
    /// file I/O on Tokio's blocking pool.
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
        let composite = composite(namespace, key);

        // First read the row so we know whether to clean up a spill file.
        let txn = self.db.begin_write()?;
        let mut had_spill = None;
        {
            let mut table = txn.open_table(KV_TABLE)?;
            let removed = table.remove(composite.as_str())?;
            if let Some(existing) = removed {
                if let Ok(row) = bincode::deserialize::<KvRow>(existing.value()) {
                    if matches!(row.body, KvBody::Spilled { .. }) {
                        had_spill = Some(self.spill_path(namespace, key));
                    }
                }
            }
        }
        txn.commit()?;
        if let Some(path) = had_spill {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    /// Async wrapper for [`Self::remove`] that runs redb commit/fsync and spill
    /// cleanup on Tokio's blocking pool.
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
        let prefix = format!("{namespace}::");
        let mut to_remove: Vec<String> = Vec::new();
        {
            let txn = self.db.begin_read()?;
            let table = txn.open_table(KV_TABLE)?;
            for entry in table.iter()? {
                let (k, _v) = entry?;
                let k_str = k.value().to_string();
                if k_str.starts_with(&prefix) {
                    to_remove.push(k_str);
                }
            }
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(KV_TABLE)?;
            for k in &to_remove {
                table.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        // Remove the spill directory wholesale; this is correct because every
        // key under <ns>/ corresponds to an entry we just removed.
        let ns_dir = self.cache_dir.join("kv").join(namespace);
        if ns_dir.exists() {
            let _ = std::fs::remove_dir_all(&ns_dir);
        }
        Ok(())
    }

    /// Async wrapper for [`Self::clear_namespace`] that runs redb commit/fsync
    /// and namespace spill cleanup on Tokio's blocking pool.
    pub async fn clear_namespace_async(&self, namespace: &str) -> KvResult<()> {
        let store = self.clone();
        let namespace = namespace.to_string();
        tokio::task::spawn_blocking(move || store.clear_namespace(&namespace))
            .await
            .map_err(|e| KvError::BlockingJoin(e.to_string()))?
    }

    /// Sorted by hex-key. Returns `(key, value-len)` pairs.
    pub fn list_namespace(&self, namespace: &str) -> KvResult<Vec<(Key, u64)>> {
        check_namespace(namespace)?;
        let prefix = format!("{namespace}::");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(KV_TABLE)?;
        let mut out: Vec<(Key, u64)> = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let k_str = k.value().to_string();
            if let Some(hex) = k_str.strip_prefix(&prefix) {
                let key = Key::from_hex(hex)?;
                let row: KvRow = bincode::deserialize(v.value())
                    .map_err(|e| KvError::Corrupt(k_str.clone(), format!("bincode: {e}")))?;
                let len = match row.body {
                    KvBody::Inline(ref b) => b.len() as u64,
                    KvBody::Spilled { len, .. } => len,
                };
                out.push((key, len));
            }
        }
        out.sort_by(|a, b| a.0.to_hex().cmp(&b.0.to_hex()));
        Ok(out)
    }

    /// Sum of value lengths in `namespace`. Does not include redb overhead.
    pub fn namespace_bytes(&self, namespace: &str) -> KvResult<u64> {
        let entries = self.list_namespace(namespace)?;
        Ok(entries.iter().map(|(_, l)| *l).sum())
    }

    /// Sum of value lengths across every namespace.
    pub fn total_bytes(&self) -> KvResult<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(KV_TABLE)?;
        let mut total: u64 = 0;
        for entry in table.iter()? {
            let (_, v) = entry?;
            if let Ok(row) = bincode::deserialize::<KvRow>(v.value()) {
                total += match row.body {
                    KvBody::Inline(b) => b.len() as u64,
                    KvBody::Spilled { len, .. } => len,
                };
            }
        }
        Ok(total)
    }

    /// Per-namespace statistics. Returned namespaces are sorted lexically.
    pub fn stats(&self) -> KvResult<Vec<(String, u64)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(KV_TABLE)?;
        let mut by_ns: std::collections::BTreeMap<String, u64> = Default::default();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let k_str = k.value().to_string();
            let ns = match k_str.split_once("::") {
                Some((ns, _)) => ns.to_string(),
                None => continue,
            };
            if let Ok(row) = bincode::deserialize::<KvRow>(v.value()) {
                let len = match row.body {
                    KvBody::Inline(b) => b.len() as u64,
                    KvBody::Spilled { len, .. } => len,
                };
                *by_ns.entry(ns).or_insert(0) += len;
            }
        }
        Ok(by_ns.into_iter().collect())
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

    // ---- F1: round-trip across size boundaries ----
    #[test]
    fn f1_round_trip_sizes() {
        let (_d, s) = store();
        // Boundary-focused sizes; the 4-MiB / 64-MiB cases live in the
        // `--full` stress tests (`tests/kv_stress.rs`) so the default test
        // run stays fast on Windows where every redb commit is an fsync.
        let sizes = [
            0,
            1,
            100,
            INLINE_THRESHOLD - 1,
            INLINE_THRESHOLD,
            INLINE_THRESHOLD + 1,
            64 * 1024,
        ];
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
        // Removing again is OK.
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

    // ---- F6: list_namespace sorted, lengths correct ----
    #[test]
    fn f6_list_sorted_and_lengths() {
        let (_d, s) = store();
        let mut keys: Vec<Key> = (0u32..5).map(|i| key_from(&i.to_le_bytes())).collect();
        // Mix sizes: half inline, half spilled.
        for (i, k) in keys.iter().enumerate() {
            let n = if i % 2 == 0 {
                10
            } else {
                INLINE_THRESHOLD + 100
            };
            let val = vec![i as u8; n];
            s.put("ns", k, &val).unwrap();
        }
        let listed = s.list_namespace("ns").unwrap();
        assert_eq!(listed.len(), 5);
        keys.sort_by_key(|k| k.to_hex());
        for (i, (k, _)) in listed.iter().enumerate() {
            assert_eq!(k.to_hex(), keys[i].to_hex(), "list not sorted at {i}");
        }
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
    }

    // ---- F8: byte-exact inline/spill threshold ----
    #[test]
    fn f8_byte_exact_threshold() {
        let (d, s) = store();
        let inline_key = key_from(b"inline");
        let spill_key = key_from(b"spill");
        s.put("ns", &inline_key, &vec![1u8; INLINE_THRESHOLD])
            .unwrap();
        s.put("ns", &spill_key, &vec![2u8; INLINE_THRESHOLD + 1])
            .unwrap();

        let inline_path = d
            .path()
            .join("kv")
            .join("ns")
            .join(format!("{}.bin", inline_key.to_hex()));
        let spill_path = d
            .path()
            .join("kv")
            .join("ns")
            .join(format!("{}.bin", spill_key.to_hex()));
        assert!(!inline_path.exists(), "inline value must NOT spill");
        assert!(spill_path.exists(), "spill threshold + 1 must spill");
        assert_eq!(
            std::fs::metadata(&spill_path).unwrap().len(),
            (INLINE_THRESHOLD + 1) as u64
        );
    }

    // ---- F9: tampered spill file → Corrupt ----
    #[test]
    fn f9_tampered_spill_detected() {
        let (d, s) = store();
        let k = key_from(b"corrupt");
        s.put("ns", &k, &vec![7u8; INLINE_THRESHOLD + 100]).unwrap();
        let path = d
            .path()
            .join("kv")
            .join("ns")
            .join(format!("{}.bin", k.to_hex()));
        // Tamper one byte.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let err = s.get("ns", &k).unwrap_err();
        assert!(matches!(err, KvError::Corrupt(_, _)), "got {err:?}");
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
        let k2 = Key::from_hex(&hex).unwrap();
        assert_eq!(k, k2);

        // Upper-case is accepted (case-insensitive parse).
        let upper = hex.to_ascii_uppercase();
        let k3 = Key::from_hex(&upper).unwrap();
        assert_eq!(k, k3);

        // Bad inputs.
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
        let (_d, s) = store();
        let k = key_from(b"sv");
        // Write a row by hand with a forged schema_version.
        let row = KvRow {
            schema_version: SCHEMA_VERSION + 1,
            body: KvBody::Inline(b"hi".to_vec()),
        };
        let bytes = bincode::serialize(&row).unwrap();
        let composite_key = composite("ns", &k);
        let txn = s.db.begin_write().unwrap();
        {
            let mut t = txn.open_table(KV_TABLE).unwrap();
            t.insert(composite_key.as_str(), bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        let err = s.get("ns", &k).unwrap_err();
        match err {
            KvError::Corrupt(_, msg) => assert!(msg.contains("schema_version="), "msg={msg}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
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

    // ---- I5/I6: max value bytes (allocates 64 MiB, runs only under --full) ----
    #[test]
    #[ignore = "allocates 64 MiB; see tests/kv_stress.rs for max-cap coverage"]
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

    // ---- D3 / P8: reopen sees prior writes ----
    #[test]
    fn p8_reopen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let k = key_from(b"persist");
        {
            let s = KvStore::open(dir.path()).unwrap();
            s.put("ns", &k, &vec![3u8; INLINE_THRESHOLD + 10]).unwrap();
        }
        let s = KvStore::open(dir.path()).unwrap();
        let got = s.get("ns", &k).unwrap().unwrap();
        assert_eq!(got, vec![3u8; INLINE_THRESHOLD + 10]);
    }

    // ---- P3: case-insensitive key parsing means UPPER and lower collide ----
    #[test]
    fn p3_case_insensitive_key_parses_to_same_key() {
        let h = ::blake3::hash(b"x");
        let k = Key::from_hash(h);
        let lower = k.to_hex();
        let upper = lower.to_ascii_uppercase();
        let k_lower = Key::from_hex(&lower).unwrap();
        let k_upper = Key::from_hex(&upper).unwrap();
        assert_eq!(k_lower, k_upper);
    }

    // ---- I8: put then reopen with fresh KvStore reads back ----
    #[test]
    fn i8_durability_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        let k = key_from(b"d");
        {
            let s = KvStore::open(dir.path()).unwrap();
            s.put("ns", &k, b"durable").unwrap();
        }
        let s = KvStore::open(dir.path()).unwrap();
        assert_eq!(s.get("ns", &k).unwrap().unwrap(), b"durable");
    }
}
