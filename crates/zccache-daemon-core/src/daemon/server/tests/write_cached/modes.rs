use super::*;

/// #1791: a hit upgrades blobs sealed by older versions without dropping
/// shared-inode delivery or the cache's protection against in-place writes.
#[test]
fn legacy_readonly_blob_upgrades_when_delivered_as_a_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("legacy.rlib");
    let output = dir.path().join("libapp.rlib");
    seed_persisted_blob(&blob, b"legacy bytes");
    crate::platform::fs::permissions::set_readonly(&blob, true).unwrap();
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());

    deliver_linked(&output, &blob);
    if !require_hardlink(&output, &blob, "legacy_readonly_blob_upgrades") {
        return;
    }
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());
    assert!(!std::fs::metadata(&output).unwrap().permissions().readonly());
    assert_eq!(std::fs::read(&blob).unwrap(), b"legacy bytes");

    let replacement = dir.path().join("replacement.rlib");
    std::fs::write(&replacement, b"new output").unwrap();
    std::fs::rename(&replacement, &output).unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), b"new output");
    assert_eq!(std::fs::read(&blob).unwrap(), b"legacy bytes");
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());
    crate::platform::fs::permissions::make_writable(&blob).unwrap();
}

/// #1792: the standalone default keeps outputs independent so a later
/// wrapper-less rustc can replace them (#1791).
#[test]
fn auto_never_shares_the_cache_inode() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("libunit.rlib");
    let cache = dir.path().join("artifact-key_0");
    std::fs::write(&source, b"auto store").unwrap();
    let stored = persist_artifact_file(&cache, &source, MaterializationMode::Auto).unwrap();
    assert_eq!(stored.hardlink_count, 0, "AUTO stored by hardlink");
    assert!(!crate::platform::fs::identity::same_file(&source, &cache).unwrap());

    let out = dir.path().join("libapp.rlib");
    let delivered = materialize_cached_file_with_mode(
        &out,
        &cache,
        crate::compiler::DeliveryPolicy::HardlinkEligible,
        MaterializationMode::Auto,
        true,
    )
    .unwrap();
    assert_eq!(delivered.hardlink_count, 0, "AUTO delivered a hardlink");
    assert!(!crate::platform::fs::identity::same_file(&out, &cache).unwrap());
    assert!(
        !std::fs::metadata(&out).unwrap().permissions().readonly(),
        "an AUTO hit must be writable so rustc accepts it"
    );
}

/// #1791: a sealed blob refuses its owner's in-place writes (#1039) but is
/// not "read-only" to `Permissions::readonly()`, which is the check rustc
/// uses to refuse replacing an output. Unsealing restores the plain mode.
#[test]
fn sealed_blob_passes_rustcs_readonly_check_and_unseals_to_the_plain_mode() {
    use crate::platform::fs::permissions::{
        apply_mode, is_sealed, make_writable, mode, seal_cache_blob,
    };
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    std::fs::write(&blob, b"sealed").unwrap();
    apply_mode(&blob, 0o644).unwrap();

    seal_cache_blob(&blob).unwrap();
    let metadata = std::fs::metadata(&blob).unwrap();
    if !kernal_api::platform::host::target_is_windows() {
        assert_eq!(mode(&metadata) & 0o777, 0o464);
    }
    assert!(
        is_sealed(&blob).unwrap(),
        "the owner must not be able to write"
    );
    assert!(
        !metadata.permissions().readonly(),
        "rustc's check_file_is_writeable must see a writable output"
    );

    make_writable(&blob).unwrap();
    let metadata = std::fs::metadata(&blob).unwrap();
    if !kernal_api::platform::host::target_is_windows() {
        assert_eq!(
            mode(&metadata) & 0o777,
            0o644,
            "unsealing must not leave group write"
        );
    }
    assert!(!is_sealed(&blob).unwrap());
}
