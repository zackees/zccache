//! `snapshot-bytes` parallel walk tests (issue #189): empty/missing tree,
//! `--prune-incremental`, and the `*/build/*/out/` prune toggle.

use std::path::Path;

use super::super::cache_ops::snapshot_bytes_walk;

// ─── snapshot-bytes parallel walk (issue #189) ──────────────────────

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(path, bytes).expect("write file");
}

/// Empty / missing target dir returns 0 bytes (mirrors os.walk behavior).
#[test]
fn snapshot_bytes_missing_target_is_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("nope");
    assert_eq!(snapshot_bytes_walk(&missing, true, false).unwrap(), 0);
}

/// Sums regular files. `--prune-incremental` removes `incremental/`
/// directories from the walk entirely.
#[test]
fn snapshot_bytes_prunes_incremental() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path();
    write_file(&target.join("debug/deps/libfoo.rlib"), &[0u8; 100]);
    write_file(&target.join("debug/incremental/foo/state.bin"), &[0u8; 999]);
    write_file(
        &target.join("release/incremental/bar/state.bin"),
        &[0u8; 999],
    );

    let with_prune = snapshot_bytes_walk(target, true, false).unwrap();
    assert_eq!(with_prune, 100, "incremental should be excluded");

    let without_prune = snapshot_bytes_walk(target, false, false).unwrap();
    assert_eq!(
        without_prune,
        100 + 999 + 999,
        "without prune, all files counted"
    );
}

/// `--prune-build-script-out` removes `*/build/*/out/` only. A bare `out/`
/// outside that pattern stays in the count.
#[test]
fn snapshot_bytes_prunes_build_script_out_only_under_build() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path();
    write_file(
        &target.join("debug/build/libz-sys-abc/out/native/libz.a"),
        &[0u8; 500],
    );
    write_file(
        &target.join("debug/build/libz-sys-abc/build-script-build"),
        &[0u8; 50],
    );
    // `out/` that is NOT under `build/<pkg>/` should not be pruned.
    write_file(&target.join("debug/deps/some/out/data.bin"), &[0u8; 7]);

    let pruned = snapshot_bytes_walk(target, true, true).unwrap();
    assert_eq!(
        pruned,
        50 + 7,
        "only build/<pkg>/out should be pruned; deps/some/out kept"
    );

    let kept = snapshot_bytes_walk(target, true, false).unwrap();
    assert_eq!(kept, 500 + 50 + 7);
}

/// Walker tolerates an entirely empty tree — returns 0, doesn't error.
#[test]
fn snapshot_bytes_empty_target_is_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_eq!(snapshot_bytes_walk(tmp.path(), true, false).unwrap(), 0);
}
