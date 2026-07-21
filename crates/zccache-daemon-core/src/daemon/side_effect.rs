//! Before/after directory scan for link side-effect file detection.
//!
//! When compiler wrappers (e.g., ctc-clang++) link a binary, their post-link
//! hooks may deploy runtime DLLs, PDBs, Emscripten sidecars, or other files
//! alongside the output. These "side-effect" files are not declared in linker
//! arguments, so they cannot be discovered by parsing. Instead, we snapshot
//! the output directory before and after the link, and treat new or changed
//! sibling files as side effects to cache.

use crate::core::NormalizedPath;
use crate::hash::{hash_file, ContentHash};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

/// Maximum size (bytes) for a single side-effect file. Larger files make the
/// whole link result uncacheable: storing a partial output set is incorrect.
const MAX_SIDE_EFFECT_SIZE: u64 = 50 * 1024 * 1024; // 50 MB

/// Maximum number of side-effect files captured per link invocation. Exceeding
/// this makes the whole link result uncacheable.
const MAX_SIDE_EFFECT_COUNT: usize = 10;

/// Snapshot of a directory: filename → (size, mtime).
#[derive(Default)]
pub struct DirSnapshot {
    entries: HashMap<std::ffi::OsString, FileEntry>,
}

struct FileEntry {
    size: u64,
    modified: SystemTime,
    content_hash: ContentHash,
}

/// A detected side-effect file ready to be cached.
#[derive(Debug)]
pub struct SideEffectFile {
    pub path: NormalizedPath,
    pub file_name: std::ffi::OsString,
    pub content_hash: ContentHash,
}

/// Result of scanning a link output directory for implicit outputs.
///
/// A successful linker invocation may still be uncacheable when zccache cannot
/// retain the complete output set. Callers must return the linker result but
/// skip cache population for [`Self::Uncacheable`].
pub enum SideEffectScan {
    Complete(Vec<SideEffectFile>),
    Uncacheable { reason: String },
}

fn stable_file_entry(path: &Path) -> std::io::Result<FileEntry> {
    let before = std::fs::metadata(path)?;
    let content_hash = hash_file(path)?;
    let after = std::fs::metadata(path)?;
    let before_modified = before.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let after_modified = after.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if before.len() != after.len() || before_modified != after_modified {
        return Err(std::io::Error::other(format!(
            "file changed while snapshotting: {}",
            path.display()
        )));
    }
    Ok(FileEntry {
        size: after.len(),
        modified: after_modified,
        content_hash,
    })
}

/// Capture the current state of `dir`.
///
/// A nonexistent directory is empty because a linker may create it. Any other
/// scan error is returned so the caller can fail cache population closed.
pub fn snapshot_directory(dir: &Path) -> std::io::Result<DirSnapshot> {
    let mut snap = DirSnapshot::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(snap),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            let path = entry.path();
            snap.entries
                .insert(entry.file_name(), stable_file_entry(&path)?);
        }
    }
    Ok(snap)
}

