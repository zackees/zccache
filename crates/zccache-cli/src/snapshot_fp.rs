//! Per-crate fingerprint revalidation for `zccache`-managed target snapshots.
//!
//! ## Why this exists
//!
//! `actions/checkout` writes every source file with mtime ≈ "now", and
//! `actions/cache` brings a `target/` snapshot back with mtimes from when
//! that snapshot was originally tarred (always in the past). Cargo's
//! freshness check (`source.mtime > dep_info.mtime → rebuild`) then sees
//! every source as newer than every fingerprint and cold-rebuilds the
//! whole workspace. The `action.yml` workaround — touching every file in
//! `target/` to "now + 1 minute" — defeats the check too aggressively:
//! cargo trusts the cache even for crates whose source actually changed,
//! silently linking the previous commit's compiled binary against the new
//! source tree.
//!
//! The optimal fix is to use a content hash instead of mtime to decide
//! freshness. Stable cargo doesn't support that (`-Z checksum-freshness`
//! is nightly-only), so we replicate the same idea in a sidecar:
//!
//!   1. **Save side** (`snapshot_fp::record`): before tarring, walk every
//!      `target/debug/.fingerprint/<crate>-*` directory. For each, find
//!      the matching `target/debug/deps/<crate>-*.d` Makefile dep file
//!      (rustc-emitted, easy to parse) and blake3-hash every tracked
//!      source under the workspace root. Emit a JSON sidecar at
//!      `<target>/<MANIFEST_FILENAME>` listing per-crate (fp_dir,
//!      dep_info_files, source_path → blake3) entries.
//!
//!   2. **Restore side** (`snapshot_fp::validate`): after the snapshot is
//!      extracted, recompute current hashes for every source in the
//!      manifest (parallel via rayon). For each crate where **every**
//!      tracked source still matches its recorded hash, future-stamp its
//!      `dep_info_files` so cargo trusts the cache. Crates with any
//!      mismatch are left alone — cargo's normal stale check then
//!      correctly rebuilds them.
//!
//! Output mtimes (`.rlib`/`.rmeta`/build artifacts) are handled by the
//! existing post-restore touch step in `action.yml`, which already
//! excludes `.fingerprint/` (see #279). The two layers compose cleanly:
//! outputs always future-stamped; fingerprints future-stamped *only* for
//! crates we can prove are still fresh.

use std::collections::BTreeMap;
use std::fs::FileTimes;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Default name of the sidecar manifest, written under `<target>/`. Lives
/// inside `target/` so it rides the existing snapshot tar without needing
/// special handling in `prepare-target-snapshot.sh`.
pub const MANIFEST_FILENAME: &str = ".zccache-fp-manifest.json";

