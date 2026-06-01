//! Cross-platform path utilities.
//!
//! Handles path normalization, case sensitivity, and platform differences.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A normalized, platform-aware path representation.
///
/// On case-insensitive filesystems (Windows, default macOS), paths are
/// stored in a canonical form for consistent cache keying.
#[derive(Debug, Clone)]
pub struct NormalizedPath {
    /// The original path, normalized but preserving original casing.
    path: PathBuf,
    /// Lowercased version for case-insensitive comparison, if applicable.
    case_key: Option<String>,
}

impl PartialEq for NormalizedPath {
    fn eq(&self, other: &Self) -> bool {
        normalize_for_key(&self.path) == normalize_for_key(&other.path)
    }
}

impl PartialEq<PathBuf> for NormalizedPath {
    fn eq(&self, other: &PathBuf) -> bool {
        self == &Self::new(other)
    }
}

impl PartialEq<NormalizedPath> for PathBuf {
    fn eq(&self, other: &NormalizedPath) -> bool {
        other == self
    }
}

impl PartialEq<Path> for NormalizedPath {
    fn eq(&self, other: &Path) -> bool {
        self == &Self::new(other)
    }
}

impl PartialEq<&Path> for NormalizedPath {
    fn eq(&self, other: &&Path) -> bool {
        self == *other
    }
}

impl PartialEq<NormalizedPath> for Path {
    fn eq(&self, other: &NormalizedPath) -> bool {
        other == self
    }
}

impl PartialEq<&NormalizedPath> for Path {
    fn eq(&self, other: &&NormalizedPath) -> bool {
        *other == self
    }
}

impl PartialEq<&PathBuf> for NormalizedPath {
    fn eq(&self, other: &&PathBuf) -> bool {
        self == *other
    }
}

impl PartialEq<&NormalizedPath> for PathBuf {
    fn eq(&self, other: &&NormalizedPath) -> bool {
        *other == self
    }
}

impl Eq for NormalizedPath {}

impl Hash for NormalizedPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        normalize_for_key(&self.path).hash(state);
    }
}

impl PartialOrd for NormalizedPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NormalizedPath {
    fn cmp(&self, other: &Self) -> Ordering {
        normalize_for_key(&self.path).cmp(&normalize_for_key(&other.path))
    }
}

impl NormalizedPath {
    /// Create a new normalized path.
    ///
    /// On Windows, this also computes a lowercase key for case-insensitive matching.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = normalize(path.as_ref());
        let case_key = if cfg!(windows) || cfg!(target_os = "macos") {
            Some(normalize_for_key(&path))
        } else {
            None
        };
        Self { path, case_key }
    }

    /// Returns the underlying path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Returns the case-insensitive comparison key, if applicable.
    #[must_use]
    pub fn case_key(&self) -> Option<&str> {
        self.case_key.as_deref()
    }

    /// Convert back to an owned normalized `PathBuf`.
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }

    /// Join a path segment onto this normalized path.
    #[must_use]
    pub fn join(&self, path: impl AsRef<Path>) -> Self {
        Self::new(self.path.join(path))
    }
}

impl AsRef<Path> for NormalizedPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl AsRef<OsStr> for NormalizedPath {
    fn as_ref(&self) -> &OsStr {
        self.as_path().as_os_str()
    }
}

impl Deref for NormalizedPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl From<PathBuf> for NormalizedPath {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}

impl From<&Path> for NormalizedPath {
    fn from(path: &Path) -> Self {
        Self::new(path)
    }
}

impl From<String> for NormalizedPath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<&str> for NormalizedPath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<&String> for NormalizedPath {
    fn from(path: &String) -> Self {
        Self::new(path)
    }
}

impl Serialize for NormalizedPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.path.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NormalizedPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        PathBuf::deserialize(deserializer).map(Self::new)
    }
}

/// Normalize a path by resolving `.` and `..` components without
/// touching the filesystem (no symlink resolution).
///
/// This is intentionally not `canonicalize()` --- we avoid filesystem
/// access and symlink resolution for performance and determinism.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

