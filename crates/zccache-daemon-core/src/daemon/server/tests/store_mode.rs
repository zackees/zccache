//! Layer E of the #1683 test design: the *store* direction (compiler output
//! -> cache blob) under `ZCCACHE_MODE`. COPY and REFLINK promise that no
//! build-tree output shares the cache blob's inode, so they never hardlink
//! the compiler output in; LINK and REFLINK_OR_LINK_OR_COPY keep the hardlink
//! tier. Raw AUTO (rustc outputs) never hardlinks; the daemon maps AUTO to the
//! full chain for every other compiler (#1792).

use super::super::*;
use MaterializationMode::{Auto, Copy, Link, Reflink};

const BYTES: &[u8] = b"freshly compiled rust archive";

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("target-libunit.rlib");
    std::fs::write(&source, BYTES).unwrap();
    let cache = dir.path().join("artifacts").join("key_0");
    (dir, source, cache)
}

fn same_file(a: &Path, b: &Path) -> bool {
    crate::platform::fs::identity::same_file(a, b).unwrap()
}

#[test]
fn independent_modes_never_hardlink_the_compiler_output_into_the_cache() {
    // #1792: AUTO is independent too; only LINK and REFLINK_OR_LINK_OR_COPY
    // may store by hardlink.
    for mode in [Auto, Copy, Reflink] {
        let (_dir, source, cache) = fixture();
        let stats = persist_artifact_file(&cache, &source, mode).unwrap();
        assert_eq!(stats.hardlink_count, 0, "{mode}");
        assert_eq!(stats.reflink_count + stats.copy_count, 1, "{mode}");
        assert!(
            !same_file(&source, &cache),
            "{mode}: build output shares the blob"
        );
        assert_eq!(std::fs::read(&cache).unwrap(), BYTES);
        // The build output stays the compiler's own file: an in-place edit
        // (a later non-wrapped rustc) never reaches the cache blob.
        std::fs::write(&source, b"rewritten in place").unwrap();
        assert_eq!(std::fs::read(&cache).unwrap(), BYTES, "{mode}");
    }
}

#[test]
fn copy_mode_stores_a_byte_copy() {
    let (_dir, source, cache) = fixture();
    let stats = persist_artifact_file(&cache, &source, Copy).unwrap();
    assert_eq!(
        (stats.reflink_count, stats.hardlink_count, stats.copy_count),
        (0, 0, 1)
    );
    assert_eq!(stats.copy_bytes, BYTES.len() as u64);
}

/// LINK keeps the store's hardlink fast path and never clones.
#[test]
fn link_mode_hardlinks_the_store_where_supported() {
    let (dir, source, cache) = fixture();
    if !fs_caps_raw(&source, &dir.path().join("probe-target")).hardlink {
        eprintln!("SKIP link_mode_hardlinks_the_store_where_supported: no hardlinks here");
        return;
    }
    let stats = persist_artifact_file(&cache, &source, Link).unwrap();
    assert_eq!(stats.reflink_count, 0);
    assert_eq!(stats.hardlink_count, 1);
    assert!(same_file(&source, &cache));
}

/// AUTO (rustc outputs) is reflink -> copy (#1792): exactly one tier.
#[test]
fn auto_mode_store_reports_exactly_one_tier() {
    let (_dir, source, cache) = fixture();
    let stats = persist_artifact_file(&cache, &source, Auto).unwrap();
    assert_eq!(
        stats.reflink_count + stats.hardlink_count + stats.copy_count,
        1
    );
    assert_eq!(std::fs::read(&cache).unwrap(), BYTES);
}

/// The store plan is the shared core decision for policy-free deliveries.
#[test]
fn store_plan_matches_the_shareable_core_tiers() {
    for mode in MaterializationMode::ALL {
        let core = mode.tiers_for_shareable();
        let plan = plan_store_tiers(mode);
        assert_eq!(
            (plan.reflink, plan.hardlink),
            (core.reflink, core.hardlink),
            "{mode}"
        );
    }
}

/// #1792: a C/C++ (non-rustc) miss under AUTO still stores by hardlink where
/// the volume cannot clone, exactly as before; only rustc outputs lost it.
#[test]
fn auto_store_for_non_rustc_outputs_may_hardlink_like_the_full_chain() {
    let (_dir, source, cache) = fixture();
    let mode = for_non_rustc_outputs(Auto);
    let stats = persist_artifact_file(&cache, &source, mode).unwrap();
    if stats.reflink_count == 0 && fs_caps_raw(&source, &cache).hardlink {
        assert_eq!(
            stats.hardlink_count, 1,
            "non-rustc AUTO store must keep the hardlink tier"
        );
    }
    assert_eq!(std::fs::read(&cache).unwrap(), BYTES);
}
