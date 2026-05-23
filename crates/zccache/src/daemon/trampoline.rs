//! Windows exe unlock + cwd release for the long-running zccache daemon.
//!
//! Problem: On Windows, running executables are file-locked. `pip install
//! --upgrade zccache` fails if the daemon is running because it can't
//! overwrite Scripts/zccache-daemon.exe. Likewise, a running process holds
//! an implicit kernel handle on its current working directory, so launching
//! the daemon from a project dir blocks deletion of that dir until the
//! daemon exits.
//!
//! Solution: This module is a verbatim port of clud's same-named pattern
//! at `crates/clud-bin/src/trampoline.rs` (see the `unlock_exe` and
//! `gc_old_files` functions there). On launch, the daemon renames itself
//! (`Scripts/zccache-daemon.exe` → `zccache-daemon.exe.old.<rand>`), then
//! copies a fresh unlocked copy back to Scripts/zccache-daemon.exe. The
//! running process continues from the renamed file. No child process, no
//! handle transfer.
//!
//! Result: Scripts/zccache-daemon.exe is always an unlocked copy. pip
//! install always works. Each running instance locks its own
//! `zccache-daemon.exe.old.<rand>` file.
//!
//! IMPORTANT: Every operation is best-effort. If anything fails, the app
//! continues normally — it just won't get the lock-free install benefit.
//!
//! On Linux/macOS: `unlock_exe` is a no-op (Unix allows deleting running
//! binaries). `release_cwd` runs on every OS — it's cheap and the
//! Windows-specific motivation (cwd handle pinning) is the primary driver.

use std::fs;
use std::path::Path;

/// Unlock the running daemon binary on Windows so it can be replaced by
/// `pip install --upgrade zccache` while we keep running. Verbatim port of
/// clud's `unlock_exe()` (`crates/clud-bin/src/trampoline.rs:141`):
/// rename `zccache-daemon.exe` → `zccache-daemon.exe.old.<rand>`, copy
/// back so the canonical path is unlocked, then GC stale `.old.*` siblings
/// in a background thread. Best-effort — no panics on failure.
///
/// No-op on non-Windows. Set `ZCCACHE_NO_UNLOCK=1` to opt out (mirrors
/// clud's `CLUD_NO_UNLOCK`).
pub fn unlock_exe() {
    if !cfg!(target_os = "windows") {
        return;
    }

    // Escape hatch for CI / test harnesses that spawn many short-lived
    // zccache invocations: the rename+copy+GC dance on every start costs
    // real time and (under investigation in clud's #37) appears to keep
    // stdout/stderr pipe handles open on Windows GHA runners so Python's
    // subprocess.run never sees EOF. Set `ZCCACHE_NO_UNLOCK=1` to disable.
    if std::env::var_os("ZCCACHE_NO_UNLOCK").is_some() {
        return;
    }

    let my_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    // If the CLI already relocated us into `<global>/runtime-binaries/`
    // before spawning, the install path is already unlocked. No rename
    // needed — short-circuit. See issue #134.
    if exe_is_under_runtime_binaries(&my_exe) {
        return;
    }

    // Rename zccache-daemon.exe → zccache-daemon.exe.old.<rand>. We keep
    // running from the renamed file.
    let rand_id: u32 = std::process::id()
        ^ (std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .subsec_nanos());
    let old_exe = my_exe.with_extension(format!("exe.old.{rand_id}"));

    if fs::rename(&my_exe, &old_exe).is_err() {
        tracing::warn!(
            "could not unlock exe for hot-reload; pip install may fail while zccache is running"
        );
        return;
    }

    // Copy back: zccache-daemon.exe.old.<rand> → zccache-daemon.exe (new
    // file, unlocked).
    let _ = fs::copy(&old_exe, &my_exe);

    // GC stale .old files in background. Fire and forget.
    let parent = match my_exe.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    let stem = match my_exe.file_name().and_then(|n| n.to_str()) {
        Some(s) => s.to_string(),
        None => return,
    };
    std::thread::spawn(move || gc_old_files(&parent, &stem));
}

/// Release the launch-cwd handle by chdir-ing to the OS temp dir. On
/// Windows a running process holds an implicit kernel handle on its
/// cwd, so launching the daemon from a project dir blocks deletion of
/// that dir until the daemon exits. Cheap one-liner, runs on every OS.
pub fn release_cwd() {
    let _ = std::env::set_current_dir(std::env::temp_dir());
}

