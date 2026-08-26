use super::super::executable_hash_blake3;
use super::{fake_identity, EnvGuard};
use crate::{backend_identity_path, daemon_identity_matches, read_backend_identity};

#[test]
fn identity_blake3_mmaps_large_files_without_changing_the_digest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large-daemon-image");
    let bytes: Vec<u8> = (0..(256 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect();
    std::fs::write(&path, &bytes).unwrap();

    let actual_blake3 = executable_hash_blake3(&path).unwrap();
    let expected_blake3 = *blake3::hash(&bytes).as_bytes();

    assert_eq!(actual_blake3, expected_blake3);
}

#[test]
fn pre_4_10_4_identity_defaults_missing_legacy_digest() {
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(temp.path());
    let expected = fake_identity(4321, 1_700_000_000_000, "boot-a");
    let mut legacy_json = serde_json::to_value(&expected).unwrap();
    legacy_json
        .as_object_mut()
        .unwrap()
        .remove("legacy_exe_sha256");
    let path = backend_identity_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&legacy_json).unwrap()).unwrap();

    let decoded = read_backend_identity().expect("4.10.3 identity must remain readable");
    assert_eq!(decoded.legacy_exe_sha256, [0; 32]);
    assert!(daemon_identity_matches(&expected));
}
