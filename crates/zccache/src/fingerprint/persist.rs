use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;
use zccache::core::NormalizedPath;

use serde::{Deserialize, Serialize};

use super::error::Result;
use super::file_lock;

const CACHE_VERSION: u32 = 1;

/// Per-file cached metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File modification time as nanoseconds since UNIX epoch.
    pub mtime_ns: u64,
    /// File size in bytes.
    pub size: u64,
    /// blake3 content hash as hex string.
    pub hash: String,
}

/// On-disk format for `TwoLayerCache`.
#[derive(Debug, Serialize, Deserialize)]
pub struct TwoLayerData {
    pub version: u32,
    pub status: String,
    pub timestamp_ns: u64,
    pub files: BTreeMap<String, FileEntry>,
    /// Maximum source file mtime at the time of the last check.
    /// Used by the dir-mtime fast-path to skip re-scanning.
    #[serde(default)]
    pub max_source_mtime_ns: u64,
}

/// On-disk format for `HashCache`.
#[derive(Debug, Serialize, Deserialize)]
pub struct HashCacheData {
    pub version: u32,
    pub hash: String,
    pub status: String,
    pub timestamp_ns: u64,
    pub file_count: usize,
    /// Maximum source file mtime at the time of the last check.
    /// Used by the dir-mtime fast-path to skip re-scanning.
    #[serde(default)]
    pub max_source_mtime_ns: u64,
}

impl TwoLayerData {
    pub fn new(status: &str, files: BTreeMap<String, FileEntry>) -> Self {
        let max_source_mtime_ns = files.values().map(|e| e.mtime_ns).max().unwrap_or(0);
        Self {
            version: CACHE_VERSION,
            status: status.to_string(),
            timestamp_ns: now_ns(),
            files,
            max_source_mtime_ns,
        }
    }
}

impl HashCacheData {
    pub fn new(hash: String, status: &str, file_count: usize) -> Self {
        Self {
            version: CACHE_VERSION,
            hash,
            status: status.to_string(),
            timestamp_ns: now_ns(),
            file_count,
            max_source_mtime_ns: 0,
        }
    }

    pub fn with_max_mtime(
        hash: String,
        status: &str,
        file_count: usize,
        max_source_mtime_ns: u64,
    ) -> Self {
        Self {
            version: CACHE_VERSION,
            hash,
            status: status.to_string(),
            timestamp_ns: now_ns(),
            file_count,
            max_source_mtime_ns,
        }
    }
}

// ── Public API (locked) ──────────────────────────────────────────

/// Read a JSON cache file. Returns `None` if missing or corrupt (fail-open).
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    file_lock::with_shared_lock(path, || read_json_inner(path))
}

/// Write data to a path atomically (write `.tmp` + rename).
pub fn write_atomic<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    file_lock::with_exclusive_lock(path, || write_atomic_inner(path, data))
}

/// Write data to the `.pending` sibling of a cache file.
pub fn write_pending<T: Serialize>(cache_path: &Path, data: &T) -> Result<()> {
    // Acquire lock on the cache path once; call inner write_atomic to avoid
    // double-locking (write_pending → write_atomic would deadlock otherwise).
    file_lock::with_exclusive_lock(cache_path, || {
        let pending = pending_path(cache_path);
        write_atomic_inner(&pending, data)
    })
}

/// Read the `.pending` sibling of a cache file.
pub fn read_pending<T: serde::de::DeserializeOwned>(cache_path: &Path) -> Result<Option<T>> {
    file_lock::with_shared_lock(cache_path, || read_json_inner(&pending_path(cache_path)))
}

/// Detect which cache type wrote a pending file.
///
/// Returns `"hash"` for [`HashCacheData`] or `"two-layer"` for [`TwoLayerData`],
/// based on discriminating JSON keys. Returns `None` if the pending file
/// doesn't exist or can't be identified.
pub fn detect_pending_type(cache_path: &Path) -> Option<&'static str> {
    file_lock::with_shared_lock(cache_path, || {
        let pending = pending_path(cache_path);
        let content = match std::fs::read_to_string(&pending) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        let value: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let obj = match value.as_object() {
            Some(o) => o,
            None => return Ok(None),
        };
        if obj.contains_key("hash") && obj.contains_key("file_count") {
            Ok(Some("hash"))
        } else if obj.contains_key("files") {
            Ok(Some("two-layer"))
        } else {
            Ok(None)
        }
    })
    .ok()
    .flatten()
}

/// Promote `.pending` → cache file (atomic rename).
pub fn promote_pending(cache_path: &Path) -> Result<()> {
    file_lock::with_exclusive_lock(cache_path, || promote_pending_inner(cache_path))
}

