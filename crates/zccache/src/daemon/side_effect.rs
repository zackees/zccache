//! Before/after directory scan for link side-effect file detection.
//!
//! When compiler wrappers (e.g., ctc-clang++) link a binary, their post-link
//! hooks may deploy runtime DLLs, PDBs, Emscripten sidecars, or other files
//! alongside the output. These "side-effect" files are not declared in linker
//! arguments, so they cannot be discovered by parsing. Instead, we snapshot
//! the output directory before and after the link, and treat new or changed
//! sibling files as side effects to cache.

use crate::core::NormalizedPath;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

/// Maximum size (bytes) for a single side-effect file. Larger files are skipped.
const MAX_SIDE_EFFECT_SIZE: u64 = 50 * 1024 * 1024; // 50 MB

/// Maximum number of side-effect files captured per link invocation.
const MAX_SIDE_EFFECT_COUNT: usize = 10;

/// Snapshot of a directory: filename → (size, mtime).
#[derive(Default)]
pub struct DirSnapshot {
    entries: HashMap<std::ffi::OsString, FileEntry>,
}

struct FileEntry {
    size: u64,
    modified: SystemTime,
}

/// A detected side-effect file ready to be cached.
pub struct SideEffectFile {
    pub path: NormalizedPath,
    pub file_name: std::ffi::OsString,
}

/// Capture the current state of `dir`. Returns an empty snapshot if the
/// directory does not exist or cannot be read (e.g., it will be created
/// by the linker).
pub fn snapshot_directory(dir: &Path) -> DirSnapshot {
    let mut snap = DirSnapshot::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return snap,
    };
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                snap.entries.insert(
                    entry.file_name(),
                    FileEntry {
                        size: meta.len(),
                        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    },
                );
            }
        }
    }
    snap
}

/// Re-scan `dir` and return files that are new or changed since `before`,
/// excluding `primary_name` and anything in `already_captured`.
pub fn detect_side_effects(
    before: &DirSnapshot,
    dir: &Path,
    primary_name: &OsStr,
    already_captured: &HashSet<std::ffi::OsString>,
) -> std::io::Result<Vec<SideEffectFile>> {
    let mut results = Vec::new();

    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();

        // Skip the primary output and already-captured secondaries.
        if name == primary_name || already_captured.contains(&name) {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };

        // Skip files that existed before the link with the same size+mtime.
        if let Some(prev) = before.entries.get(&name) {
            let same_size = prev.size == meta.len();
            let same_mtime = meta.modified().map(|m| m == prev.modified).unwrap_or(false);
            if same_size && same_mtime {
                continue;
            }
        }

        // Enforce size limit.
        if meta.len() > MAX_SIDE_EFFECT_SIZE {
            tracing::warn!(
                file = %name.to_string_lossy(),
                size = meta.len(),
                limit = MAX_SIDE_EFFECT_SIZE,
                "side-effect file exceeds size limit, skipping"
            );
            continue;
        }

        results.push(SideEffectFile {
            path: entry.path().into(),
            file_name: name,
        });

        if results.len() >= MAX_SIDE_EFFECT_COUNT {
            tracing::warn!(
                limit = MAX_SIDE_EFFECT_COUNT,
                "side-effect file count limit reached, truncating"
            );
            break;
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn new_dll_detected() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot_directory(dir.path());

        // Simulate linker deploying a DLL after the snapshot.
        fs::write(dir.path().join("asan_runtime.dll"), b"fake dll").unwrap();

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name, "asan_runtime.dll");
    }

    #[test]
    fn preexisting_dll_not_detected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.dll"), b"old dll").unwrap();

        let snap = snapshot_directory(dir.path());

        // No changes after snapshot.
        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn primary_output_excluded() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot_directory(dir.path());

        fs::write(dir.path().join("app.dll"), b"primary").unwrap();

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.dll"), &HashSet::new()).unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn already_captured_excluded() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot_directory(dir.path());

        fs::write(dir.path().join("foo.dll"), b"secondary").unwrap();

        let mut captured = HashSet::new();
        captured.insert(std::ffi::OsString::from("foo.dll"));

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &captured).unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn non_shared_sibling_detected() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot_directory(dir.path());

        fs::write(dir.path().join("debug.pdb"), b"pdb data").unwrap();
        fs::write(dir.path().join("build.log"), b"log data").unwrap();
        fs::write(dir.path().join("output.obj"), b"obj data").unwrap();

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert_eq!(found.len(), 3);
        let names: HashSet<_> = found.iter().map(|f| f.file_name.clone()).collect();
        assert!(names.contains(OsStr::new("debug.pdb")));
        assert!(names.contains(OsStr::new("build.log")));
        assert!(names.contains(OsStr::new("output.obj")));
    }

    #[test]
    fn size_limit_enforced() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot_directory(dir.path());

        // Create a file exceeding MAX_SIDE_EFFECT_SIZE (use sparse-like approach).
        let big_path = dir.path().join("huge.dll");
        let f = fs::File::create(&big_path).unwrap();
        f.set_len(MAX_SIDE_EFFECT_SIZE + 1).unwrap();

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn count_limit_enforced() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot_directory(dir.path());

        for i in 0..15 {
            fs::write(dir.path().join(format!("lib{i}.dll")), b"dll").unwrap();
        }

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert_eq!(found.len(), MAX_SIDE_EFFECT_COUNT);
    }

    #[test]
    fn nonexistent_dir_snapshot_is_empty() {
        let snap = snapshot_directory(Path::new("/nonexistent/path/xyz"));
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn changed_dll_detected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("runtime.dll"), b"v1").unwrap();

        let snap = snapshot_directory(dir.path());

        // Overwrite with different content (different size → detected).
        fs::write(dir.path().join("runtime.dll"), b"version2-longer").unwrap();

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name, "runtime.dll");
    }
}
