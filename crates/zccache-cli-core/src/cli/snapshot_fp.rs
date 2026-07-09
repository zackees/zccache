//! Per-crate fingerprint revalidation for `zccache`-managed target snapshots.
//!
//! # Why this exists
//!
//! `actions/checkout` writes every source file with mtime ≈ "now", and
//! `actions/cache` brings a `target/` snapshot back with mtimes from when
//! that snapshot was originally tarred (always in the past). Cargo's
//! freshness check (`source.mtime > dep_info.mtime → rebuild`) then sees
//! every source as newer than every fingerprint anchor and cold-rebuilds
//! the whole workspace. The original `action.yml` workaround — touching
//! every file in `target/` to "now + 1 minute" — defeated the check too
//! aggressively: cargo trusted the cache even for crates whose source had
//! actually changed, silently linking the previous commit's compiled
//! binary against the new source tree (see PR #279 for the bug).
//!
//! The optimal fix is to use a content hash instead of mtime to decide
//! freshness. Stable cargo doesn't support that (`-Z checksum-freshness`
//! is nightly-only), so we replicate the same idea per crate in a sidecar:
//!
//! * **Save side** (`record`): before tarring, walk every cargo fingerprint
//!   dir under `target/<profile>/.fingerprint/`. For each compilation unit
//!   tracked by that fingerprint, find its source-input list and
//!   blake3-hash every workspace-local source. Emit a JSON sidecar at
//!   `<target>/<MANIFEST_FILENAME>` listing per-crate
//!   `(fingerprint_dir, stamp_targets, source_path → blake3)` entries.
//! * **Restore side** (`validate`): rehash every recorded source in
//!   parallel. For each crate where *every* tracked source still matches
//!   its recorded hash, future-stamp that crate's `stamp_targets` so
//!   cargo trusts the cache. Crates with any mismatch are left untouched
//!   so cargo's normal stale check correctly rebuilds them.
//!
//! ## Fingerprint kinds handled
//!
//! 1. **Library / bin / test compilation** (cargo's `CheckDepInfo`
//!    LocalFingerprint). The dep file lives at
//!    `<fp_dir>/dep-{lib,bin,test}-<...>`; the matching `.d` Makefile is
//!    at `target/<profile>/deps/<fp_dir>.d`.
//! 2. **Build script compilation** (also `CheckDepInfo`). The dep file
//!    is `<fp_dir>/dep-build-script-build-script-build`; the `.d` file
//!    lives at `target/<profile>/build/<fp_dir>/build_script_build-<hash>.d`.
//! 3. **Build script execution** (`RerunIfChanged`). No `.d` file; the
//!    json fingerprint lists `paths` relative to the crate's manifest
//!    directory and an `output` file in `target/<profile>/build/<fp_dir>/`
//!    that cargo treats as the freshness anchor.
//!
//! Together these cover every freshness signal cargo uses for workspace
//! crates, so the blanket post-restore `find ... -touch '+1 min'` step in
//! `action.yml` is no longer necessary.

use std::collections::{BTreeMap, HashMap};
use std::fs::FileTimes;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Name of the sidecar manifest, written under `<target>/`. Lives inside
/// `target/` so it rides the existing snapshot tar without needing
/// special handling in `prepare-target-snapshot.sh`.
pub const MANIFEST_FILENAME: &str = ".zccache-fp-manifest.json";

