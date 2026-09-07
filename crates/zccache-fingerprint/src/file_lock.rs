use kernal_api::platform::fs::FileLock;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use zccache_core::NormalizedPath;

use super::error::Result;

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

// The facade returns a guard that unlocks on drop, so these return the guard
// rather than a bool: the lock's lifetime is now the guard's, and a caller
// that dropped it on the floor would release the lock immediately. The
// callers below bind it for exactly as long as they held the `File` before.

fn acquire_shared(file: &File, timeout: Duration) -> Option<FileLock<'_>> {
    if let Ok(guard) = kernal_api::platform::fs::try_lock_shared(file) {
        return Some(guard);
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(RETRY_INTERVAL);
        if let Ok(guard) = kernal_api::platform::fs::try_lock_shared(file) {
            return Some(guard);
        }
    }
    None
}

fn acquire_exclusive(file: &File, timeout: Duration) -> Option<FileLock<'_>> {
    if let Ok(guard) = kernal_api::platform::fs::try_lock_exclusive(file) {
        return Some(guard);
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(RETRY_INTERVAL);
        if let Ok(guard) = kernal_api::platform::fs::try_lock_exclusive(file) {
            return Some(guard);
        }
    }
    None
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

    let acquired = lock_file.as_ref().and_then(|file| {
        let guard = acquire_shared(file, DEFAULT_LOCK_TIMEOUT);
        if guard.is_none() {
            tracing::warn!(
                path = %cache_path.display(),
                "shared lock timeout after {}s, proceeding without lock",
                DEFAULT_LOCK_TIMEOUT.as_secs()
            );
        }
        guard
    });

    let result = f();

    // The guard borrows the file, so it is released first; the lock is gone
    // by the time the handle closes, as it was before.
    drop(acquired);
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

    let acquired = lock_file.as_ref().and_then(|file| {
        let guard = acquire_exclusive(file, DEFAULT_LOCK_TIMEOUT);
        if guard.is_none() {
            tracing::warn!(
                path = %cache_path.display(),
                "exclusive lock timeout after {}s, proceeding without lock",
                DEFAULT_LOCK_TIMEOUT.as_secs()
            );
        }
        guard
    });

    let result = f();

    // The guard borrows the file, so it is released first; the lock is gone
    // by the time the handle closes, as it was before.
    drop(acquired);
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

        // Both guards are bound: dropping the first inline would make the
        // second succeed trivially, and the point is that they coexist.
        let _first =
            kernal_api::platform::fs::try_lock_shared(&file1).expect("first shared holder");
        let _second = kernal_api::platform::fs::try_lock_shared(&file2)
            .expect("a second shared holder must be allowed alongside the first");
    }

    #[test]
    fn exclusive_blocks_shared() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join("fp.json");

        let exclusive = open_lock_file(&cache_path).unwrap();
        // Bind the guard: it releases on drop, so asserting on it inline
        // would unlock before the assertion's semicolon.
        let exclusive_guard = kernal_api::platform::fs::try_lock_exclusive(&exclusive)
            .expect("exclusive lock must be available");

        let cache_path2 = cache_path.clone();
        let barrier = Arc::new(Barrier::new(2));
        let barrier2 = barrier.clone();

        let handle = std::thread::spawn(move || {
            let file = open_lock_file(&cache_path2).unwrap();
            barrier2.wait();
            // Should fail to acquire shared lock immediately.
            assert!(kernal_api::platform::fs::try_lock_shared(&file).is_err());
        });

        barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        drop(exclusive_guard); // Release.
        drop(exclusive);
        handle.join().unwrap();
    }

    #[test]
    fn exclusive_blocks_exclusive() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("fp.json");

        let file1 = open_lock_file(&cache).unwrap();
        let first = kernal_api::platform::fs::try_lock_exclusive(&file1)
            .expect("first holder takes the lock");

        let file2 = open_lock_file(&cache).unwrap();
        assert!(kernal_api::platform::fs::try_lock_exclusive(&file2).is_err());

        drop(first);
        drop(file1);
        drop(
            kernal_api::platform::fs::try_lock_exclusive(&file2)
                .expect("the lock is free once the first holder releases"),
        );
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
            // Held for the whole inner scope, so what the outer assertion
            // observes is the *drop* releasing it rather than the lock never
            // having been taken.
            let _held = kernal_api::platform::fs::try_lock_exclusive(&file)
                .expect("exclusive lock must be available");
        }

        let file = open_lock_file(&cache).unwrap();
        drop(
            kernal_api::platform::fs::try_lock_exclusive(&file)
                .expect("the lock must be free once the previous holder's scope ended"),
        );
    }
}
