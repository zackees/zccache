use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use zccache_core::NormalizedPath;

// fs2::FileExt trait methods are called via UFCS below to avoid
// ambiguity with std::fs::File inherent methods added in Rust 1.89.

use crate::error::Result;

const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

fn lock_path(cache_path: &Path) -> NormalizedPath {
    let mut s = cache_path.as_os_str().to_os_string();
    s.push(".lock");
    NormalizedPath::new(Path::new(&s))
}

fn open_lock_file(cache_path: &Path) -> io::Result<File> {
    let path = lock_path(cache_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
}

// Use UFCS to call fs2 trait methods, avoiding ambiguity with
// std::fs::File inherent methods added in Rust 1.89.

fn try_lock_shared(file: &File) -> io::Result<()> {
    fs2::FileExt::try_lock_shared(file)
}

fn try_lock_exclusive(file: &File) -> io::Result<()> {
    fs2::FileExt::try_lock_exclusive(file)
}

fn acquire_shared(file: &File, timeout: Duration) -> bool {
    if try_lock_shared(file).is_ok() {
        return true;
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(RETRY_INTERVAL);
        if try_lock_shared(file).is_ok() {
            return true;
        }
    }
    false
}

fn acquire_exclusive(file: &File, timeout: Duration) -> bool {
    if try_lock_exclusive(file).is_ok() {
        return true;
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(RETRY_INTERVAL);
        if try_lock_exclusive(file).is_ok() {
            return true;
        }
    }
    false
}

/// Run a closure while holding a shared (read) lock on the cache path.
/// Fail-open: if the lock cannot be acquired, the closure runs anyway.
pub fn with_shared_lock<T, F>(cache_path: &Path, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let lock_file = match open_lock_file(cache_path) {
        Ok(file) => Some(file),
        Err(e) => {
            tracing::warn!(
                path = %cache_path.display(),
                error = %e,
                "failed to open lock file, proceeding without lock"
            );
            None
        }
    };

    let _acquired = lock_file.as_ref().map(|file| {
        if !acquire_shared(file, DEFAULT_LOCK_TIMEOUT) {
            tracing::warn!(
                path = %cache_path.display(),
                "shared lock timeout after {}s, proceeding without lock",
                DEFAULT_LOCK_TIMEOUT.as_secs()
            );
        }
    });

    let result = f();

    // Lock released on File drop.
    drop(lock_file);

    result
}

/// Run a closure while holding an exclusive (write) lock on the cache path.
/// Fail-open: if the lock cannot be acquired, the closure runs anyway.
pub fn with_exclusive_lock<T, F>(cache_path: &Path, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let lock_file = match open_lock_file(cache_path) {
        Ok(file) => Some(file),
        Err(e) => {
            tracing::warn!(
                path = %cache_path.display(),
                error = %e,
                "failed to open lock file, proceeding without lock"
            );
            None
        }
    };

    let _acquired = lock_file.as_ref().map(|file| {
        if !acquire_exclusive(file, DEFAULT_LOCK_TIMEOUT) {
            tracing::warn!(
                path = %cache_path.display(),
                "exclusive lock timeout after {}s, proceeding without lock",
                DEFAULT_LOCK_TIMEOUT.as_secs()
            );
        }
    });

    let result = f();

    // Lock released on File drop.
    drop(lock_file);

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lock_file_created_next_to_cache() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");

        with_shared_lock(&cache, || Ok(())).unwrap();

        assert!(lock_path(&cache).exists());
    }

    #[test]
    fn shared_locks_concurrent() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");

        let file1 = open_lock_file(&cache).unwrap();
        let file2 = open_lock_file(&cache).unwrap();

        assert!(try_lock_shared(&file1).is_ok());
        assert!(try_lock_shared(&file2).is_ok());
    }

    #[test]
    fn exclusive_blocks_shared() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("fp.json");

        let exclusive = open_lock_file(&cache_path).unwrap();
        assert!(try_lock_exclusive(&exclusive).is_ok());

        let cache_path2 = cache_path.clone();
        let barrier = Arc::new(Barrier::new(2));
        let barrier2 = barrier.clone();

        let handle = std::thread::spawn(move || {
            let file = open_lock_file(&cache_path2).unwrap();
            barrier2.wait();
            // Should fail to acquire shared lock immediately.
            assert!(try_lock_shared(&file).is_err());
        });

        barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        drop(exclusive); // Release.
        handle.join().unwrap();
    }

    #[test]
    fn exclusive_blocks_exclusive() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");

        let file1 = open_lock_file(&cache).unwrap();
        assert!(try_lock_exclusive(&file1).is_ok());

        let file2 = open_lock_file(&cache).unwrap();
        assert!(try_lock_exclusive(&file2).is_err());

        drop(file1);
        assert!(try_lock_exclusive(&file2).is_ok());
    }

    #[test]
    fn fail_open_on_timeout() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");

        let result: Result<i32> = with_shared_lock(&cache, || Ok(42));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn lock_parent_dir_created() {
        let dir = TempDir::new().unwrap();
        let deep = dir.path().join("a/b/c/fp.json");

        with_exclusive_lock(&deep, || Ok(())).unwrap();

        assert!(lock_path(&deep).exists());
    }

    #[test]
    fn lock_released_on_drop() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");

        {
            let file = open_lock_file(&cache).unwrap();
            assert!(try_lock_exclusive(&file).is_ok());
        }

        let file = open_lock_file(&cache).unwrap();
        assert!(try_lock_exclusive(&file).is_ok());
    }
}
