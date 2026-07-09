//! Windows Defender exclusion helper (issue #273).
//!
//! Cold zccache builds on Windows pay a multi-minute penalty when Defender
//! real-time-scans every freshly written `.rmeta` / `.rlib` / `.o` file in
//! the cache directory. The fix is a one-time `Add-MpPreference
//! -ExclusionPath` against the cache root.
//!
//! This module exposes:
//! - [`compute_exclusion_paths`] — the set of paths that should be excluded
//!   for a given cache root (the root itself plus sibling dirs).
//! - [`is_elevated`] — Windows token-elevation check (always `true` off Windows).
//! - [`is_quiet_env`] — honours `ZCCACHE_QUIET=1` to suppress the daemon
//!   first-run banner.
//! - Windows-only [`query_excluded`] / [`add_exclusions`] / [`remove_exclusions`]
//!   — thin wrappers around PowerShell's `Get-MpPreference` /
//!   `Add-MpPreference` / `Remove-MpPreference`. Non-Windows builds get
//!   no-op stubs that report the platform as unsupported.
//!
//! Subprocess errors (PowerShell missing, Defender service down, access
//! denied) surface as [`DefenderError`] and are never treated as fatal at
//! call sites — a perf hint is not worth crashing the daemon.

use std::path::{Path, PathBuf};

/// Suppression env var for the daemon's first-run "not excluded" banner.
///
/// Any non-empty value other than `"0"` silences the banner. CI / scripted
/// callers can set this once and forget. The user-visible `defender-exclusions
/// check` subcommand is unaffected — it always prints.
pub const QUIET_ENV: &str = "ZCCACHE_QUIET";

/// Returns the de-duplicated set of paths that should be on Defender's
/// exclusion list for a given resolved cache root.
///
/// The cache root itself is always included. A sibling `runtime/`
/// directory (managed-binary cache from soldr-style integrations) is
/// included when it currently exists on disk — there is no point asking
/// Defender to exclude a path that the daemon never writes into.
///
/// Pure function — no I/O beyond `Path::exists()`. Stable ordering so the
/// JSON output of `defender-exclusions check` is reproducible.
#[must_use]
pub fn compute_exclusion_paths(cache_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(2);
    out.push(cache_root.to_path_buf());

    if let Some(parent) = cache_root.parent() {
        let runtime = parent.join("runtime");
        if runtime.exists() && runtime != cache_root {
            out.push(runtime);
        }
    }

    out.sort();
    out.dedup();
    out
}

