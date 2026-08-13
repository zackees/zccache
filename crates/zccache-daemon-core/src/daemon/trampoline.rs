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

    // If we are already running from the CLI-deployed daemon copy under the
    // versioned cache dir (`<global>/v<VERSION>/zccache-daemon.exe`, #999),
    // the install path is not locked by us — no rename needed. Short-circuit.
    // See issues #134 / #999.
    if exe_is_deployed_daemon(&my_exe) {
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

/// Release the launch-cwd handle by chdir-ing to a stable global
/// directory the daemon owns. On Windows a running process holds an
/// implicit kernel handle on its cwd, so launching the daemon from a
/// project dir blocks deletion of that dir until the daemon exits.
/// Runs on every OS.
///
/// Target order:
///   1. `~/.zccache/` — sibling of the daemon's runtime-binaries and
///      logs, stable across reboots and outside any user workspace.
///      Preferred per #747: a path the daemon owns can never be
///      reconfigured via a stray `TMP` / `TEMP` env override.
///   2. `std::env::temp_dir()` — fallback when the home directory is
///      not discoverable (`$HOME` / `%USERPROFILE%` unset) or
///      chdir into `~/.zccache/` fails for any reason. Best-effort and
///      safe on every supported platform.
///
/// Best-effort; no panic on failure. A failure here is strictly better
/// than the pre-fix behavior (no chdir at all): the daemon will still
/// hold the inherited workspace handle, but anything that worked before
/// still works.
pub fn release_cwd() {
    if let Some(stable) = zccache_home_dir() {
        let _ = std::fs::create_dir_all(&stable);
        if std::env::set_current_dir(&stable).is_ok() {
            return;
        }
    }
    let _ = std::env::set_current_dir(std::env::temp_dir());
}

/// `~/.zccache/` resolved from `$HOME` (Unix) or `%USERPROFILE%`
/// (Windows). Returns `None` if neither env var is set.
///
/// Intentionally does NOT consult `ZCCACHE_CACHE_DIR`: that override
/// can legitimately point into a workspace (perf scenarios use a
/// project-local cache), and the whole point of [`release_cwd`] is to
/// chdir OUT of any workspace so its directory handle is released.
fn zccache_home_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    if home.is_empty() {
        return None;
    }
    Some(std::path::Path::new(&home).join(".zccache"))
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
    zccache_platform::process::stdio::detach();
}

/// Redirect this process's stdout and stderr to the log path, leaving stdin
/// connected to the platform null device. Falls back to full detachment when
/// the log cannot be opened.
pub fn redirect_stdio_to_log(log_path: &Path) {
    if !zccache_platform::process::stdio::redirect_to_log(log_path) {
        zccache_platform::process::stdio::detach();
    }
}

/// True if `exe` is the daemon binary the CLI deployed under the versioned
/// cache dir (`<global_cache_dir>/zccache-daemon[.exe]`, issue #999) — i.e. we
/// are already running from the relocated copy, not the install path, so no
/// unlock rename is needed. Compared against the canonicalized cache dir to be
/// robust to symlinks and short-name (8.3) tilde expansion on Windows.
fn exe_is_deployed_daemon(exe: &Path) -> bool {
    let cache_dir = crate::core::config::default_cache_dir();
    let cache_canon = match fs::canonicalize(cache_dir.as_path()) {
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
    exe_parent_canon == cache_canon
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

    #[test]
    fn zccache_home_dir_resolves_to_dot_zccache_under_home_or_userprofile() {
        // Ignores `ZCCACHE_CACHE_DIR` deliberately — see the doc comment
        // on `zccache_home_dir`. Reads `HOME` / `USERPROFILE` directly.
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("test host must have HOME or USERPROFILE set");
        let resolved = zccache_home_dir().expect("home discoverable");
        assert_eq!(resolved, Path::new(&home).join(".zccache"));
    }
}
