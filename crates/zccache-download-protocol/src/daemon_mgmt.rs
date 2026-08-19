//! Shared daemon-management utilities used by both the download client and
//! the download daemon binary (lock-file helpers, default endpoint, etc.).

use zccache_core::NormalizedPath;

/// Return the default IPC endpoint for the download daemon.
pub fn default_endpoint() -> String {
    // The daemon state directory includes the cache protocol version and
    // namespace for both the default root and an explicit cache override.
    // Deriving IPC identity from it prevents a freshly upgraded client from
    // connecting to an older download daemon before it can self-deploy the
    // matching multicall executable.
    endpoint_for_cache_dir(zccache_core::config::daemon_state_dir().as_path())
}

fn endpoint_for_cache_dir(cache_dir: &std::path::Path) -> String {
    let file_path = cache_dir
        .join("download-daemon.sock")
        .to_string_lossy()
        .into_owned();
    let suffix = zccache_core::stable_path_id(cache_dir);
    crate::platform::ipc::Endpoint::select(file_path, format!("zccache-download-{suffix}"))
        .to_string()
}

/// Path to the daemon PID lock file.
pub fn lock_file_path() -> NormalizedPath {
    zccache_core::config::daemon_state_dir().join("download-daemon.lock")
}

/// Write the daemon PID to the lock file.
pub fn write_lock_file(pid: u32) -> Result<(), std::io::Error> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pid.to_string())
}

/// Remove the daemon lock file (best-effort).
pub fn remove_lock_file() {
    let _ = std::fs::remove_file(lock_file_path());
}

/// Read the PID stored in the daemon lock file, if it exists and is valid.
pub fn read_lock_file_pid() -> Option<u32> {
    std::fs::read_to_string(lock_file_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_cache_dir(value: &std::path::Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let previous = std::env::var_os(zccache_core::config::CACHE_DIR_ENV);
            std::env::set_var(zccache_core::config::CACHE_DIR_ENV, value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(zccache_core::config::CACHE_DIR_ENV, value),
                None => std::env::remove_var(zccache_core::config::CACHE_DIR_ENV),
            }
        }
    }

    #[test]
    fn cache_dir_override_moves_download_endpoint_and_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&cache_dir);

        // Issue #761 / #762 Phase 0: the env var pins the *top-level* root;
        // every state file (including this lock file) lives under
        // `<top-level>/v<VERSION>/`.
        let versioned = cache_dir.join(zccache_core::config::versioned_subdir());

        let endpoint = default_endpoint();
        let expected = crate::platform::ipc::Endpoint::select(
            versioned
                .join("download-daemon.sock")
                .to_string_lossy()
                .into_owned(),
            format!(
                "zccache-download-{}",
                zccache_core::stable_path_id(&versioned)
            ),
        );
        assert_eq!(endpoint, expected.as_str());

        assert_eq!(lock_file_path(), versioned.join("download-daemon.lock"));
    }

    #[test]
    fn different_version_roots_have_different_download_endpoints() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("v1.0.0");
        let new = root.path().join("v2.0.0");

        assert_ne!(endpoint_for_cache_dir(&old), endpoint_for_cache_dir(&new));
    }
}
