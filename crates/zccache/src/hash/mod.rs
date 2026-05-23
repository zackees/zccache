//! Hashing utilities for zccache.
//!
//! Provides blake3-based content hashing and cache key computation.

pub mod cache_key;
pub mod link_cache_key;

use std::io::Read;
use std::path::Path;

/// A 32-byte blake3 hash digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Create a `ContentHash` from raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the hash as a hex string.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// Returns the raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the first N bytes for directory sharding.
    ///
    /// # Panics
    ///
    /// Panics if `levels * bytes_per_level > 32` (exceeds hash size).
    #[must_use]
    pub fn shard_prefix(&self, levels: usize, bytes_per_level: usize) -> Vec<String> {
        let hex = self.to_hex();
        let chars_per_level = bytes_per_level * 2;
        let required = levels * chars_per_level;
        assert!(
            required <= hex.len(),
            "shard_prefix: levels={levels} * bytes_per_level={bytes_per_level} \
             requires {required} hex chars but hash is only {} chars",
            hex.len()
        );
        (0..levels)
            .map(|i| {
                let start = i * chars_per_level;
                let end = start + chars_per_level;
                hex[start..end].to_string()
            })
            .collect()
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Hash the contents of a byte slice.
#[must_use]
pub fn hash_bytes(data: &[u8]) -> ContentHash {
    let hash = blake3::hash(data);
    ContentHash(*hash.as_bytes())
}

/// Incremental hasher for building a `ContentHash` from multiple updates.
///
/// Avoids allocating an intermediate buffer when the input is spread across
/// multiple slices (e.g., request fingerprinting).
pub struct StreamHasher(blake3::Hasher);

impl StreamHasher {
    /// Create a new streaming hasher.
    #[must_use]
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Feed bytes into the hasher.
    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.0.update(data);
        self
    }

    /// Finalize and return the hash.
    #[must_use]
    pub fn finalize(self) -> ContentHash {
        ContentHash(*self.0.finalize().as_bytes())
    }
}

impl Default for StreamHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash the contents of a reader.
///
/// # Errors
///
/// Returns an error if reading from the reader fails.
pub fn hash_reader<R: Read>(mut reader: R) -> std::io::Result<ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(ContentHash(*hasher.finalize().as_bytes()))
}

/// Hash the contents of a file using memory mapping.
///
/// Uses `memmap2` for zero-copy file access. The OS page cache ensures
/// files recently read (e.g., during compilation) are hashed from memory,
/// not disk. Falls back to buffered reading for empty files.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
///
/// # Safety
///
/// Memory mapping is technically unsafe if another process modifies the file
/// concurrently. The TOCTOU check in `MetadataCache::hash_and_insert` detects
/// this by comparing stat before and after hashing.
pub fn hash_file(path: &Path) -> std::io::Result<ContentHash> {
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;

    if meta.len() == 0 {
        return Ok(hash_bytes(b""));
    }

    // SAFETY: The caller (MetadataCache::hash_and_insert) stats before and
    // after hashing to detect concurrent modification.
    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    Ok(hash_bytes(&mmap))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_deterministic() {
        let h1 = hash_bytes(b"hello world");
        let h2 = hash_bytes(b"hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_different_inputs() {
        let h1 = hash_bytes(b"hello");
        let h2 = hash_bytes(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hex_roundtrip() {
        let h = hash_bytes(b"test");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn shard_prefix_works() {
        let h = hash_bytes(b"test");
        let shards = h.shard_prefix(2, 1);
        assert_eq!(shards.len(), 2);
        assert_eq!(shards[0].len(), 2);
        assert_eq!(shards[1].len(), 2);
    }

    #[test]
    fn shard_prefix_max_valid() {
        // 32 bytes = 64 hex chars. 32 levels of 1 byte each uses all 64 chars.
        let h = hash_bytes(b"test");
        let shards = h.shard_prefix(32, 1);
        assert_eq!(shards.len(), 32);
    }

    #[test]
    #[should_panic(expected = "shard_prefix")]
    fn shard_prefix_overflow_panics() {
        // Bug: shard_prefix(33, 1) would index past the 64-char hex string,
        // causing an opaque "index out of bounds" panic. Now panics with a
        // descriptive message.
        let h = hash_bytes(b"test");
        let _ = h.shard_prefix(33, 1);
    }

    #[test]
    #[should_panic(expected = "shard_prefix")]
    fn shard_prefix_large_bytes_per_level_panics() {
        let h = hash_bytes(b"test");
        // 2 levels of 17 bytes each = 34 bytes > 32 hash bytes.
        let _ = h.shard_prefix(2, 17);
    }
}
