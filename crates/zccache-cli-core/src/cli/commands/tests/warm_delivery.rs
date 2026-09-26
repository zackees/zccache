//! `zccache warm` delivery under each `ZCCACHE_MODE` (#1683).

use super::*;
use crate::core::config::MaterializationMode;

const BYTES: &[u8] = b"cached rlib bytes";

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("cache-blob");
    std::fs::write(&src, BYTES).unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let dst = dir.path().join("libwarm.rlib");
    (dir, src, dst)
}

fn now_times() -> (std::time::SystemTime, std::fs::FileTimes) {
    let now = std::time::SystemTime::now();
    (
        now,
        std::fs::FileTimes::new()
            .set_accessed(now)
            .set_modified(now),
    )
}

fn same_file(a: &Path, b: &Path) -> bool {
    kernal_api::platform::fs::path_file::same_file(a, b).unwrap()
}

fn hardlinks_supported(dir: &Path) -> bool {
    let probe = dir.join("probe");
    std::fs::write(&probe, b"x").unwrap();
    let linked = std::fs::hard_link(&probe, dir.join("probe-link")).is_ok();
    let _ = std::fs::remove_file(dir.join("probe-link"));
    let _ = std::fs::remove_file(probe);
    linked
}

/// COPY and REFLINK restore an independent, writable output and still stamp
/// the cache file as recently used (the eviction LRU signal).
#[test]
fn independent_modes_restore_a_private_output_and_touch_the_cache_file() {
    for mode in [MaterializationMode::Copy, MaterializationMode::Reflink] {
        let (_dir, src, dst) = fixture();
        let (now, times) = now_times();
        deliver_warm_file(&src, &dst, mode, times).unwrap();
        assert!(
            !same_file(&src, &dst),
            "{mode}: output shares the cache inode"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), BYTES);
        assert!(
            !std::fs::metadata(&dst).unwrap().permissions().readonly(),
            "{mode}"
        );
        std::fs::write(&dst, b"edited").unwrap();
        assert_eq!(
            std::fs::read(&src).unwrap(),
            BYTES,
            "{mode}: edit reached the cache"
        );
        let cache_mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
        assert!(
            cache_mtime.duration_since(now).is_ok()
                || now.duration_since(cache_mtime).unwrap().as_secs() < 2,
            "{mode}: the cache file must be stamped recently used"
        );
    }
}

#[test]
fn link_mode_shares_the_cache_inode() {
    let (dir, src, dst) = fixture();
    if !hardlinks_supported(dir.path()) {
        eprintln!("SKIP link_mode_shares_the_cache_inode: no hardlinks on this volume");
        return;
    }
    let (_, times) = now_times();
    deliver_warm_file(&src, &dst, MaterializationMode::Link, times).unwrap();
    assert!(same_file(&src, &dst));
}

/// AUTO never copies where it can share: the output is either a hardlink or
/// a clone, and its bytes are the cache file's.
#[test]
fn auto_mode_restores_the_cached_bytes() {
    let (dir, src, dst) = fixture();
    let (_, times) = now_times();
    deliver_warm_file(&src, &dst, MaterializationMode::Auto, times).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), BYTES);
    if hardlinks_supported(dir.path())
        && kernal_api::platform::fs::reflink_file(&src, &dir.path().join("clone-probe")).is_err()
    {
        assert!(
            same_file(&src, &dst),
            "AUTO must hardlink where it cannot clone"
        );
    }
}