/// Manifest schema version. Bump if you change the layout. `validate`
/// refuses to operate on unknown versions and exits cleanly so a snapshot
/// saved by an older daemon doesn't poison a new run.
const MANIFEST_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug)]
pub struct Manifest {
    pub version: u32,
    /// Informational; ISO8601 timestamp at record time.
    pub saved_at: String,
    /// Absolute workspace root recorded at save time. The manifest stores
    /// every source path relative to this so restore on a different runner
    /// (different home dir, etc.) can re-anchor.
    pub workspace_root: String,
    pub crates: Vec<CrateEntry>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CrateEntry {
    /// Directory name under `target/<profile>/.fingerprint/`, e.g.
    /// `zccache-daemon-0020565b109aa8e3`.
    pub fingerprint_dir: String,
    /// File names within `fingerprint_dir` that cargo treats as the
    /// staleness anchor for this crate (`dep-*` files). All of these get
    /// touched together if the crate is unchanged.
    pub dep_info_files: Vec<String>,
    /// Workspace-relative source paths → blake3 hex digest.
    pub sources: BTreeMap<String, String>,
}

/// Walk `target/<profile>/.fingerprint/` and the sibling `deps/` directory
/// to build the manifest. Hashing runs in parallel.
pub fn record(
    target_dir: &Path,
    workspace_root: &Path,
    manifest_path: &Path,
    profile: &str,
) -> io::Result<RecordStats> {
    let fp_root = target_dir.join(profile).join(".fingerprint");
    let deps_root = target_dir.join(profile).join("deps");
    if !fp_root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("fingerprint dir not found: {}", fp_root.display()),
        ));
    }

    // Phase 1: enumerate (fingerprint_dir, dep_info_files, .d_file) triples.
    let crate_dirs: Vec<(String, Vec<String>, PathBuf)> = std::fs::read_dir(&fp_root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let dir_name = e.file_name().to_string_lossy().into_owned();
            let dep_info_files: Vec<String> = std::fs::read_dir(e.path())
                .ok()?
                .filter_map(|f| f.ok())
                .map(|f| f.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("dep-"))
                .collect();
            if dep_info_files.is_empty() {
                return None;
            }
            let d_file = deps_root.join(format!("{dir_name}.d"));
            if !d_file.is_file() {
                return None;
            }
            Some((dir_name, dep_info_files, d_file))
        })
        .collect();

    // Phase 2: parse each .d file and hash every workspace source. Parallel
    // across crates AND across sources within a crate.
    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let entries: Vec<CrateEntry> = crate_dirs
        .par_iter()
        .filter_map(|(dir_name, dep_info_files, d_file)| {
            let content = std::fs::read_to_string(d_file).ok()?;
            let sources = parse_d_file(&content);
            let hashed: BTreeMap<String, String> = sources
                .par_iter()
                .filter_map(|src| {
                    let canon = src.canonicalize().ok()?;
                    let rel = canon.strip_prefix(&workspace_root).ok()?;
                    // Skip cargo registry / git-vendored deps: they're pinned
                    // by Cargo.lock (which is part of the cache key), so they
                    // can't drift between save and restore. Hashing them just
                    // bloats the manifest.
                    if is_vendored_dep(rel) {
                        return None;
                    }
                    let bytes = std::fs::read(&canon).ok()?;
                    let hash = blake3::hash(&bytes).to_hex().to_string();
                    let key = path_to_unix(rel);
                    Some((key, hash))
                })
                .collect();
            if hashed.is_empty() {
                // Crate has no workspace-local sources (pure registry deps).
                // We can't prove freshness from workspace content, so omit
                // the crate from the manifest entirely → validate leaves
                // its fingerprint alone (cargo's normal check applies).
                return None;
            }
            Some(CrateEntry {
                fingerprint_dir: dir_name.clone(),
                dep_info_files: dep_info_files.clone(),
                sources: hashed,
            })
        })
        .collect();

    let stats = RecordStats {
        crates_recorded: entries.len(),
        sources_hashed: entries.iter().map(|c| c.sources.len()).sum(),
    };

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        saved_at: chrono_like_now(),
        workspace_root: path_to_unix(&workspace_root),
        crates: entries,
    };

    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string(&manifest)
        .map_err(|e| io::Error::other(format!("serialize manifest: {e}")))?;
    std::fs::write(manifest_path, serialized)?;

    Ok(stats)
}

#[derive(Debug, Default)]
pub struct RecordStats {
    pub crates_recorded: usize,
    pub sources_hashed: usize,
}

