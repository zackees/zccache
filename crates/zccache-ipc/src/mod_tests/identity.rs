use super::{fake_identity, EnvGuard};
use crate::{backend_identity_path, daemon_identity_matches, read_backend_identity};

#[test]
fn pre_4_10_4_identity_defaults_missing_legacy_digest() {
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_cache_dir(temp.path());
    let expected = fake_identity(4321, 1_700_000_000_000, "boot-a");
    let path = backend_identity_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Start from the current sidecar form and strip the field that 4.10.3
    // binaries never wrote.
    expected.write_sidecar(path.as_path()).unwrap();
    let mut legacy_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    legacy_json
        .as_object_mut()
        .unwrap()
        .remove("legacy_exe_sha256")
        .expect("current sidecar carries the legacy digest field");
    std::fs::write(&path, serde_json::to_vec_pretty(&legacy_json).unwrap()).unwrap();

    let decoded = read_backend_identity().expect("4.10.3 identity must remain readable");
    assert_eq!(decoded.legacy_sha256_digest(), &[0; 32]);
    assert!(daemon_identity_matches(&expected));
}

#[test]
fn sidecar_keeps_the_historical_json_fields() {
    // Old and new zccache binaries read each other's identity sidecar, so the
    // facade writer must keep the historical `DaemonProcess` JSON fields.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("identity.json");
    fake_identity(4321, 1_700_000_000_000, "boot-a")
        .write_sidecar(&path)
        .unwrap();
    let written = std::fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&written).unwrap();
    for field in [
        "pid",
        "exe_path",
        "exe_hash",
        "legacy_exe_sha256",
        "boot_id",
        "ipc_endpoint",
        "started_at_unix_ms",
    ] {
        assert!(value.get(field).is_some(), "sidecar lost field {field}");
    }
}
