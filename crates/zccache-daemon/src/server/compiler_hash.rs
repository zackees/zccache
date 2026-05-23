//! Compiler executable hash memoization.
//!
//! Caches `(mtime, size) -> ContentHash` for compiler binaries to skip
//! the per-request blake3 over multi-MB executables.

use super::*;

#[derive(Clone)]
pub(super) struct CompilerHashEntry {
    pub(super) mtime: std::time::SystemTime,
    pub(super) size: u64,
    pub(super) hash: ContentHash,
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

    pub(super) fn get_or_hash(&self, path: &Path) -> Option<ContentHash> {
        self.get_or_hash_with(path, |path| zccache_hash::hash_file(path).ok())
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
}