/// Read the manifest written by `record` and selectively bump `dep-*`
/// mtimes for crates whose tracked sources still match their recorded
/// hash. Crates with any mismatch are left untouched (cargo will detect
/// staleness via its normal `source.mtime > dep_info.mtime` check).
pub fn validate(
    target_dir: &Path,
    workspace_root: &Path,
    manifest_path: &Path,
    profile: &str,
    stamp_seconds_ahead: u64,
) -> io::Result<ValidateStats> {
    let manifest_bytes = match std::fs::read(manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // No manifest in the snapshot — snapshot was saved by an
            // older daemon. Safe behaviour: do nothing; cargo's normal
            // mtime check fires (overbuild but correct).
            return Ok(ValidateStats::default());
        }
        Err(e) => return Err(e),
    };
    let manifest: Manifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                path = %manifest_path.display(),
                "manifest parse failed, leaving fingerprints alone: {e}"
            );
            return Ok(ValidateStats::default());
        }
    };
    if manifest.version != MANIFEST_VERSION {
        tracing::warn!(
            recorded = manifest.version,
            current = MANIFEST_VERSION,
            "manifest version mismatch, leaving fingerprints alone",
        );
        return Ok(ValidateStats::default());
    }

    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let fp_root = target_dir.join(profile).join(".fingerprint");
    let stamp_time = SystemTime::now() + Duration::from_secs(stamp_seconds_ahead);
    let file_times = FileTimes::new()
        .set_modified(stamp_time)
        .set_accessed(stamp_time);

    let results: Vec<bool> = manifest
        .crates
        .par_iter()
        .map(|entry| {
            let all_unchanged = entry.sources.par_iter().all(|(rel, expected_hex)| {
                let abs = workspace_root.join(rel);
                match std::fs::read(&abs) {
                    Ok(bytes) => blake3::hash(&bytes).to_hex().to_string() == *expected_hex,
                    Err(_) => false,
                }
            });
            if all_unchanged {
                for dep_file in &entry.dep_info_files {
                    let path = fp_root.join(&entry.fingerprint_dir).join(dep_file);
                    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                        let _ = file.set_times(file_times);
                    }
                }
            }
            all_unchanged
        })
        .collect();

    Ok(ValidateStats {
        crates_total: manifest.crates.len(),
        crates_clean: results.iter().filter(|c| **c).count(),
    })
}

#[derive(Debug, Default)]
pub struct ValidateStats {
    pub crates_total: usize,
    pub crates_clean: usize,
}

impl ValidateStats {
    pub fn crates_dirty(&self) -> usize {
        self.crates_total - self.crates_clean
    }
}

/// Parse rustc's Makefile-format `.d` dep file. Format:
///
/// ```text
/// <output_path>: <dep1> <dep2> <dep3...>
/// ```
///
/// Spaces inside paths are backslash-escaped (`\ `), per Make convention.
/// Multiple lines may appear; cargo emits one line per output product.
fn parse_d_file(content: &str) -> Vec<PathBuf> {
    let mut out = std::collections::HashSet::<PathBuf>::new();
    for line in content.lines() {
        // Find the ": " that separates outputs from dependencies. Windows
        // paths contain ":" (drive letter), so we can't just split on ':'.
        let Some(idx) = line.find(": ") else { continue };
        let deps_str = &line[idx + 2..];
        let mut buf = String::new();
        let mut chars = deps_str.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek().map(|&n| n == ' ').unwrap_or(false) {
                buf.push(' ');
                chars.next();
            } else if c.is_whitespace() {
                if !buf.is_empty() {
                    out.insert(PathBuf::from(&buf));
                    buf.clear();
                }
            } else {
                buf.push(c);
            }
        }
        if !buf.is_empty() {
            out.insert(PathBuf::from(&buf));
        }
    }
    out.into_iter().collect()
}

/// Path → forward-slash UTF-8 string. The manifest must be cross-platform
/// (Linux save, Windows restore, and vice-versa). Also strips any Windows
/// extended-length prefix (`\\?\`) that `canonicalize` adds, so the same
/// manifest reads cleanly on either OS.
fn path_to_unix(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.replace('\\', "/")
}

/// True if `rel` (workspace-relative) points into a cargo registry or git
/// vendored-dep tree. Those are pinned by `Cargo.lock` and don't drift
/// between snapshot save and restore.
fn is_vendored_dep(rel: &Path) -> bool {
    let s = path_to_unix(rel);
    s.starts_with(".cargo/registry/")
        || s.starts_with(".cargo/git/")
        || s.contains("/.cargo/registry/")
        || s.contains("/.cargo/git/")
}

