//! IPC transport layer for zccache.
//!
//! Provides platform-abstracted IPC between CLI/compiler wrapper
//! and the daemon, using Unix domain sockets on Unix and named
//! pipes on Windows.

#![allow(clippy::missing_errors_doc)]

pub mod error;
pub mod transport;

pub use error::IpcError;
#[cfg(windows)]
pub use transport::IpcClientConnection;
pub use transport::{
    connect, unique_test_endpoint, IpcConnection, IpcListener, DEFAULT_CLIENT_RECV_TIMEOUT,
};

use zccache_core::NormalizedPath;

/// Returns the platform-specific default IPC endpoint path.
///
/// - Linux: `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-$USER/sock`
/// - macOS: `/tmp/zccache-$USER/sock`
/// - Windows: `\\.\pipe\zccache-{username}`
///
/// If `ZCCACHE_CACHE_DIR` is set, the endpoint is derived from that cache root
/// so independently managed cache roots get independent daemon instances.
#[must_use]
pub fn default_endpoint() -> String {
    if let Some(cache_dir) = zccache_core::config::cache_dir_override() {
        return endpoint_for_cache_dir(&cache_dir);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{runtime_dir}/zccache/sock");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-{user}/sock")
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        format!(r"\\.\pipe\zccache-{username}")
    }
}

fn endpoint_for_cache_dir(cache_dir: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        cache_dir.join("daemon.sock").to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        let suffix = zccache_core::stable_path_id(cache_dir);
        format!(r"\\.\pipe\zccache-{suffix}")
    }
}

/// Returns the path for the daemon lock file.
#[must_use]
pub fn lock_file_path() -> NormalizedPath {
    if let Some(cache_dir) = zccache_core::config::cache_dir_override() {
        return cache_dir.join("daemon.lock");
    }

    #[cfg(unix)]
    {
        let endpoint = default_endpoint();
        let dir = std::path::Path::new(&endpoint)
            .parent()
            .expect("endpoint should have parent directory");
        dir.join("daemon.lock").into()
    }
    #[cfg(windows)]
    {
        zccache_core::config::default_cache_dir().join("daemon.lock")
    }
}

/// Write the daemon PID to the lock file.
///
/// Creates parent directories if needed.
pub fn write_lock_file(pid: u32) -> Result<(), std::io::Error> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, pid.to_string())
}

/// Read the daemon PID from the lock file, if it exists and is valid.
#[must_use]
pub fn read_lock_file_pid() -> Option<u32> {
    std::fs::read_to_string(lock_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove the lock file.
pub fn remove_lock_file() {
    let _ = std::fs::remove_file(lock_file_path());
}

/// Forcefully terminate a process by PID.
///
/// This is intended as a last-resort escape hatch when the daemon is no longer
/// reachable over IPC, so graceful shutdown is not possible.
pub fn force_kill_process(pid: u32) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        // SAFETY: kill is called with a PID provided by the caller and a fixed
        // signal value. No pointers are involved.
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        let rc = unsafe { kill(pid as i32, SIGKILL) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn TerminateProcess(handle: isize, exit_code: u32) -> i32;
            fn CloseHandle(handle: isize) -> i32;
        }
        const PROCESS_TERMINATE: u32 = 0x0001;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
            if handle == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let result = TerminateProcess(handle, 1);
            let err = if result == 0 {
                Some(std::io::Error::last_os_error())
            } else {
                None
            };
            CloseHandle(handle);
            match err {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }
    }
}

/// Check if a process with the given PID is alive.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) is a standard POSIX call that checks process
        // existence without sending any signal.
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe { kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn CloseHandle(handle: isize) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle != 0 {
                CloseHandle(handle);
                true
            } else {
                false
            }
        }
    }
}

/// Returns true if `pid` exists **and** its executable looks like a zccache
/// daemon. Defends against stale `daemon.lock` files where the recorded PID has
/// been recycled by an unrelated process — typical when a CI runner restores a
/// cache directory containing a lock file from a prior, abruptly-terminated
/// run. Without this check, [`check_running_daemon`] would mis-identify the
/// recycled PID as our daemon and callers like `zccache stop` would
/// `force_kill_process` an arbitrary system process. See issue #132.
#[must_use]
pub fn verify_daemon_pid(pid: u32) -> bool {
    verify_pid_exe_stem(pid, "zccache-daemon")
}

