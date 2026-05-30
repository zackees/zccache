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

use crate::core::NormalizedPath;

#[cfg(unix)]
const MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES: usize = 100;

/// Returns the platform-specific default IPC endpoint path.
///
/// - Linux: `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-$USER/sock`
/// - macOS: `/tmp/zccache-$USER/sock`
/// - Windows: `\\.\pipe\zccache-{username}`
///
/// If `ZCCACHE_CACHE_DIR` is set, the endpoint is derived from that cache root
/// so independently managed cache roots get independent daemon instances.
/// If `ZCCACHE_DAEMON_NAMESPACE` is also set, the sanitized namespace is folded
/// into the endpoint while the unset/default namespace keeps the historical
/// endpoint unchanged.
#[must_use]
pub fn default_endpoint() -> String {
    let namespace = crate::core::config::daemon_namespace();
    if let Some(cache_dir) = crate::core::config::cache_dir_override() {
        return endpoint_for_cache_dir(&cache_dir, namespace.as_deref());
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!(
                "{runtime_dir}/zccache/{}",
                socket_name(namespace.as_deref())
            );
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-{user}/{}", socket_name(namespace.as_deref()))
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        pipe_name(&username, namespace.as_deref())
    }
}

pub fn endpoint_for_cache_dir(cache_dir: &std::path::Path, namespace: Option<&str>) -> String {
    #[cfg(unix)]
    {
        let direct = cache_dir.join(daemon_socket_name(namespace));
        let direct = direct.to_string_lossy();
        if direct.len() <= MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES {
            return direct.into_owned();
        }

        compact_cache_dir_endpoint(cache_dir, namespace)
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        let suffix = crate::core::stable_path_id(cache_dir);
        pipe_name(&suffix, namespace)
    }
}

#[cfg(unix)]
fn compact_cache_dir_endpoint(
    cache_dir: &std::path::Path,
    namespace: Option<&str>,
) -> std::path::PathBuf {
    let cache_id = crate::core::stable_path_id(cache_dir);
    std::path::PathBuf::from(format!(
        "/tmp/zccache-{cache_id}-{}",
        daemon_socket_name(namespace)
    ))
}

/// Derive a platform IPC endpoint for a portable private daemon name.
///
/// When `cache_dir` is supplied the endpoint is rooted in that cache identity;
/// otherwise it follows the default runtime/tmp/pipe location while folding
/// the sanitized daemon name into the endpoint.
#[must_use]
pub fn endpoint_for_private_daemon_name(
    cache_dir: Option<&std::path::Path>,
    daemon_name: &str,
) -> String {
    let namespace = crate::core::config::sanitize_daemon_namespace(daemon_name)
        .unwrap_or_else(|| crate::core::config::DEV_DAEMON_NAMESPACE.to_string());
    if let Some(cache_dir) = cache_dir {
        return endpoint_for_cache_dir(cache_dir, Some(&namespace));
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{runtime_dir}/zccache/{}", socket_name(Some(&namespace)));
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-{user}/{}", socket_name(Some(&namespace)))
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        pipe_name(&username, Some(&namespace))
    }
}

/// Returns the path for the daemon lock file.
#[must_use]
pub fn lock_file_path() -> NormalizedPath {
    let namespace = crate::core::config::daemon_namespace();
    if let Some(cache_dir) = crate::core::config::cache_dir_override() {
        return cache_dir.join(lock_file_name(namespace.as_deref()));
    }

    #[cfg(unix)]
    {
        let endpoint = default_endpoint();
        let dir = std::path::Path::new(&endpoint)
            .parent()
            .expect("endpoint should have parent directory");
        dir.join(lock_file_name(namespace.as_deref())).into()
    }
    #[cfg(windows)]
    {
        crate::core::config::default_cache_dir().join(lock_file_name(namespace.as_deref()))
    }
}

#[cfg(unix)]
fn socket_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("sock-{ns}"),
        None => "sock".to_string(),
    }
}

#[cfg(unix)]
fn daemon_socket_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("daemon-{ns}.sock"),
        None => "daemon.sock".to_string(),
    }
}

#[cfg(windows)]
fn pipe_name(base: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!(r"\\.\pipe\zccache-{base}-{ns}"),
        None => format!(r"\\.\pipe\zccache-{base}"),
    }
}

fn lock_file_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("daemon-{ns}.lock"),
        None => "daemon.lock".to_string(),
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
        // windows-sys defines CloseHandle/OpenProcess/TerminateProcess with
        // HANDLE/BOOL newtypes; our local extern uses the underlying isize/i32
        // for ergonomics. Same ABI, different signature in the type-system,
        // so the linker accepts both but rustc warns. -D warnings on CI
        // promotes the warn to error.
        #[allow(clashing_extern_declarations)]
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
        // See CloseHandle note in force_kill_process above.
        #[allow(clashing_extern_declarations)]
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
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    // `proc_pidpath` from libproc (`libSystem.dylib`) — same one
    // `ps`/`lsof` use under the hood. Available on macOS 10.5+.
    //
    // PROC_PIDPATHINFO_MAXSIZE is documented as 4 * MAXPATHLEN (= 4096)
    // in `<sys/proc_info.h>`. Allocate exactly that and let the call
    // tell us how many bytes it wrote.
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;

    extern "C" {
        fn proc_pidpath(pid: i32, buf: *mut std::ffi::c_void, bufsize: u32) -> i32;
    }

    let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: pid is a u32 from the caller, buf is a freshly-allocated
    // Vec we own. bufsize matches the allocation size. proc_pidpath
    // returns the number of bytes written (>0) or -1 on error and is
    // tolerant of stale PIDs (returns ESRCH).
    let written = unsafe { proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if written <= 0 {
        // EPERM (process belongs to another user), ESRCH (pid gone), etc.
        // Don't trust the PID — recycled-PID defense fires.
        return None;
    }
    buf.truncate(written as usize);
    let s = std::str::from_utf8(&buf).ok()?;
    Some(NormalizedPath::from(std::path::PathBuf::from(s)))
}