/// Best-effort ISO-8601-ish timestamp without pulling chrono in as a dep.
fn chrono_like_now() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn make_synthetic_target(root: &Path, source_content: &str) -> (PathBuf, PathBuf) {
        let workspace = root.join("workspace");
        let target = workspace.join("target");
        let profile = "debug";
        let fp_dir = target.join(profile).join(".fingerprint").join("foo-abcd");
        let deps_dir = target.join(profile).join("deps");
        std::fs::create_dir_all(&fp_dir).unwrap();
        std::fs::create_dir_all(&deps_dir).unwrap();
        // Synthetic source under workspace
        let src = workspace.join("crates/foo/src/lib.rs");
        write(&src, source_content);
        // dep-* file (cargo's binary-format anchor — we only use its mtime)
        write(&fp_dir.join("dep-lib-foo"), "dummy");
        // .d makefile (the file `record` actually parses)
        let canon_src = src.canonicalize().unwrap();
        let d_content = format!(
            "{}: {}\n",
            deps_dir.join("libfoo-abcd.rmeta").display(),
            canon_src.display()
        );
        write(&deps_dir.join("foo-abcd.d"), &d_content);
        (workspace, target)
    }

    #[test]
    fn record_and_validate_unchanged_crate_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, target) = make_synthetic_target(tmp.path(), "fn main() {}\n");

        let manifest = target.join(MANIFEST_FILENAME);
        let stats = record(&target, &workspace, &manifest, "debug").unwrap();
        assert_eq!(stats.crates_recorded, 1);
        assert_eq!(stats.sources_hashed, 1);

        // Mtime before validate
        let dep_path = target.join("debug/.fingerprint/foo-abcd/dep-lib-foo");
        let before = std::fs::metadata(&dep_path).unwrap().modified().unwrap();

        // Validate without modifying source — should touch the dep-info.
        let stats = validate(&target, &workspace, &manifest, "debug", 60).unwrap();
        assert_eq!(stats.crates_clean, 1);
        assert_eq!(stats.crates_dirty(), 0);

        let after = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        assert!(after > before, "dep-info mtime should have been bumped");
    }

    #[test]
    fn validate_dirty_crate_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, target) = make_synthetic_target(tmp.path(), "fn main() {}\n");
        let manifest = target.join(MANIFEST_FILENAME);
        record(&target, &workspace, &manifest, "debug").unwrap();

        let dep_path = target.join("debug/.fingerprint/foo-abcd/dep-lib-foo");
        let before = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        // Simulate a source edit (different content → different hash).
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(
            workspace.join("crates/foo/src/lib.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();

        let stats = validate(&target, &workspace, &manifest, "debug", 60).unwrap();
        assert_eq!(
            stats.crates_clean, 0,
            "modified source should leave crate dirty"
        );
        assert_eq!(stats.crates_dirty(), 1);

        let after = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        assert_eq!(after, before, "dirty crate's dep-info mtime must not move");
    }

    #[test]
    fn validate_missing_manifest_is_silent_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let stats = validate(
            tmp.path(),
            tmp.path(),
            &tmp.path().join(".missing"),
            "debug",
            60,
        )
        .unwrap();
        assert_eq!(stats.crates_total, 0);
    }

    #[test]
    fn parse_d_file_handles_unix_and_windows_paths() {
        let content = "/tmp/out: /a/b.rs /a/c.rs\nC:\\out: C:\\a\\b.rs C:\\a\\c.rs\n";
        let parsed = parse_d_file(content);
        assert!(parsed.iter().any(|p| p == Path::new("/a/b.rs")));
        assert!(parsed.iter().any(|p| p == Path::new("/a/c.rs")));
        assert!(parsed.iter().any(|p| p == Path::new("C:\\a\\b.rs")));
        assert!(parsed.iter().any(|p| p == Path::new("C:\\a\\c.rs")));
    }

    #[test]
    fn parse_d_file_handles_escaped_spaces() {
        // Path with literal space in it: "/a path/foo.rs"
        let content = "/out: /a\\ path/foo.rs\n";
        let parsed = parse_d_file(content);
        assert!(parsed.iter().any(|p| p == Path::new("/a path/foo.rs")));
    }
}