/// Manifest schema version. Bump if you change the layout. `validate`
/// refuses to operate on unknown versions and exits cleanly so a snapshot
/// saved by an older daemon doesn't poison a new run.
///
/// v2: generalised `dep_info_files` (list of names inside the fingerprint
/// dir) into `stamp_targets` (list of paths relative to target_dir) so we
/// can address both the in-fingerprint anchors used by `CheckDepInfo` and
/// the `out`-style anchor files used by `RerunIfChanged`.
const MANIFEST_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Debug)]
pub struct Manifest {
    pub version: u32,
    /// Informational; `epoch:<unix-seconds>` at record time.
    pub saved_at: String,
    /// Workspace root recorded at save time (forward-slash path). Stored
    /// for diagnostics; restore re-anchors against the runtime workspace.
    pub workspace_root: String,
    pub crates: Vec<CrateEntry>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CrateEntry {
    /// Directory name under `target/<profile>/.fingerprint/`, e.g.
    /// `zccache-daemon-0020565b109aa8e3`. Diagnostic only.
    pub fingerprint_dir: String,
    /// Files to future-stamp if *every* recorded source still matches its
    /// hash. Paths are stored relative to `target_dir`. Lib/bin/test
    /// entries point at the in-`.fingerprint/` anchor; build-script-run
    /// entries point at the output file referenced by `RerunIfChanged`.
    pub stamp_targets: Vec<String>,
    /// Workspace-relative source path → blake3 hex digest.
    pub sources: BTreeMap<String, String>,
}

/// Walk `target/<profile>/.fingerprint/` and emit a manifest covering
/// every CheckDepInfo / RerunIfChanged fingerprint with workspace-local
/// source inputs. Hashing runs in parallel.
pub fn record(
    target_dir: &Path,
    workspace_root: &Path,
    manifest_path: &Path,
    profile: &str,
) -> io::Result<RecordStats> {
    let fp_root = target_dir.join(profile).join(".fingerprint");
    if !fp_root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("fingerprint dir not found: {}", fp_root.display()),
        ));
    }
    let deps_root = target_dir.join(profile).join("deps");
    let build_root = target_dir.join(profile).join("build");
    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());

    // For RerunIfChanged we need to resolve `paths` (relative to the
    // crate's manifest directory) against the workspace. Discover the
    // package-name → manifest-dir mapping once up front.
    let manifest_dirs = discover_workspace_crates(&workspace_root);

    // Phase 1: scan each `<fp_dir>/` for processable units.
    let fp_dirs: Vec<PathBuf> = std::fs::read_dir(&fp_root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    // Phase 2: process all units in parallel. A single fingerprint dir
    // can produce *multiple* manifest entries — e.g. one for the build
    // script's compilation (`dep-build-script-build-script-build`) and
    // one for its execution (`run-build-script-build-script-build.json`).
    let entries: Vec<CrateEntry> = fp_dirs
        .par_iter()
        .flat_map(|fp_dir| {
            process_fingerprint_dir(
                fp_dir,
                &fp_root,
                &deps_root,
                &build_root,
                &workspace_root,
                &manifest_dirs,
                profile,
            )
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

/// Inspect a single `<fp_root>/<fp_dir>` and return zero or more entries
/// for the manifest.
fn process_fingerprint_dir(
    fp_dir: &Path,
    fp_root: &Path,
    deps_root: &Path,
    build_root: &Path,
    workspace_root: &Path,
    manifest_dirs: &HashMap<String, PathBuf>,
    profile: &str,
) -> Vec<CrateEntry> {
    let Some(dir_name) = fp_dir.file_name().and_then(|n| n.to_str()) else {
        return vec![];
    };
    let mut entries = Vec::new();

    // List of (dep_file_basename, .d_path) for CheckDepInfo-style units.
    let mut depinfo_units: Vec<(String, PathBuf)> = Vec::new();
    let mut has_run_build_script = false;

    let Ok(read_dir) = std::fs::read_dir(fp_dir) else {
        return vec![];
    };
    for entry in read_dir.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "run-build-script-build-script-build.json" {
            has_run_build_script = true;
        } else if name == "dep-build-script-build-script-build" {
            // Build-script compilation. .d file lives in build/<dir>/.
            if let Some(d_file) = first_buildscript_d_file(build_root, dir_name) {
                depinfo_units.push((name, d_file));
            }
        } else if name.starts_with("dep-") {
            // Lib/bin/test compilation. .d file lives in deps/<dir>.d.
            let d_file = deps_root.join(format!("{dir_name}.d"));
            if d_file.is_file() {
                depinfo_units.push((name, d_file));
            }
        }
    }

    // Group CheckDepInfo units of the same fingerprint dir into one
    // entry, since they share the same source-input list (they're all
    // for the same compilation invocation).
    if !depinfo_units.is_empty() {
        // All depinfo units in the same fp_dir reference the same .d file
        // by definition. Use the first.
        let d_file = depinfo_units[0].1.clone();
        if let Some(sources) = hash_d_file_sources(&d_file, workspace_root) {
            let stamp_targets: Vec<String> = depinfo_units
                .iter()
                .map(|(name, _)| {
                    let abs = fp_root.join(dir_name).join(name);
                    relativize_to_target(&abs, target_dir_from_fp_root(fp_root, profile))
                })
                .collect();
            entries.push(CrateEntry {
                fingerprint_dir: dir_name.to_string(),
                stamp_targets,
                sources,
            });
        }
    }

    if has_run_build_script {
        if let Some(entry) =
            process_run_build_script(fp_dir, dir_name, workspace_root, manifest_dirs)
        {
            entries.push(entry);
        }
    }

    entries
}

fn target_dir_from_fp_root<'a>(fp_root: &'a Path, _profile: &str) -> &'a Path {
    // fp_root = <target>/<profile>/.fingerprint
    // target  = parent of <profile>
    fp_root.parent().and_then(|p| p.parent()).unwrap_or(fp_root)
}