/// True when the running process has elevated (administrator) rights.
///
/// On Windows we query the process token for `TokenElevation`. On every
/// other platform we return `true` — non-Windows callers never need
/// elevation for the Defender flow because the flow is a no-op there.
#[cfg(windows)]
#[must_use]
pub fn is_elevated() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size: u32 = 0;
        #[allow(clippy::cast_possible_truncation)]
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            std::ptr::from_mut::<TOKEN_ELEVATION>(&mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
#[must_use]
pub fn is_elevated() -> bool {
    true
}

/// True when `ZCCACHE_QUIET` is set to a non-empty value other than `"0"`.
#[must_use]
pub fn is_quiet_env() -> bool {
    quiet_value_silences(std::env::var(QUIET_ENV).ok().as_deref())
}

/// Predicate version of [`is_quiet_env`] that operates on a borrowed
/// value instead of reading the process environment — lets unit tests
/// exercise the rule without `unsafe` env mutation.
#[must_use]
pub fn quiet_value_silences(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Emit a single stderr line if the daemon's cache root is not on the
/// Windows Defender exclusion list. Best-effort: any query failure is
/// silenced (the banner is a perf hint, not a diagnostic the operator
/// asked for). No-ops cleanly off Windows and when
/// [`QUIET_ENV`] is set.
///
/// Called from `zccache-daemon`'s `run_server` so the warning lands on
/// the daemon's redirected stderr — eventually the user's terminal — on
/// every fresh start.
pub fn maybe_emit_first_run_banner(cache_root: &Path) {
    if !cfg!(windows) {
        return;
    }
    if is_quiet_env() {
        return;
    }
    let paths = compute_exclusion_paths(cache_root);
    let Ok(statuses) = query_excluded(&paths) else {
        return;
    };
    let cache_root_excluded = statuses
        .iter()
        .find(|s| s.path == cache_root)
        .is_some_and(|s| s.excluded);
    if !cache_root_excluded {
        eprintln!(
            "warning: zccache cache dir is not in Windows Defender exclusion list. \
             Run 'zccache defender-exclusions add' (as administrator) to fix."
        );
    }
}

/// Errors returned by the Windows Defender PowerShell wrappers.
#[derive(Debug)]
pub enum DefenderError {
    /// The current platform is not Windows. Callers should treat this as
    /// a clean no-op (print the platform message, exit 0) rather than a
    /// failure.
    Unsupported,
    /// `powershell.exe` could not be located on PATH.
    PowerShellNotFound,
    /// `powershell.exe` exited non-zero.
    PowerShellFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
    /// PowerShell stdout could not be parsed.
    OutputParse(String),
    /// An I/O error occurred spawning or reading from the child process.
    Io(std::io::Error),
}

impl std::fmt::Display for DefenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "Defender exclusion is Windows-only."),
            Self::PowerShellNotFound => write!(
                f,
                "powershell.exe not found on PATH (required to query Windows Defender)"
            ),
            Self::PowerShellFailed { exit_code, stderr } => {
                write!(f, "powershell exited")?;
                if let Some(code) = exit_code {
                    write!(f, " with code {code}")?;
                }
                let trimmed = stderr.trim();
                if !trimmed.is_empty() {
                    write!(f, ": {trimmed}")?;
                }
                Ok(())
            }
            Self::OutputParse(msg) => write!(f, "failed to parse PowerShell output: {msg}"),
            Self::Io(err) => write!(f, "io error invoking powershell: {err}"),
        }
    }
}

impl std::error::Error for DefenderError {}

impl From<std::io::Error> for DefenderError {
    fn from(value: std::io::Error) -> Self {
        if value.kind() == std::io::ErrorKind::NotFound {
            Self::PowerShellNotFound
        } else {
            Self::Io(value)
        }
    }
}

/// Status of a single path in Defender's exclusion list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusionStatus {
    pub path: PathBuf,
    pub excluded: bool,
}

