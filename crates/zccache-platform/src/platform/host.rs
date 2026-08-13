//! Neutral facts about the machine running zccache.

use std::path::PathBuf;

/// Native Defender command failure without product-specific wording.
#[derive(Debug)]
pub enum DefenderError {
    Unsupported,
    PowerShellNotFound,
    CommandFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
    OutputParse(String),
    Io(std::io::Error),
}

/// Stable Rust host OS identifier.
#[must_use]
pub fn os() -> &'static str {
    crate::platform_imp::host::os()
}

/// Stable Rust host architecture identifier.
#[must_use]
pub fn arch() -> &'static str {
    crate::platform_imp::host::arch()
}

/// Whether this process is running on Windows.
#[must_use]
pub fn is_windows() -> bool {
    os() == "windows"
}

/// Whether this process is running on macOS.
#[must_use]
pub fn is_macos() -> bool {
    os() == "macos"
}

/// Whether this process is running on Linux.
#[must_use]
pub fn is_linux() -> bool {
    os() == "linux"
}

/// Best-effort current user's home directory.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    crate::platform_imp::host::home_dir()
}

/// Best-effort per-user runtime directory for local sockets and locks.
#[must_use]
pub fn runtime_dir() -> Option<String> {
    crate::platform_imp::host::runtime_dir()
}

/// Best-effort current user identifier suitable for endpoint names.
#[must_use]
pub fn current_user() -> Option<String> {
    crate::platform_imp::host::current_user()
}

/// Opaque raw host inputs used by callers to domain-separate native CPU keys.
#[must_use]
pub fn cpu_identity_material() -> String {
    crate::platform_imp::host::cpu_identity_material()
}

/// Logical concurrency exposed by the host.
#[must_use]
pub fn available_parallelism() -> Option<usize> {
    std::thread::available_parallelism()
        .ok()
        .map(std::num::NonZeroUsize::get)
}

/// Whether the current process has host administrator privileges.
#[must_use]
pub fn is_elevated() -> bool {
    crate::platform_imp::host::is_elevated()
}

/// Whether native Defender exclusion management exists on this host.
#[must_use]
pub fn defender_supported() -> bool {
    crate::platform_imp::host::defender_supported()
}

/// Returns the host's configured Defender exclusion paths.
pub fn defender_exclusions() -> Result<Vec<PathBuf>, DefenderError> {
    crate::platform_imp::host::defender_exclusions()
}

/// Adds one native Defender exclusion path.
pub fn add_defender_exclusion(path: &std::path::Path) -> Result<(), DefenderError> {
    crate::platform_imp::host::add_defender_exclusion(path)
}

/// Removes one native Defender exclusion path.
pub fn remove_defender_exclusion(path: &std::path::Path) -> Result<(), DefenderError> {
    crate::platform_imp::host::remove_defender_exclusion(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_identity_facts_are_present_and_stable() {
        assert!(!os().is_empty());
        assert!(!arch().is_empty());
        assert!(!cpu_identity_material().is_empty());
        assert_eq!(cpu_identity_material(), cpu_identity_material());
        assert!(available_parallelism().is_some_and(|value| value >= 1));
    }
}