/// Resolve an absolute path under target/ to a target-relative
/// forward-slash string.
fn relativize_to_target(abs: &Path, target_dir: &Path) -> String {
    let abs = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
    let target = target_dir
        .canonicalize()
        .unwrap_or_else(|_| target_dir.to_path_buf());
    match abs.strip_prefix(&target) {
        Ok(rel) => path_to_unix(rel),
        Err(_) => path_to_unix(abs.as_path()),
    }
}

/// Pick the first build-script `.d` file inside
/// `<build_root>/<dir_name>/`. Cargo emits `build_script_<scriptname>-<hash>.d`
/// — we don't care about the exact name, only that it lists the script's
/// source inputs.
fn first_buildscript_d_file(build_root: &Path, dir_name: &str) -> Option<PathBuf> {
    let bs_dir = build_root.join(dir_name);
    let read_dir = std::fs::read_dir(&bs_dir).ok()?;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("d") {
            return Some(path);
        }
    }
    None
}

/// Parse a `.d` Makefile, hash each tracked workspace-local source, and
/// return the (workspace-relative path → blake3 hex) map. Returns `None`
/// if no workspace-local source was found (so the crate is omitted from
/// the manifest — cargo's normal mtime check applies).
fn hash_d_file_sources(d_file: &Path, workspace_root: &Path) -> Option<BTreeMap<String, String>> {
    let content = std::fs::read_to_string(d_file).ok()?;
    let sources = parse_d_file(&content);
    let hashed: BTreeMap<String, String> = sources
        .par_iter()
        .filter_map(|src| {
            let canon = src.canonicalize().ok()?;
            let rel = canon.strip_prefix(workspace_root).ok()?;
            if is_vendored_dep(rel) {
                return None;
            }
            let bytes = std::fs::read(&canon).ok()?;
            let hash = blake3::hash(&bytes).to_hex().to_string();
            Some((path_to_unix(rel), hash))
        })
        .collect();
    if hashed.is_empty() {
        None
    } else {
        Some(hashed)
    }
}

