use super::*;
use filetime::{set_file_mtime, FileTime};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn create_file(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
}

fn mtime_of(path: &Path) -> FileTime {
    let meta = fs::metadata(path).unwrap();
    FileTime::from_last_modification_time(&meta)
}

fn rels(manifest: &MtimeManifest) -> Vec<&str> {
    manifest.entries.iter().map(|e| e.path.as_str()).collect()
}

fn entry_for<'a>(manifest: &'a MtimeManifest, path: &str) -> &'a MtimeEntry {
    manifest
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("manifest missing entry for {path}"))
}

const OLD_MTIME: FileTime = FileTime::from_unix_time(1_600_000_000, 0);
const FRESH_MTIME: FileTime = FileTime::from_unix_time(1_700_000_000, 0);

#[test]
fn unchanged_file_gets_recorded_mtime() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "unchanged content");
    set_file_mtime(&path, OLD_MTIME).unwrap();

    let manifest = snapshot(dir.path(), &[]).unwrap();

    // Simulate a checkout stamping the file with a fresh mtime.
    set_file_mtime(&path, FRESH_MTIME).unwrap();

    let report = replay(dir.path(), &manifest);
    assert_eq!(report.applied, 1);
    assert_eq!(report.total, 1);
    assert_eq!(mtime_of(&path), OLD_MTIME);
}

#[test]
fn same_size_content_change_is_modified_and_mtime_untouched() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "aaaaaaaa");
    set_file_mtime(&path, OLD_MTIME).unwrap();

    let manifest = snapshot(dir.path(), &[]).unwrap();
    let entry = entry_for(&manifest, "a.txt").clone();

    // Same length, different bytes.
    fs::write(&path, "bbbbbbbb").unwrap();
    set_file_mtime(&path, FRESH_MTIME).unwrap();

    let outcome = replay_one(dir.path(), &entry);
    assert_eq!(outcome, ReplayOutcome::Modified);
    assert_eq!(mtime_of(&path), FRESH_MTIME);
}

#[test]
fn size_change_is_size_mismatch() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "short");
    set_file_mtime(&path, OLD_MTIME).unwrap();

    let manifest = snapshot(dir.path(), &[]).unwrap();
    let entry = entry_for(&manifest, "a.txt").clone();

    fs::write(&path, "this content is much longer now").unwrap();
    set_file_mtime(&path, FRESH_MTIME).unwrap();

    let outcome = replay_one(dir.path(), &entry);
    assert_eq!(outcome, ReplayOutcome::SizeMismatch);
    assert_eq!(mtime_of(&path), FRESH_MTIME);
}

#[test]
fn missing_file_is_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "content");

    let manifest = snapshot(dir.path(), &[]).unwrap();
    let entry = entry_for(&manifest, "a.txt").clone();

    fs::remove_file(&path).unwrap();

    assert_eq!(replay_one(dir.path(), &entry), ReplayOutcome::Missing);
}

#[test]
fn directory_in_place_of_file_is_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "content");

    let manifest = snapshot(dir.path(), &[]).unwrap();
    let entry = entry_for(&manifest, "a.txt").clone();

    fs::remove_file(&path).unwrap();
    fs::create_dir_all(&path).unwrap();

    assert_eq!(replay_one(dir.path(), &entry), ReplayOutcome::Missing);
}

#[test]
fn excluded_dir_by_resolved_path() {
    let dir = TempDir::new().unwrap();
    create_file(dir.path(), "build/out.o", "obj");
    create_file(dir.path(), "src/build/keep.c", "kept");
    create_file(dir.path(), "target/x", "target file");
    create_file(dir.path(), ".git/HEAD", "ref: refs/heads/main");
    create_file(dir.path(), "node_modules/m/index.js", "module");
    create_file(dir.path(), "sub/node_modules/y.js", "nested module");

    let manifest = snapshot(dir.path(), &[Path::new("build")]).unwrap();
    let paths = rels(&manifest);

    assert!(paths.contains(&"src/build/keep.c"), "{paths:?}");
    assert!(paths.contains(&"target/x"), "{paths:?}");
    assert!(!paths.contains(&"build/out.o"), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains(".git")), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.contains("node_modules")),
        "{paths:?}"
    );

    // An absolute exclude path resolves to the same directory and must
    // produce an identical result.
    let absolute_build = dir.path().join("build");
    let manifest_abs = snapshot(dir.path(), &[absolute_build.as_path()]).unwrap();
    assert_eq!(rels(&manifest_abs), paths);
}