/// Detach inherited stdio (stdin/stdout/stderr) by re-opening them to the
/// platform null device (`/dev/null` on Unix, `NUL` on Windows). This
/// closes whatever file descriptors / handles the daemon inherited from
/// its spawning process, releasing any pipe write ends in particular.
///
/// Without this, a grandparent process that reads the daemon's
/// (inherited) stdout via a pipe — e.g. Python's
/// `subprocess.Popen(["soldr", "cargo", "build", ...], stdout=PIPE)` —
/// never observes EOF after the parent exits, because the orphaned daemon
/// keeps the pipe's write end alive indefinitely. See issue #276 for the
/// real-world hang this fix prevents (47+ minute waits on Windows).
///
/// Called once, very early in the daemon binary's `main()` before the
/// tracing subscriber is installed, so the subscriber's stdout/stderr
/// writes go to the null device from the start. Do not move this later:
/// any code that writes via `println!` / `tracing` between startup and
/// the detach point would still hit the inherited pipe and defeat the
/// purpose.
///
/// Best-effort — no panics on failure. A best-effort detach is strictly
/// better than no detach, and any platform where this fails is a platform
/// where the original pipe write end could not have been opened anyway.
pub fn detach_stdio() {
    #[cfg(unix)]
    detach_stdio_unix();

    #[cfg(windows)]
    detach_stdio_windows();
}

/// Redirect this process's stdout + stderr to the given log file, leaving
/// stdin nulled.
///
/// Differs from [`detach_stdio`] in that the daemon's tracing output and
/// any pre-tracing panic spew lands in a file instead of `/dev/null` (Unix)
/// or `NUL` (Windows). The CLI passes the path via `--log-file` and the
/// daemon calls this before [`super::crash::install_panic_hook`] so even a
/// dyld / gatekeeper / pre-runtime panic on macOS leaves evidence on disk.
///
/// Best-effort: open errors fall through to [`detach_stdio`] so a missing
/// directory or read-only filesystem never blocks daemon start.
pub fn redirect_stdio_to_log(log_path: &Path) {
    #[cfg(unix)]
    if !redirect_stdio_to_log_unix(log_path) {
        detach_stdio_unix();
    }

    #[cfg(windows)]
    if !redirect_stdio_to_log_windows(log_path) {
        detach_stdio_windows();
    }
}

#[cfg(unix)]
fn redirect_stdio_to_log_unix(log_path: &Path) -> bool {
    // SAFETY: open/dup2/close are async-signal-safe and we're running on
    // the main thread with no other threads spawned yet.
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path_c = match CString::new(log_path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };

    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if null < 0 {
            return false;
        }
        let _ = libc::dup2(null, libc::STDIN_FILENO);
        if null > libc::STDERR_FILENO {
            let _ = libc::close(null);
        }

        let log_fd = libc::open(
            path_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o644,
        );
        if log_fd < 0 {
            return false;
        }
        let _ = libc::dup2(log_fd, libc::STDOUT_FILENO);
        let _ = libc::dup2(log_fd, libc::STDERR_FILENO);
        if log_fd > libc::STDERR_FILENO {
            let _ = libc::close(log_fd);
        }
    }
    true
}

#[cfg(windows)]
fn redirect_stdio_to_log_windows(log_path: &Path) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
        fn SetStdHandle(n_std_handle: u32, h_handle: *mut std::ffi::c_void) -> i32;
        fn CloseHandle(h_object: *mut std::ffi::c_void) -> i32;
        fn SetFilePointerEx(
            h_file: *mut std::ffi::c_void,
            li_distance_to_move: i64,
            lp_new_file_pointer: *mut i64,
            dw_move_method: u32,
        ) -> i32;
    }

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const OPEN_ALWAYS: u32 = 4;
    const FILE_END: u32 = 2;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    let path_w: Vec<u16> = log_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let nul_w: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();

    unsafe {
        // stdin → NUL (read).
        let nul_in = CreateFileW(
            nul_w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if !nul_in.is_null() && nul_in != INVALID_HANDLE_VALUE {
            let old = GetStdHandle(STD_INPUT_HANDLE);
            let _ = SetStdHandle(STD_INPUT_HANDLE, nul_in);
            if !old.is_null() && old != INVALID_HANDLE_VALUE {
                let _ = CloseHandle(old);
            }
        }

        // stdout + stderr → log file (append).
        let log_handle = CreateFileW(
            path_w.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_ALWAYS,
            0,
            ptr::null_mut(),
        );
        if log_handle.is_null() || log_handle == INVALID_HANDLE_VALUE {
            return false;
        }
        // Seek to EOF so we append rather than truncate.
        let _ = SetFilePointerEx(log_handle, 0, ptr::null_mut(), FILE_END);

        for slot in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let old = GetStdHandle(slot);
            let _ = SetStdHandle(slot, log_handle);
            if !old.is_null() && old != INVALID_HANDLE_VALUE {
                let _ = CloseHandle(old);
            }
        }
    }
    true
}