/// Re-scan `dir` and return files that are new or changed since `before`,
/// excluding `primary_name` and anything in `already_captured`.
pub fn detect_side_effects(
    before: &DirSnapshot,
    dir: &Path,
    primary_name: &OsStr,
    already_captured: &HashSet<std::ffi::OsString>,
) -> std::io::Result<SideEffectScan> {
    let mut results = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();

        // Skip the primary output and already-captured secondaries.
        if name == primary_name || already_captured.contains(&name) {
            continue;
        }

        if !entry.metadata()?.is_file() {
            continue;
        }
        let path = entry.path();
        let current = stable_file_entry(&path)?;

        // Strong content identity catches same-size changes on filesystems
        // whose timestamp resolution is too coarse to distinguish rewrites.
        if let Some(prev) = before.entries.get(&name) {
            if prev.size == current.size
                && prev.modified == current.modified
                && prev.content_hash == current.content_hash
            {
                continue;
            }
        }

        if current.size > MAX_SIDE_EFFECT_SIZE {
            return Ok(SideEffectScan::Uncacheable {
                reason: format!(
                    "side-effect {} exceeds {MAX_SIDE_EFFECT_SIZE}-byte limit",
                    name.to_string_lossy()
                ),
            });
        }

        results.push(SideEffectFile {
            path: path.into(),
            file_name: name,
            content_hash: current.content_hash,
        });

        if results.len() > MAX_SIDE_EFFECT_COUNT {
            return Ok(SideEffectScan::Uncacheable {
                reason: format!("side-effect count exceeds {MAX_SIDE_EFFECT_COUNT}"),
            });
        }
    }

    Ok(SideEffectScan::Complete(results))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn snapshot(dir: &Path) -> DirSnapshot {
        snapshot_directory(dir).unwrap()
    }

    fn complete(scan: SideEffectScan) -> Vec<SideEffectFile> {
        match scan {
            SideEffectScan::Complete(files) => files,
            SideEffectScan::Uncacheable { reason } => panic!("unexpected uncacheable scan: {reason}"),
        }
    }

    #[test]
    fn new_dll_detected() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path());

        // Simulate linker deploying a DLL after the snapshot.
        fs::write(dir.path().join("asan_runtime.dll"), b"fake dll").unwrap();

        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap(),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name, "asan_runtime.dll");
    }

    #[test]
    fn preexisting_dll_not_detected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.dll"), b"old dll").unwrap();

        let snap = snapshot(dir.path());

        // No changes after snapshot.
        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap(),
        );

        assert!(found.is_empty());
    }

    #[test]
    fn primary_output_excluded() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path());

        fs::write(dir.path().join("app.dll"), b"primary").unwrap();

        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.dll"), &HashSet::new()).unwrap(),
        );

        assert!(found.is_empty());
    }

    #[test]
    fn already_captured_excluded() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path());

        fs::write(dir.path().join("foo.dll"), b"secondary").unwrap();

        let mut captured = HashSet::new();
        captured.insert(std::ffi::OsString::from("foo.dll"));

        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &captured).unwrap(),
        );

        assert!(found.is_empty());
    }

    #[test]
    fn non_shared_sibling_detected() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path());

        fs::write(dir.path().join("debug.pdb"), b"pdb data").unwrap();
        fs::write(dir.path().join("build.log"), b"log data").unwrap();
        fs::write(dir.path().join("output.obj"), b"obj data").unwrap();

        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap(),
        );

        assert_eq!(found.len(), 3);
        let names: HashSet<_> = found.iter().map(|f| f.file_name.clone()).collect();
        assert!(names.contains(OsStr::new("debug.pdb")));
        assert!(names.contains(OsStr::new("build.log")));
        assert!(names.contains(OsStr::new("output.obj")));
    }

    #[test]
    fn size_limit_enforced() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path());

        // Create a file exceeding MAX_SIDE_EFFECT_SIZE (use sparse-like approach).
        let big_path = dir.path().join("huge.dll");
        let f = fs::File::create(&big_path).unwrap();
        f.set_len(MAX_SIDE_EFFECT_SIZE + 1).unwrap();

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert!(matches!(found, SideEffectScan::Uncacheable { .. }));
    }

    #[test]
    fn count_limit_enforced() {
        let dir = TempDir::new().unwrap();
        let snap = snapshot(dir.path());

        for i in 0..15 {
            fs::write(dir.path().join(format!("lib{i}.dll")), b"dll").unwrap();
        }

        let found =
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap();

        assert!(matches!(found, SideEffectScan::Uncacheable { .. }));
    }

    #[test]
    fn nonexistent_dir_snapshot_is_empty() {
        let snap = snapshot(Path::new("/nonexistent/path/xyz"));
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn changed_dll_detected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("runtime.dll"), b"v1").unwrap();

        let snap = snapshot(dir.path());

        // Overwrite with different content (different size → detected).
        fs::write(dir.path().join("runtime.dll"), b"version2-longer").unwrap();

        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap(),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name, "runtime.dll");
    }

    #[test]
    fn same_size_same_mtime_rewrite_is_detected() {
        let dir = TempDir::new().unwrap();
        let runtime = dir.path().join("runtime.dll");
        fs::write(&runtime, b"v1").unwrap();
        let fixed_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&runtime, fixed_mtime).unwrap();
        let snap = snapshot(dir.path());

        fs::write(&runtime, b"v2").unwrap();
        filetime::set_file_mtime(&runtime, fixed_mtime).unwrap();

        let found = complete(
            detect_side_effects(&snap, dir.path(), OsStr::new("app.exe"), &HashSet::new()).unwrap(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name, "runtime.dll");
    }
}
