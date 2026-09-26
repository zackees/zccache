//! Rust-plan bundle I/O under `ZCCACHE_MODE` (#1683): bundles are never
//! hardlinked; every mode but COPY may clone.

use super::super::local::{bundle_clone_allowed_for, copy_bundle_file};
use zccache_core::config::MaterializationMode;

#[test]
fn only_copy_mode_forbids_cloning_bundles() {
    for mode in MaterializationMode::ALL {
        assert_eq!(
            bundle_clone_allowed_for(mode),
            mode != MaterializationMode::Copy,
            "{mode}"
        );
    }
}

/// With cloning off the bundle file owns its blocks even on a
/// reflink-capable volume; with it on, a capable volume shares them.
#[test]
fn copy_bundle_file_clones_only_when_asked() {
    use kernal_api::platform::fs::{extent_sharing, ExtentSharing};
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.rlib");
    std::fs::write(&src, vec![3_u8; 128 * 1024]).unwrap();

    let copied = dir.path().join("copied.rlib");
    copy_bundle_file(&src, &copied, false).unwrap();
    assert_eq!(
        std::fs::read(&copied).unwrap(),
        std::fs::read(&src).unwrap()
    );
    assert!(!kernal_api::platform::fs::path_file::same_file(&src, &copied).unwrap());
    assert!(
        !matches!(extent_sharing(&copied), Ok(ExtentSharing::Shared)),
        "COPY must not share blocks with the source"
    );

    let cloned = dir.path().join("cloned.rlib");
    copy_bundle_file(&src, &cloned, true).unwrap();
    assert_eq!(
        std::fs::read(&cloned).unwrap(),
        std::fs::read(&src).unwrap()
    );
    assert!(!kernal_api::platform::fs::path_file::same_file(&src, &cloned).unwrap());
}