/// Delete cache and pending files.
pub fn remove_cache(cache_path: &Path) {
    // Best-effort lock; ignore errors since remove_cache already ignores I/O errors.
    let _ = file_lock::with_exclusive_lock(cache_path, || {
        remove_cache_inner(cache_path);
        Ok(())
    });
}

// ── Inner implementations (no locking) ───────────────────────────

fn read_json_inner<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match serde_json::from_str(&content) {
        Ok(data) => Ok(Some(data)),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "corrupt fingerprint cache, treating as empty"
            );
            Ok(None)
        }
    }
}

fn write_atomic_inner<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let json = serde_json::to_string(data)?;
    let tmp = tmp_path(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, json.as_bytes())?;
    // Windows: remove target before rename (rename doesn't overwrite on Windows).
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn promote_pending_inner(cache_path: &Path) -> Result<()> {
    let pending = pending_path(cache_path);
    if !pending.exists() {
        return Ok(());
    }
    let _ = std::fs::remove_file(cache_path);
    std::fs::rename(&pending, cache_path)?;
    Ok(())
}

fn remove_cache_inner(cache_path: &Path) {
    let _ = std::fs::remove_file(cache_path);
    let _ = std::fs::remove_file(pending_path(cache_path));
    let _ = std::fs::remove_file(tmp_path(cache_path));
}

// ── Path helpers ─────────────────────────────────────────────────

fn pending_path(cache_path: &Path) -> NormalizedPath {
    cache_path.with_extension("pending").into()
}

fn tmp_path(path: &Path) -> NormalizedPath {
    cache_path_with_suffix(path, ".tmp")
}

fn cache_path_with_suffix(path: &Path, suffix: &str) -> NormalizedPath {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    NormalizedPath::new(Path::new(&s))
}

// ── Utilities ────────────────────────────────────────────────────

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub fn mtime_ns(path: &Path) -> std::io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    Ok(meta
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64)
}

