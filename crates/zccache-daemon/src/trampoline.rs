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