/// Read the `run-build-script-build-script-build.json` file in `fp_dir`
/// and emit an entry covering its `RerunIfChanged` local fingerprints.
fn process_run_build_script(
    fp_dir: &Path,
    dir_name: &str,
    workspace_root: &Path,
    manifest_dirs: &HashMap<String, PathBuf>,
) -> Option<CrateEntry> {
    let json_path = fp_dir.join("run-build-script-build-script-build.json");
    let content = std::fs::read_to_string(&json_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let local = value.get("local")?.as_array()?;

    // Find the package's manifest dir from the fingerprint dir name. The
    // dir name is `<package>-<16-hex-hash>`.
    let pkg_name = strip_hash_suffix(dir_name)?;
    let manifest_dir = manifest_dirs.get(pkg_name)?;

    let mut sources = BTreeMap::new();
    let mut stamp_targets: Vec<String> = Vec::new();
    for item in local {
        let Some(rerun) = item.get("RerunIfChanged") else {
            continue;
        };
        let output = rerun
            .get("output")
            .and_then(|v| v.as_str())
            .map(|s| s.replace('\\', "/"));
        if let Some(out) = output {
            stamp_targets.push(out);
        }
        let Some(paths) = rerun.get("paths").and_then(|p| p.as_array()) else {
            continue;
        };
        for p in paths {
            let Some(rel) = p.as_str() else { continue };
            let abs = manifest_dir.join(rel);
            let Ok(canon) = abs.canonicalize() else {
                continue;
            };
            let Ok(ws_rel) = canon.strip_prefix(workspace_root) else {
                continue;
            };
            if is_vendored_dep(ws_rel) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&canon) else {
                continue;
            };
            let hash = blake3::hash(&bytes).to_hex().to_string();
            sources.insert(path_to_unix(ws_rel), hash);
        }
    }

    if sources.is_empty() || stamp_targets.is_empty() {
        return None;
    }
    Some(CrateEntry {
        fingerprint_dir: dir_name.to_string(),
        stamp_targets,
        sources,
    })
}

/// `<crate>-<hex16>` → `Some("<crate>")`. Returns `None` if the suffix
/// isn't a plausible cargo fingerprint hash.
fn strip_hash_suffix(s: &str) -> Option<&str> {
    let idx = s.rfind('-')?;
    let (name, hash) = s.split_at(idx);
    let hash = &hash[1..];
    if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(name)
    } else {
        None
    }
}

/// Walk `workspace_root` for `Cargo.toml` files and build a
/// `package_name → manifest_dir` map. Skips `target/`, `.cargo/`, `.git/`,
/// `node_modules/`, and dot-directories.
fn discover_workspace_crates(workspace_root: &Path) -> HashMap<String, PathBuf> {
    use jwalk::WalkDirGeneric;
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    let walker: WalkDirGeneric<((), ())> = WalkDirGeneric::new(workspace_root)
        .skip_hidden(false)
        .process_read_dir(|_depth, _parent, _, children| {
            children.retain(|c| match c {
                Ok(e) => {
                    let name = e.file_name();
                    let name_str = name.to_string_lossy();
                    !matches!(
                        name_str.as_ref(),
                        "target" | ".cargo" | ".git" | "node_modules" | ".venv"
                    )
                }
                Err(_) => false,
            });
        });
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if entry.file_name() != "Cargo.toml" {
            continue;
        }
        let path = entry.path();
        let Some(pkg_name) = parse_package_name(&path) else {
            continue;
        };
        let Some(dir) = path.parent() else { continue };
        map.insert(pkg_name, dir.to_path_buf());
    }
    map
}

/// Minimal TOML parser that pulls only `[package].name = "..."`. Avoids
/// adding a `toml` dep for a one-field lookup. Returns `None` if the file
/// has no `[package]` section, no `name` key, or uses workspace inheritance
/// (`name.workspace = true`).
fn parse_package_name(toml_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(toml_path).ok()?;
    let mut in_package = false;
    for raw in content.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            // `name.workspace = true` lands here — workspace inheritance
            // we don't resolve, so skip.
            continue;
        };
        let val = rest.trim();
        let val = val.trim_matches(|c: char| c == '"' || c == '\'');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
}