pub fn file_size(path: &Path) -> std::io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Return the maximum mtime (in nanoseconds) across a set of files.
/// Returns 0 if the file list is empty.
pub fn max_mtime_ns(files: &[super::scan::ScannedFile]) -> std::io::Result<u64> {
    let mut max = 0u64;
    for file in files {
        let mt = mtime_ns(&file.absolute)?;
        if mt > max {
            max = mt;
        }
    }
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_hash_cache_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");

        let data = HashCacheData::new("abcdef".to_string(), "success", 42);
        write_atomic(&path, &data).unwrap();

        let loaded: HashCacheData = read_json(&path).unwrap().unwrap();
        assert_eq!(loaded.hash, "abcdef");
        assert_eq!(loaded.status, "success");
        assert_eq!(loaded.file_count, 42);
        assert_eq!(loaded.version, CACHE_VERSION);
    }

    #[test]
    fn roundtrip_two_layer_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("two.json");

        let mut files = BTreeMap::new();
        files.insert(
            "src/main.rs".to_string(),
            FileEntry {
                mtime_ns: 123_456_789,
                size: 100,
                hash: "aabb".to_string(),
            },
        );
        let data = TwoLayerData::new("success", files);
        write_atomic(&path, &data).unwrap();

        let loaded: TwoLayerData = read_json(&path).unwrap().unwrap();
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(loaded.files["src/main.rs"].hash, "aabb");
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.json");
        let result: Option<HashCacheData> = read_json(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn corrupt_json_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json{{{").unwrap();

        let result: Option<HashCacheData> = read_json(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn pending_write_and_promote() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("cache.json");
        let pending = pending_path(&cache);

        let data = HashCacheData::new("ff00".to_string(), "success", 5);
        write_pending(&cache, &data).unwrap();

        // Pending file should exist, cache file should not.
        assert!(pending.exists());
        assert!(!cache.exists());

        promote_pending(&cache).unwrap();

        // Now cache file should exist, pending should not.
        assert!(cache.exists());
        assert!(!pending.exists());

        let loaded: HashCacheData = read_json(&cache).unwrap().unwrap();
        assert_eq!(loaded.hash, "ff00");
    }

    #[test]
    fn atomic_write_no_leftover_tmp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.json");
        let tmp = tmp_path(&path);

        let data = HashCacheData::new("xx".to_string(), "success", 1);
        write_atomic(&path, &data).unwrap();

        assert!(path.exists());
        assert!(!tmp.exists());
    }

    #[test]
    fn remove_cache_cleans_all() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("c.json");

        let data = HashCacheData::new("x".to_string(), "success", 1);
        write_atomic(&cache, &data).unwrap();
        write_pending(&cache, &data).unwrap();

        assert!(cache.exists());
        assert!(pending_path(&cache).exists());

        remove_cache(&cache);

        assert!(!cache.exists());
        assert!(!pending_path(&cache).exists());
    }

    // ── Adversarial tests ─────────────────────────────────────────

    #[test]
    fn empty_json_object_returns_none_for_typed() {
        // Valid JSON but wrong schema → fail-open.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad_schema.json");
        std::fs::write(&path, "{}").unwrap();

        let result: Option<HashCacheData> = read_json(&path).unwrap();
        // serde will fail because required fields are missing.
        assert!(result.is_none());
    }

    #[test]
    fn truncated_json_returns_none() {
        // Simulate a crash mid-write that leaves truncated JSON.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trunc.json");
        std::fs::write(&path, r#"{"version": 1, "hash": "abc"#).unwrap();

        let result: Option<HashCacheData> = read_json(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn promote_pending_noop_when_no_pending() {
        // Should not error when there's no pending file.
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("no_pending.json");
        promote_pending(&cache).unwrap();
        assert!(!cache.exists());
    }

    #[test]
    fn write_atomic_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("over.json");

        let data1 = HashCacheData::new("first".to_string(), "success", 1);
        write_atomic(&path, &data1).unwrap();

        let data2 = HashCacheData::new("second".to_string(), "success", 2);
        write_atomic(&path, &data2).unwrap();

        let loaded: HashCacheData = read_json(&path).unwrap().unwrap();
        assert_eq!(loaded.hash, "second");
        assert_eq!(loaded.file_count, 2);
    }

    #[test]
    fn write_atomic_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c/deep.json");

        let data = HashCacheData::new("deep".to_string(), "success", 1);
        write_atomic(&path, &data).unwrap();

        let loaded: HashCacheData = read_json(&path).unwrap().unwrap();
        assert_eq!(loaded.hash, "deep");
    }

    #[test]
    fn remove_cache_noop_when_nothing_exists() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("ghost.json");
        // Should not panic or error.
        remove_cache(&cache);
    }

    #[test]
    fn pending_path_replaces_extension() {
        // Verify the pending path convention.
        let cache = NormalizedPath::from("/tmp/fp.json");
        let pending = pending_path(&cache);
        assert_eq!(pending, NormalizedPath::from("/tmp/fp.pending"));
    }

    #[test]
    fn read_pending_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("no.json");
        let result: Option<HashCacheData> = read_pending(&cache).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn file_entry_serialization_roundtrip() {
        let entry = FileEntry {
            mtime_ns: u64::MAX,
            size: 0,
            hash: "a".repeat(64),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FileEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mtime_ns, u64::MAX);
        assert_eq!(back.size, 0);
        assert_eq!(back.hash, "a".repeat(64));
    }

    #[test]
    fn now_ns_is_positive() {
        assert!(now_ns() > 0);
    }

    #[test]
    fn mtime_ns_of_real_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let mtime = mtime_ns(&path).unwrap();
        assert!(mtime > 0);
    }

    #[test]
    fn file_size_of_known_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sized.txt");
        std::fs::write(&path, "12345").unwrap();

        assert_eq!(file_size(&path).unwrap(), 5);
    }

    #[test]
    fn file_size_of_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();

        assert_eq!(file_size(&path).unwrap(), 0);
    }

    #[test]
    fn wrong_type_json_returns_none() {
        // JSON array instead of object.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("array.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();

        let result: Option<HashCacheData> = read_json(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn detect_pending_type_hash() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");
        let data = HashCacheData::new("abc".to_string(), "pending", 5);
        write_pending(&cache, &data).unwrap();
        assert_eq!(detect_pending_type(&cache), Some("hash"));
    }

    #[test]
    fn detect_pending_type_two_layer() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");
        let data = TwoLayerData::new("pending", BTreeMap::new());
        write_pending(&cache, &data).unwrap();
        assert_eq!(detect_pending_type(&cache), Some("two-layer"));
    }

    #[test]
    fn detect_pending_type_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");
        assert_eq!(detect_pending_type(&cache), None);
    }

    #[test]
    fn detect_pending_type_none_when_corrupt() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");
        let pending = cache.with_extension("pending");
        std::fs::write(&pending, "not json").unwrap();
        assert_eq!(detect_pending_type(&cache), None);
    }

    #[test]
    fn write_atomic_produces_compact_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("compact.json");
        let data = HashCacheData::new("abc".to_string(), "success", 5);
        write_atomic(&path, &data).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains('\n'),
            "expected compact JSON, got newlines: {content}"
        );
    }

    #[test]
    fn extra_fields_in_json_ignored() {
        // Forward compatibility: extra fields should be silently ignored.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("extra.json");
        std::fs::write(
            &path,
            r#"{"version":1,"hash":"xx","status":"success","timestamp_ns":0,"file_count":1,"extra_field":"ignored"}"#,
        )
        .unwrap();

        let loaded: Option<HashCacheData> = read_json(&path).unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().hash, "xx");
    }
}