#[cfg(unix)]
fn detach_stdio_unix() {
    // SAFETY: open/dup2/close are async-signal-safe and we're running on
    // the main thread with no other threads spawned yet. Failure of any
    // step is logged at debug level (we cannot use tracing here — it
    // hasn't been initialised — and we deliberately don't write to
    // stderr, since that's exactly what we're trying to detach).
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return;
        }
        let _ = libc::dup2(null, libc::STDIN_FILENO);
        let _ = libc::dup2(null, libc::STDOUT_FILENO);
        let _ = libc::dup2(null, libc::STDERR_FILENO);
        if null > libc::STDERR_FILENO {
            let _ = libc::close(null);
        }
    }
}

#[cfg(windows)]
fn detach_stdio_windows() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
        fn SetStdHandle(n_std_handle: u32, h_handle: *mut std::ffi::c_void) -> i32;
        fn CloseHandle(h_object: *mut std::ffi::c_void) -> i32;
    }

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // (DWORD)-10
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    let nul: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();

    // Replace each std handle with a fresh handle to NUL. Open one fresh
    // NUL handle per slot so closing the old one (which may be the same
    // underlying object on consoles) doesn't invalidate the others.
    for (slot, access) in [
        (STD_INPUT_HANDLE, GENERIC_READ),
        (STD_OUTPUT_HANDLE, GENERIC_WRITE),
        (STD_ERROR_HANDLE, GENERIC_WRITE),
    ] {
        // SAFETY: we own the std handle slots for this process; no other
        // thread is touching them this early in startup.
        unsafe {
            let nul_handle = CreateFileW(
                nul.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if nul_handle.is_null() || nul_handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let old = GetStdHandle(slot);
            let _ = SetStdHandle(slot, nul_handle);
            // GetStdHandle returns NULL when no handle is set and
            // INVALID_HANDLE_VALUE on error; neither is closeable.
            if !old.is_null() && old != INVALID_HANDLE_VALUE {
                let _ = CloseHandle(old);
            }
        }
    }
}

/// True if `exe` lives directly inside `<global_cache_dir>/runtime-binaries/`,
/// i.e. the CLI already copied us out of the install path. Compared
/// against the canonicalized cache dir to be robust to symlinks and
/// short-name (8.3) tilde expansion on Windows.
fn exe_is_under_runtime_binaries(exe: &Path) -> bool {
    let runtime_dir = zccache::core::config::default_cache_dir().join("runtime-binaries");
    let runtime_canon = match fs::canonicalize(&runtime_dir) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let exe_parent = match exe.parent() {
        Some(p) => p,
        None => return false,
    };
    let exe_parent_canon = match fs::canonicalize(exe_parent) {
        Ok(p) => p,
        Err(_) => return false,
    };
    exe_parent_canon == runtime_canon
}

/// Delete stale .old files next to the exe. Best-effort — locked files skipped.
fn gc_old_files(dir: &Path, stem: &str) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(stem) && name_str.contains(".old") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_old_files() {
        let tmp = std::env::temp_dir().join("zccache-unlock-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Simulate: stem.exe + two stale .old files
        fs::write(tmp.join("stem.exe"), b"current").unwrap();
        fs::write(tmp.join("stem.exe.old.1"), b"old1").unwrap();
        fs::write(tmp.join("stem.exe.old.2"), b"old2").unwrap();
        fs::write(tmp.join("other.exe"), b"unrelated").unwrap();

        gc_old_files(&tmp, "stem.exe");

        assert!(tmp.join("stem.exe").is_file()); // untouched
        assert!(!tmp.join("stem.exe.old.1").exists()); // cleaned
        assert!(!tmp.join("stem.exe.old.2").exists()); // cleaned
        assert!(tmp.join("other.exe").is_file()); // untouched

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_gc_missing_dir() {
        // Should not panic on nonexistent directory.
        gc_old_files(Path::new("/nonexistent/dir"), "stem.exe");
    }

    #[test]
    fn test_release_cwd_changes_dir() {
        let tmp = std::env::temp_dir().join("zccache-release-cwd-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Resolve via canonicalize so the comparison is robust against
        // symlinked temp dirs (e.g. /var → /private/var on macOS).
        let tmp_canon = fs::canonicalize(&tmp).unwrap();
        std::env::set_current_dir(&tmp_canon).unwrap();
        assert_eq!(std::env::current_dir().unwrap(), tmp_canon);

        release_cwd();

        assert_ne!(std::env::current_dir().unwrap(), tmp_canon);

        let _ = fs::remove_dir_all(&tmp);
    }
}
