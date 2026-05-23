//! Shared daemon-management utilities used by both the download client and
//! the download daemon binary (lock-file helpers, default endpoint, etc.).

use crate::core::NormalizedPath;

/// Return the default IPC endpoint for the download daemon.
pub fn default_endpoint() -> String {
    if let Some(cache_dir) = crate::core::config::cache_dir_override() {
        return endpoint_for_cache_dir(&cache_dir);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{runtime_dir}/zccache-download/sock");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-download-{user}/sock")
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        format!(r"\\.\pipe\zccache-download-{username}")
    }
}

fn endpoint_for_cache_dir(cache_dir: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        cache_dir
            .join("download-daemon.sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        let suffix = crate::core::stable_path_id(cache_dir);
        format!(r"\\.\pipe\zccache-download-{suffix}")
    }
}

/// Path to the daemon PID lock file.
pub fn lock_file_path() -> NormalizedPath {
    crate::core::config::default_cache_dir().join("download-daemon.lock")
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
            let previous = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
            std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(crate::core::config::CACHE_DIR_ENV, value),
                None => std::env::remove_var(crate::core::config::CACHE_DIR_ENV),
            }
        }
    }

    #[test]
    fn cache_dir_override_moves_download_endpoint_and_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&cache_dir);

        let endpoint = default_endpoint();
        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir
                .join("download-daemon.sock")
                .to_string_lossy()
                .into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-download-"));
            assert!(endpoint.ends_with(&crate::core::stable_path_id(&cache_dir)));
        }

        assert_eq!(lock_file_path(), cache_dir.join("download-daemon.lock"));
    }
}