#[cfg(windows)]
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    // See CloseHandle note in force_kill_process above.
    #[allow(clashing_extern_declarations)]
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
        previous_cache_dir: Option<OsString>,
        previous_namespace: Option<OsString>,
    }

    impl EnvGuard {
        fn set_cache_dir(value: &std::path::Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
            std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
            std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
            }
        }

        fn set_cache_dir_and_namespace(value: &std::path::Path, namespace: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
            std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
            std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous_cache_dir {
                Some(value) => std::env::set_var(crate::core::config::CACHE_DIR_ENV, value),
                None => std::env::remove_var(crate::core::config::CACHE_DIR_ENV),
            }
            match &self.previous_namespace {
                Some(value) => std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, value),
                None => std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV),
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
            assert!(endpoint.ends_with(&crate::core::stable_path_id(&cache_dir)));
        }

        assert_eq!(lock_file_path(), cache_dir.join("daemon.lock"));
    }

    #[test]
    fn different_cache_roots_get_different_endpoints() {
        let a = NormalizedPath::from("/tmp/zccache-a");
        let b = NormalizedPath::from("/tmp/zccache-b");
        assert_ne!(
            endpoint_for_cache_dir(&a, None),
            endpoint_for_cache_dir(&b, None)
        );
    }

    #[test]
    fn daemon_namespace_moves_endpoint_and_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev");

        let endpoint = default_endpoint();
        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir
                .join("daemon-soldr-dev.sock")
                .to_string_lossy()
                .into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
            assert!(endpoint.ends_with("-soldr-dev"));
            assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
        }

        assert_eq!(lock_file_path(), cache_dir.join("daemon-soldr-dev.lock"));
    }

    #[test]
    fn same_cache_root_different_daemon_namespaces_do_not_share_identity() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");

        let (endpoint_a, lock_a) = {
            let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev-a");
            (default_endpoint(), lock_file_path())
        };
        let (endpoint_b, lock_b) = {
            let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev-b");
            (default_endpoint(), lock_file_path())
        };

        assert_ne!(endpoint_a, endpoint_b);
        assert_ne!(lock_a, lock_b);
    }

    #[test]
    fn private_daemon_name_derives_endpoint_from_cache_root() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let endpoint = endpoint_for_private_daemon_name(Some(&cache_dir), "soldr dev");

        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir
                .join("daemon-soldr_dev.sock")
                .to_string_lossy()
                .into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
            assert!(endpoint.ends_with("-soldr_dev"));
            assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cache_dir_endpoint_falls_back_to_short_unix_socket_path() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root
            .path()
            .join("this")
            .join("is")
            .join("a")
            .join("deep")
            .join("private")
            .join("zccache")
            .join("cache")
            .join("directory")
            .join("that")
            .join("would")
            .join("exceed")
            .join("sockaddr_un")
            .join("path")
            .join("limits");

        let endpoint = endpoint_for_cache_dir(&cache_dir, Some("soldr-dev"));

        assert!(
            endpoint.len() <= MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES,
            "endpoint too long: {endpoint}"
        );
        assert!(endpoint.starts_with("/tmp/zccache-"));
        assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
        assert!(endpoint.ends_with("-daemon-soldr-dev.sock"));
    }

    /// On macOS, `daemon_exe_for_pid` must reject a PID whose
    /// executable is something other than `zccache-daemon`. Until
    /// `proc_pidpath` was wired up, this returned `None` and
    /// `verify_pid_exe_stem` fell back to alive-only — which meant a
    /// recycled PID in `daemon.lock` could keep the CLI talking to a
    /// random process on a shared CI runner. This test would have
    /// failed before that fix.
    #[cfg(target_os = "macos")]
    #[test]
    fn recycled_pid_is_rejected_on_macos() {
        use std::process::Stdio;

        // `/bin/sleep 60` — guaranteed-alive, not zccache-daemon.
        let mut sleeper = std::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = sleeper.id();

        let exe = daemon_exe_for_pid(pid);
        let verified = verify_pid_exe_stem(pid, "zccache-daemon");

        // Clean up before assertions so a panic doesn't orphan the child.
        let _ = sleeper.kill();
        let _ = sleeper.wait();

        let exe = exe.expect("proc_pidpath must succeed for an alive child");
        let basename = exe
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        assert_eq!(
            basename, "sleep",
            "proc_pidpath should report `sleep` as the executable"
        );
        assert!(
            !verified,
            "verify_pid_exe_stem must reject a /bin/sleep PID even though it is alive"
        );
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