#[cfg(windows)]
mod windows_impl {
    use super::{DefenderError, ExclusionStatus};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Run `Get-MpPreference | Select-Object -ExpandProperty ExclusionPath`
    /// once and check membership locally. One subprocess regardless of how
    /// many paths are being checked — Defender's exclusion list is small
    /// and stable.
    pub fn query_excluded(paths: &[PathBuf]) -> Result<Vec<ExclusionStatus>, DefenderError> {
        let raw = run_powershell(&[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-MpPreference).ExclusionPath",
        ])?;
        let excluded: Vec<String> = raw
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        Ok(paths
            .iter()
            .map(|p| ExclusionStatus {
                path: p.clone(),
                excluded: path_matches_any(p, &excluded),
            })
            .collect())
    }

    pub fn add_exclusions(paths: &[PathBuf]) -> Result<(), DefenderError> {
        for p in paths {
            let arg = quote_for_powershell(p);
            run_powershell(&[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Add-MpPreference -ExclusionPath {arg}"),
            ])?;
        }
        Ok(())
    }

    pub fn remove_exclusions(paths: &[PathBuf]) -> Result<(), DefenderError> {
        for p in paths {
            let arg = quote_for_powershell(p);
            run_powershell(&[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Remove-MpPreference -ExclusionPath {arg}"),
            ])?;
        }
        Ok(())
    }

    fn run_powershell(args: &[&str]) -> Result<String, DefenderError> {
        let output = Command::new("powershell.exe").args(args).output()?;
        if !output.status.success() {
            return Err(DefenderError::PowerShellFailed {
                exit_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        String::from_utf8(output.stdout).map_err(|e| DefenderError::OutputParse(e.to_string()))
    }

    /// Defender exclusion paths are stored in whatever form the user
    /// supplied to `Add-MpPreference`. Normalise both sides before
    /// comparing so `C:\Users\me\.zccache` matches `C:/Users/me/.zccache`
    /// and trailing separators don't cause false negatives.
    fn path_matches_any(needle: &Path, haystack: &[String]) -> bool {
        let needle_norm = normalize_for_compare(needle);
        haystack
            .iter()
            .any(|h| normalize_for_compare(Path::new(h)) == needle_norm)
    }

    fn normalize_for_compare(p: &Path) -> String {
        let s: String = p.to_string_lossy().replace('/', "\\");
        let trimmed = s.trim_end_matches('\\');
        trimmed.to_ascii_lowercase()
    }

    fn quote_for_powershell(p: &Path) -> String {
        // Single-quote so PowerShell does not expand `$`; double single
        // quotes inside escape the quote character. Paths in Windows do
        // not contain single quotes in practice, but guard anyway.
        let s = p.to_string_lossy();
        format!("'{}'", s.replace('\'', "''"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn normalize_for_compare_canonicalizes() {
            assert_eq!(
                normalize_for_compare(Path::new("C:/Users/me/.zccache")),
                normalize_for_compare(Path::new("C:\\Users\\me\\.zccache\\"))
            );
        }

        #[test]
        fn path_matches_any_is_case_insensitive() {
            let cands = vec!["C:\\Users\\Me\\.zccache".to_string()];
            assert!(path_matches_any(Path::new("c:/users/me/.zccache"), &cands));
        }

        #[test]
        fn quote_for_powershell_escapes_single_quote() {
            let p = Path::new("C:/it's/weird");
            assert_eq!(quote_for_powershell(p), "'C:/it''s/weird'");
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{add_exclusions, query_excluded, remove_exclusions};

#[cfg(not(windows))]
pub fn query_excluded(_paths: &[PathBuf]) -> Result<Vec<ExclusionStatus>, DefenderError> {
    Err(DefenderError::Unsupported)
}

#[cfg(not(windows))]
pub fn add_exclusions(_paths: &[PathBuf]) -> Result<(), DefenderError> {
    Err(DefenderError::Unsupported)
}

#[cfg(not(windows))]
pub fn remove_exclusions(_paths: &[PathBuf]) -> Result<(), DefenderError> {
    Err(DefenderError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn compute_paths_includes_cache_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        std::fs::create_dir_all(&root).unwrap();

        let paths = compute_exclusion_paths(&root);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], root);
    }

    #[test]
    fn compute_paths_picks_up_sibling_runtime() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();

        let paths = compute_exclusion_paths(&root);
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|p| p == &root));
        assert!(paths.iter().any(|p| p == &runtime));
    }

    #[test]
    fn compute_paths_skips_missing_runtime() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        std::fs::create_dir_all(&root).unwrap();

        let paths = compute_exclusion_paths(&root);
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn compute_paths_is_deduped_and_sorted() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        std::fs::create_dir_all(&root).unwrap();

        let paths = compute_exclusion_paths(&root);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn quiet_value_silences_only_for_meaningful_values() {
        assert!(!quiet_value_silences(None));
        assert!(!quiet_value_silences(Some("")));
        assert!(!quiet_value_silences(Some("0")));
        assert!(quiet_value_silences(Some("1")));
        assert!(quiet_value_silences(Some("true")));
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_is_elevated_true() {
        // Non-Windows always reports elevated so the Defender flow no-ops
        // cleanly on macOS / Linux.
        assert!(is_elevated());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_query_returns_unsupported() {
        let err = query_excluded(&[PathBuf::from("/tmp/x")]).unwrap_err();
        assert!(matches!(err, DefenderError::Unsupported));
        assert_eq!(format!("{err}"), "Defender exclusion is Windows-only.");
    }
}