/// Normalize a path into a stable string key for hashing and comparisons.
///
/// This is the shared representation for path-based cache keys. It avoids
/// filesystem access, strips Windows extended-length prefixes, normalizes
/// separators, and folds case on case-insensitive platforms.
#[must_use]
pub fn normalize_for_key(path: &Path) -> String {
    let normalized = normalize(path);

    #[cfg(windows)]
    {
        let mut s = normalized.to_string_lossy().replace('\\', "/");
        if let Some(stripped) = s.strip_prefix("//?/") {
            s = stripped.to_string();
        }
        s.make_ascii_lowercase();
        s
    }

    #[cfg(target_os = "macos")]
    {
        normalized.to_string_lossy().to_lowercase()
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // Issue #550: zero-copy `OsString::into_string()` when the path is
        // valid UTF-8 (always true for the C/C++ headers in the hot
        // `compute_artifact_key` loop). Falls back to lossy conversion if
        // not — preserves prior `to_string_lossy().into_owned()` behavior.
        // Saves one `String` allocation per call on Linux (~500 alloc/dealloc
        // pairs per cpp-inline cold compile of `<iostream>`-bearing files).
        normalized
            .into_os_string()
            .into_string()
            .unwrap_or_else(|os| os.to_string_lossy().into_owned())
    }
}

/// Return a compact, stable identifier for a path.
///
/// This is intended for filesystem-derived runtime names such as Windows named
/// pipes where the full normalized path may be too long or contain invalid
/// characters. It is not a cryptographic digest.
#[must_use]
pub fn stable_path_id(path: &Path) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let key = normalize_for_key(path);
    let mut hash = FNV_OFFSET;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Convert an MSYS2/Git Bash style path to a native Windows path.
///
/// `/c/Users/foo` → `C:\Users\foo`
///
/// On non-Windows platforms, returns the input unchanged.
/// On Windows, only converts paths matching the MSYS pattern `/<letter>/...`.
/// Already-native paths (e.g., `C:\...`) pass through unchanged.
#[must_use]
pub fn normalize_msys_path(path: &str) -> String {
    #[cfg(windows)]
    {
        let bytes = path.as_bytes();
        // Match pattern: /X/ or /X (end of string) where X is a-zA-Z
        if bytes.len() >= 2
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && (bytes.len() == 2 || bytes[2] == b'/')
        {
            let drive = (bytes[1] as char).to_ascii_uppercase();
            let rest = if bytes.len() > 2 { &path[2..] } else { "" };
            return format!("{drive}:{rest}").replace('/', "\\");
        }
        path.to_string()
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_removes_dot() {
        let p = normalize(Path::new("a/./b/c"));
        assert_eq!(p, PathBuf::from("a/b/c"));
    }

    #[test]
    fn normalize_resolves_dotdot() {
        let p = normalize(Path::new("a/b/../c"));
        assert_eq!(p, PathBuf::from("a/c"));
    }

    #[cfg(windows)]
    #[test]
    fn normalize_for_key_windows_equivalent_spellings_match() {
        let a = normalize_for_key(Path::new(r"\\?\C:\Work\src\..\src\main.cpp"));
        let b = normalize_for_key(Path::new("c:/work/src/main.cpp"));
        assert_eq!(a, b);
    }

    #[test]
    fn msys_path_drive_letter() {
        let result = normalize_msys_path("/c/Users/foo/bar");
        #[cfg(windows)]
        assert_eq!(result, r"C:\Users\foo\bar");
        #[cfg(not(windows))]
        assert_eq!(result, "/c/Users/foo/bar");
    }

    #[test]
    fn msys_path_uppercase_drive() {
        let result = normalize_msys_path("/D/project/build");
        #[cfg(windows)]
        assert_eq!(result, r"D:\project\build");
        #[cfg(not(windows))]
        assert_eq!(result, "/D/project/build");
    }

    #[test]
    fn msys_path_bare_drive() {
        let result = normalize_msys_path("/c");
        #[cfg(windows)]
        assert_eq!(result, "C:");
        #[cfg(not(windows))]
        assert_eq!(result, "/c");
    }

    #[test]
    fn native_windows_path_unchanged() {
        let result = normalize_msys_path(r"C:\Users\foo\bar");
        assert_eq!(result, r"C:\Users\foo\bar");
    }

    #[test]
    fn relative_path_unchanged() {
        let result = normalize_msys_path("relative/path");
        assert_eq!(result, "relative/path");
    }

    #[test]
    fn empty_path_unchanged() {
        let result = normalize_msys_path("");
        assert_eq!(result, "");
    }

    #[test]
    fn unix_absolute_path_not_drive() {
        // /usr/bin/gcc — bytes[2] is 's', not '/', so NOT a drive letter path
        let result = normalize_msys_path("/usr/bin/gcc");
        assert_eq!(result, "/usr/bin/gcc");
    }

    #[test]
    fn stable_path_id_is_compact_and_deterministic() {
        let path = Path::new("a/./b/../cache");
        assert_eq!(stable_path_id(path), stable_path_id(path));
        assert_eq!(stable_path_id(path).len(), 16);
    }
}