/// Generic version of [`verify_daemon_pid`]: confirms `pid` is alive and its
/// executable filename (without `.exe`) matches `expected_stem`. Used by
/// callers that own a different daemon binary (e.g. the download daemon).
#[must_use]
pub fn verify_pid_exe_stem(pid: u32, expected_stem: &str) -> bool {
    if !is_process_alive(pid) {
        return false;
    }
    match daemon_exe_for_pid(pid) {
        // Got an exe path — only trust the PID if it points at our daemon.
        Some(exe) => exe_stem_matches(&exe, expected_stem),
        // Platform doesn't support reading the exe path. Fall back to the
        // existing alive-only behavior so we don't regress on those platforms.
        None => true,
    }
}

fn exe_stem_matches(path: &std::path::Path, expected_stem: &str) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == expected_stem
}

#[cfg(target_os = "linux")]
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(NormalizedPath::from)
}

#[cfg(target_os = "macos")]
fn daemon_exe_for_pid(_pid: u32) -> Option<NormalizedPath> {
    // proc_pidpath is the right call but pulling in libc/libproc just for
    // CI-recycle defense isn't worth it on macOS, where this failure mode
    // hasn't been observed. Fall back to alive-only.
    None
}

#[cfg(windows)]
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn QueryFullProcessImageNameW(
            handle: isize,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return None;
        }
        let mut buf = vec![0u16; 32_768];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        use std::os::windows::ffi::OsStringExt;
        let os = std::ffi::OsString::from_wide(&buf[..size as usize]);
        Some(NormalizedPath::new(&os))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn daemon_exe_for_pid(_pid: u32) -> Option<NormalizedPath> {
    None
}

/// Check if a daemon is already running. Returns the PID if alive.
#[must_use]
pub fn check_running_daemon() -> Option<u32> {
    let pid = read_lock_file_pid()?;
    if verify_daemon_pid(pid) {
        Some(pid)
    } else {
        // Stale lock file — clean up. The PID may be dead, or may belong to
        // an unrelated process that recycled the lock file's PID (issue #132).
        remove_lock_file();
        #[cfg(unix)]
        {
            // Also remove stale socket on Unix
            let endpoint = default_endpoint();
            let _ = std::fs::remove_file(&endpoint);
        }
        None
    }
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
    fn cache_dir_override_moves_endpoint_and_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&cache_dir);

        let endpoint = default_endpoint();
        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir.join("daemon.sock").to_string_lossy().into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
            assert!(endpoint.ends_with(&zccache_core::stable_path_id(&cache_dir)));
        }

        assert_eq!(lock_file_path(), cache_dir.join("daemon.lock"));
    }

    #[test]
    fn different_cache_roots_get_different_endpoints() {
        let a = NormalizedPath::from("/tmp/zccache-a");
        let b = NormalizedPath::from("/tmp/zccache-b");
        assert_ne!(endpoint_for_cache_dir(&a), endpoint_for_cache_dir(&b));
    }

    #[test]
    fn exe_stem_matches_strips_exe_suffix_and_compares_basename() {
        use std::path::Path;
        assert!(exe_stem_matches(
            Path::new("/usr/bin/zccache-daemon"),
            "zccache-daemon"
        ));
        // A different binary at the same PID must not be accepted.
        assert!(!exe_stem_matches(
            Path::new("/usr/bin/bash"),
            "zccache-daemon"
        ));
        assert!(!exe_stem_matches(
            Path::new("/usr/bin/zccache-daemon-x"),
            "zccache-daemon"
        ));
    }

    /// Windows-only: backslash-separated paths require the OS-native
    /// `Path::file_name` semantics. On Unix `\` is a regular filename
    /// character, so the same assertion would fail there (issue #143).
    #[cfg(windows)]
    #[test]
    fn exe_stem_matches_strips_exe_suffix_on_windows() {
        use std::path::Path;
        assert!(exe_stem_matches(
            Path::new(r"C:\bin\zccache-daemon.exe"),
            "zccache-daemon"
        ));
    }

    /// Regression test for issue #132: a stale `daemon.lock` restored from a
    /// CI cache can carry a PID that's been recycled by an unrelated process
    /// on a fresh runner. `check_running_daemon` must NOT report that process
    /// as our daemon — otherwise `zccache stop` would `force_kill_process`
    /// the unrelated process.
    ///
    /// We use the test's own PID, which is guaranteed alive but is clearly
    /// not zccache-daemon, then assert the lock file is treated as stale.
    #[test]
    fn stale_lock_with_recycled_pid_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&cache_dir);

        let lock = lock_file_path();
        write_lock_file(std::process::id()).unwrap();
        assert!(lock.exists());

        // The test process is alive but is not zccache-daemon — must be rejected.
        // (On macOS we can't read the exe path, so this test relaxes there: see
        // `daemon_exe_for_pid` for the platform fallback.)
        #[cfg(any(target_os = "linux", windows))]
        {
            assert!(check_running_daemon().is_none());
            assert!(!lock.exists(), "stale lock file should have been removed");
        }
    }
}
