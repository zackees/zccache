//! argv[0] multi-call dispatch (issue #998).
//!
//! The `zccache` binary is deployed as the only executable; when it needs the
//! daemon it copies itself to `…/v<VERSION>/zccache-daemon[.exe]` (issue #999)
//! and runs that copy. The copy — seeing its own `argv[0]` is the daemon name —
//! must run the daemon instead of the CLI. This module is the name check.
//!
//! Any other or unrecognized `argv[0]` (including an empty/mangled one) falls
//! through to the standard CLI. That unknown-name → CLI fallback is the safety
//! net: argv[0] is not guaranteed meaningful on every platform, so the CLI is
//! always the default and the daemon is only ever entered on an exact name
//! match (or the explicit `zccache daemon-run` escape hatch).

use std::ffi::OsStr;
use std::path::Path;

/// The file stem the self-copied daemon binary is deployed under. Must match
/// `verify_pid_exe_stem(pid, "zccache-daemon")` in `zccache-ipc` and the
/// deployment path in #999.
pub const DAEMON_STEM: &str = "zccache-daemon";

/// True when this process was invoked under the daemon's name.
///
/// Reads `std::env::args_os().next()` (argv[0]); returns `false` when it is
/// absent so the caller falls through to the CLI.
pub fn invoked_as_daemon() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|arg0| stem_matches(&arg0, DAEMON_STEM))
}

/// Core, testable name check: does `arg0`'s file stem equal `target`?
///
/// - The directory portion of a path is ignored (`/a/b/zccache-daemon` → yes).
/// - On Windows the comparison is case-insensitive and the `.exe` suffix is
///   dropped by `file_stem`; on Unix it is exact.
/// - `file_stem` strips only the final extension, so a teardown orphan named
///   `zccache-daemon.old.<rand>.exe` (issue #999 Windows unlock rename) does
///   NOT match — a dead relocated binary must never dispatch as the daemon.
fn stem_matches(arg0: &OsStr, target: &str) -> bool {
    let Some(stem) = Path::new(arg0).file_stem() else {
        return false;
    };
    let Some(stem) = stem.to_str() else {
        return false;
    };
    if cfg!(windows) {
        stem.eq_ignore_ascii_case(target)
    } else {
        stem == target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn matches(s: &str) -> bool {
        stem_matches(&OsString::from(s), DAEMON_STEM)
    }

    #[test]
    fn bare_daemon_name_matches() {
        assert!(matches("zccache-daemon"));
    }

    #[test]
    fn full_path_matches() {
        assert!(matches("/home/u/.zccache/v1.12.15/zccache-daemon"));
        // Backslash is a path separator only on Windows; on Unix it is an
        // ordinary filename char, so a backslash "path" is one component and
        // would not stem to `zccache-daemon`. Assert the native form per-OS.
        #[cfg(windows)]
        assert!(matches(r"C:\Users\u\.zccache\v1.12.15\zccache-daemon.exe"));
    }

    #[test]
    fn exe_suffix_stripped() {
        assert!(matches("zccache-daemon.exe"));
    }

    #[test]
    fn cli_name_does_not_match() {
        assert!(!matches("zccache"));
        assert!(!matches("zccache.exe"));
        assert!(!matches("/usr/bin/zccache"));
    }

    #[test]
    fn teardown_orphan_does_not_match() {
        // #999 renames a locked exe to `<stem>.old.<rand>.exe` to free it for
        // rm -rf; that orphan must never dispatch as the daemon.
        assert!(!matches("zccache-daemon.old.9271.exe"));
        assert!(!matches("zccache-daemon.old.9271"));
    }

    #[test]
    fn empty_or_unrelated_does_not_match() {
        assert!(!matches(""));
        assert!(!matches("some-other-tool"));
        assert!(!matches("zccache-download-daemon"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_is_case_insensitive() {
        assert!(matches("ZCCACHE-DAEMON.EXE"));
        assert!(matches("Zccache-Daemon"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_is_case_sensitive() {
        assert!(!matches("Zccache-Daemon"));
    }
}