/// Read the manifest written by `record` and selectively bump
/// `stamp_targets` mtimes for crates whose tracked sources still match.
pub fn validate(
    target_dir: &Path,
    workspace_root: &Path,
    manifest_path: &Path,
    _profile: &str,
    stamp_seconds_ahead: u64,
) -> io::Result<ValidateStats> {
    let manifest_bytes = match std::fs::read(manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ValidateStats::default()),
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
    let target_dir = target_dir
        .canonicalize()
        .unwrap_or_else(|_| target_dir.to_path_buf());
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
                for stamp_rel in &entry.stamp_targets {
                    let stamp_abs = target_dir.join(stamp_rel);
                    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&stamp_abs) {
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
/// Spaces inside paths are backslash-escaped (`\ `).
fn parse_d_file(content: &str) -> Vec<PathBuf> {
    let mut out = std::collections::HashSet::<PathBuf>::new();
    for line in content.lines() {
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

/// Path → forward-slash UTF-8 string. The manifest must be cross-platform.
/// Strips the Windows `\\?\` extended-length prefix that `canonicalize`
/// emits.
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

/// Best-effort timestamp without pulling chrono in as a dep.
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

    /// Synthetic layout for a single lib crate "foo" at `workspace/crates/foo/`.
    fn make_synthetic_lib(root: &Path, source_content: &str) -> (PathBuf, PathBuf) {
        let workspace = root.join("workspace");
        let target = workspace.join("target");
        let fp_dir = target.join("debug/.fingerprint/foo-abcdef0123456789");
        let deps_dir = target.join("debug/deps");
        std::fs::create_dir_all(&fp_dir).unwrap();
        std::fs::create_dir_all(&deps_dir).unwrap();
        let src = workspace.join("crates/foo/src/lib.rs");
        write(&src, source_content);
        write(
            &workspace.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n",
        );
        write(&fp_dir.join("dep-lib-foo"), "anchor");
        let canon_src = src.canonicalize().unwrap();
        let d_content = format!(
            "{}: {}\n",
            deps_dir.join("libfoo-abcdef0123456789.rmeta").display(),
            canon_src.display()
        );
        write(&deps_dir.join("foo-abcdef0123456789.d"), &d_content);
        (workspace, target)
    }

    #[test]
    fn record_and_validate_unchanged_lib_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, target) = make_synthetic_lib(tmp.path(), "fn main() {}\n");
        let manifest = target.join(MANIFEST_FILENAME);
        let stats = record(&target, &workspace, &manifest, "debug").unwrap();
        assert_eq!(stats.crates_recorded, 1);
        assert_eq!(stats.sources_hashed, 1);

        let dep_path = target.join("debug/.fingerprint/foo-abcdef0123456789/dep-lib-foo");
        let before = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        let stats = validate(&target, &workspace, &manifest, "debug", 60).unwrap();
        assert_eq!(stats.crates_clean, 1);
        assert_eq!(stats.crates_dirty(), 0);
        let after = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        assert!(after > before);
    }

    #[test]
    fn validate_dirty_lib_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, target) = make_synthetic_lib(tmp.path(), "fn main() {}\n");
        let manifest = target.join(MANIFEST_FILENAME);
        record(&target, &workspace, &manifest, "debug").unwrap();

        let dep_path = target.join("debug/.fingerprint/foo-abcdef0123456789/dep-lib-foo");
        let before = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(
            workspace.join("crates/foo/src/lib.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();
        let stats = validate(&target, &workspace, &manifest, "debug", 60).unwrap();
        assert_eq!(stats.crates_clean, 0);
        assert_eq!(stats.crates_dirty(), 1);
        let after = std::fs::metadata(&dep_path).unwrap().modified().unwrap();
        assert_eq!(after, before);
    }

    /// Synthetic build-script COMPILATION fingerprint: dep file inside
    /// the fingerprint dir, .d file under `target/<profile>/build/<dir>/`.
    #[test]
    fn record_handles_build_script_compilation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let target = workspace.join("target");
        let fp_dir = target.join("debug/.fingerprint/bar-fedcba9876543210");
        let build_dir = target.join("debug/build/bar-fedcba9876543210");
        std::fs::create_dir_all(&fp_dir).unwrap();
        std::fs::create_dir_all(&build_dir).unwrap();
        let src = workspace.join("crates/bar/build.rs");
        write(
            &src,
            "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        );
        write(
            &workspace.join("crates/bar/Cargo.toml"),
            "[package]\nname = \"bar\"\nversion = \"0.1.0\"\n",
        );
        write(
            &fp_dir.join("dep-build-script-build-script-build"),
            "anchor",
        );
        let canon_src = src.canonicalize().unwrap();
        let d_content = format!(
            "{}: {}\n",
            build_dir
                .join("build_script_build-fedcba9876543210")
                .display(),
            canon_src.display()
        );
        write(
            &build_dir.join("build_script_build-fedcba9876543210.d"),
            &d_content,
        );

        let manifest = target.join(MANIFEST_FILENAME);
        let stats = record(&target, &workspace, &manifest, "debug").unwrap();
        assert_eq!(stats.crates_recorded, 1);
        assert_eq!(stats.sources_hashed, 1);

        let m: Manifest = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(m.crates[0].fingerprint_dir, "bar-fedcba9876543210");
        assert!(m.crates[0].stamp_targets[0].contains(
            "debug/.fingerprint/bar-fedcba9876543210/dep-build-script-build-script-build"
        ));

        let stats = validate(&target, &workspace, &manifest, "debug", 60).unwrap();
        assert_eq!(stats.crates_clean, 1);
    }

    /// Synthetic run-build-script (RerunIfChanged) fingerprint: json
    /// describes paths relative to the crate manifest dir + an output
    /// file under `build/<dir>/`.
    #[test]
    fn record_handles_run_build_script() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let target = workspace.join("target");
        let fp_dir = target.join("debug/.fingerprint/baz-0123456789abcdef");
        let build_dir = target.join("debug/build/baz-0123456789abcdef");
        std::fs::create_dir_all(&fp_dir).unwrap();
        std::fs::create_dir_all(&build_dir).unwrap();
        write(&workspace.join("crates/baz/build.rs"), "fn main() {}\n");
        write(
            &workspace.join("crates/baz/Cargo.toml"),
            "[package]\nname = \"baz\"\nversion = \"0.1.0\"\n",
        );
        let output_file = build_dir.join("output");
        write(&output_file, "build-output");

        let json = serde_json::json!({
            "local": [
                {
                    "RerunIfChanged": {
                        "output": "debug/build/baz-0123456789abcdef/output",
                        "paths": ["build.rs"],
                    }
                }
            ]
        });
        write(
            &fp_dir.join("run-build-script-build-script-build.json"),
            &serde_json::to_string(&json).unwrap(),
        );

        let manifest = target.join(MANIFEST_FILENAME);
        let stats = record(&target, &workspace, &manifest, "debug").unwrap();
        assert_eq!(stats.crates_recorded, 1);
        assert_eq!(stats.sources_hashed, 1);

        let m: Manifest = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert!(m.crates[0].stamp_targets[0].contains("debug/build/baz-0123456789abcdef/output"));

        let before = std::fs::metadata(&output_file).unwrap().modified().unwrap();
        let stats = validate(&target, &workspace, &manifest, "debug", 60).unwrap();
        assert_eq!(stats.crates_clean, 1);
        let after = std::fs::metadata(&output_file).unwrap().modified().unwrap();
        assert!(after > before, "output file mtime should be bumped");
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
        let content = "/out: /a\\ path/foo.rs\n";
        let parsed = parse_d_file(content);
        assert!(parsed.iter().any(|p| p == Path::new("/a path/foo.rs")));
    }

    #[test]
    fn strip_hash_suffix_classifies_correctly() {
        assert_eq!(strip_hash_suffix("foo-0123456789abcdef"), Some("foo"));
        assert_eq!(
            strip_hash_suffix("zccache-cli-0123456789abcdef"),
            Some("zccache-cli")
        );
        assert_eq!(strip_hash_suffix("foo-bar"), None);
        assert_eq!(strip_hash_suffix("noseparator"), None);
        // Too short
        assert_eq!(strip_hash_suffix("foo-abc"), None);
        // Non-hex
        assert_eq!(strip_hash_suffix("foo-zzzzzzzzzzzzzzzz"), None);
    }

    #[test]
    fn parse_package_name_skips_workspace_inherit() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("Cargo.toml");
        write(
            &p,
            "[package]\nname.workspace = true\nversion.workspace = true\n",
        );
        assert_eq!(parse_package_name(&p), None);
    }

    #[test]
    fn parse_package_name_reads_explicit_name() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("Cargo.toml");
        write(&p, "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n");
        assert_eq!(parse_package_name(&p), Some("my-crate".to_string()));
    }
}