#[test]
fn changed_file_is_never_stamped_old() {
    let dir = TempDir::new().unwrap();
    create_file(dir.path(), "a.txt", "aaaaaaaa"); // content will change, same size
    create_file(dir.path(), "b.txt", "short"); // size will change
    create_file(dir.path(), "c.txt", "unchanged"); // stays untouched

    for name in ["a.txt", "b.txt", "c.txt"] {
        set_file_mtime(dir.path().join(name), OLD_MTIME).unwrap();
    }

    let manifest = snapshot(dir.path(), &[]).unwrap();

    // Modify content (same size) and size, respectively.
    fs::write(dir.path().join("a.txt"), "bbbbbbbb").unwrap();
    fs::write(dir.path().join("b.txt"), "this got longer").unwrap();

    // Simulate a checkout stamping every file with a fresh mtime.
    for name in ["a.txt", "b.txt", "c.txt"] {
        set_file_mtime(dir.path().join(name), FRESH_MTIME).unwrap();
    }

    let report = replay(dir.path(), &manifest);
    assert_eq!(report.total, 3);
    assert_eq!(report.applied, 1);
    assert_eq!(report.modified, 1);
    assert_eq!(report.size_mismatch, 1);

    let a_entry = entry_for(&manifest, "a.txt");
    let b_entry = entry_for(&manifest, "b.txt");
    let c_entry = entry_for(&manifest, "c.txt");

    // Changed files must never receive their old, pre-edit mtime: their
    // current mtime stays the fresh (post-checkout) one, strictly newer
    // than what was recorded.
    let a_mtime_ns = mtime_of(&dir.path().join("a.txt")).unix_seconds() * 1_000_000_000;
    let b_mtime_ns = mtime_of(&dir.path().join("b.txt")).unix_seconds() * 1_000_000_000;
    assert!(a_mtime_ns > a_entry.mtime_ns);
    assert!(b_mtime_ns > b_entry.mtime_ns);
    assert_eq!(mtime_of(&dir.path().join("a.txt")), FRESH_MTIME);
    assert_eq!(mtime_of(&dir.path().join("b.txt")), FRESH_MTIME);

    // The unchanged file gets its recorded (old) mtime back.
    assert_eq!(mtime_of(&dir.path().join("c.txt")), OLD_MTIME);
    assert_eq!(c_entry.mtime_ns, OLD_MTIME.unix_seconds() * 1_000_000_000);
}

#[test]
fn set_times_failure_reports_modified() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "content");
    set_file_mtime(&path, OLD_MTIME).unwrap();

    let manifest = snapshot(dir.path(), &[]).unwrap();
    let entry = entry_for(&manifest, "a.txt").clone();

    set_file_mtime(&path, FRESH_MTIME).unwrap();

    let outcome = replay_one_with(dir.path(), &entry, |_path, _atime, _mtime| {
        Err(std::io::Error::other("simulated set_file_times failure"))
    });
    assert_eq!(outcome, ReplayOutcome::Modified);
    assert_eq!(mtime_of(&path), FRESH_MTIME);
}

#[test]
fn rejects_escaping_manifest_paths() {
    let dir = TempDir::new().unwrap();
    create_file(dir.path(), "a.txt", "content");

    let base = MtimeEntry {
        path: String::new(),
        size: 7,
        mtime_ns: OLD_MTIME.unix_seconds() * 1_000_000_000,
        blake3: "0".repeat(64),
    };

    for bad_path in ["../x", "/abs", "a\\b", ""] {
        let entry = MtimeEntry {
            path: bad_path.to_string(),
            ..base.clone()
        };
        assert_eq!(
            replay_one(dir.path(), &entry),
            ReplayOutcome::Missing,
            "path {bad_path:?} should be rejected"
        );
    }

    // Nothing outside the workspace was ever touched.
    assert!(!dir.path().parent().unwrap().join("x").exists());
}

#[test]
fn manifest_roundtrip() {
    let dir = TempDir::new().unwrap();
    let manifest_path = dir.path().join("mtime.json");

    let manifest = MtimeManifest {
        version: MANIFEST_VERSION,
        entries: vec![MtimeEntry {
            path: "src/main.rs".to_string(),
            size: 123,
            mtime_ns: 1_700_000_000_000_000_000,
            blake3: "a".repeat(64),
        }],
    };

    write_manifest(&manifest_path, &manifest).unwrap();
    let loaded = read_manifest(&manifest_path).unwrap();
    assert_eq!(loaded, manifest);

    let bad_path = dir.path().join("bad.json");
    let mut bad_manifest = manifest;
    bad_manifest.version = MANIFEST_VERSION + 1;
    write_manifest(&bad_path, &bad_manifest).unwrap();
    let err = read_manifest(&bad_path).unwrap_err();
    assert!(
        matches!(err, super::super::error::FingerprintError::Manifest { .. }),
        "expected Manifest error, got: {err}"
    );
}

#[test]
fn applied_ratio() {
    assert_eq!(ReplayReport::default().applied_ratio(), 0.0);

    let mut report = ReplayReport::default();
    for outcome in [
        ReplayOutcome::Applied,
        ReplayOutcome::Applied,
        ReplayOutcome::Applied,
        ReplayOutcome::Missing,
    ] {
        report.record(outcome);
    }
    assert_eq!(report.applied_ratio(), 0.75);
}

#[test]
fn mtime_ns_preserves_subsecond_and_pre_epoch() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.txt");
    create_file(dir.path(), "a.txt", "content");
    let original = FileTime::from_unix_time(1_600_000_123, 456_000_000);
    set_file_mtime(&path, original).unwrap();

    let manifest = snapshot(dir.path(), &[]).unwrap();
    let entry = entry_for(&manifest, "a.txt").clone();

    // Simulate a checkout stamping the file with a fresh mtime.
    set_file_mtime(&path, FRESH_MTIME).unwrap();

    assert_eq!(replay_one(dir.path(), &entry), ReplayOutcome::Applied);

    let restored = mtime_of(&path);
    assert_eq!(restored.unix_seconds(), original.unix_seconds());
    assert!(
        restored.nanoseconds() == original.nanoseconds() || restored.nanoseconds() == 0,
        "expected exact nanosecond preservation or filesystem truncation to whole \
         seconds, got {}",
        restored.nanoseconds()
    );

    // Pre-epoch times: the signed-ns encoding must round-trip through the
    // same div_euclid/rem_euclid split `replay_one_with` uses.
    let pre_epoch = FileTime::from_unix_time(-2, 500_000_000);
    let ns = filetime_to_ns(pre_epoch);
    assert_eq!(ns, -1_500_000_000);
    let back = FileTime::from_unix_time(
        ns.div_euclid(1_000_000_000),
        ns.rem_euclid(1_000_000_000) as u32,
    );
    assert_eq!(back, pre_epoch);
}
