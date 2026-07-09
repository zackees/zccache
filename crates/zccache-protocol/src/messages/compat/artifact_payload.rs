//! `ArtifactPayload` size + clone Arc-sharing + raw bincode tests,
//! plus a sanity check that `Arc<Vec<u8>>` serializes identically to
//! `Vec<u8>` over bincode.

use super::*;

#[test]
fn artifact_clone_shares_payload_via_arc() {
    let bytes = Arc::new(vec![1u8, 2, 3, 4]);
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "test.o".into(),
            payload: ArtifactPayload::Bytes(Arc::clone(&bytes)),
        }],
        stdout: Arc::new(vec![5, 6]),
        stderr: Arc::new(vec![7, 8]),
        exit_code: 0,
    };

    let cloned = artifact.clone();

    // Arc::clone bumps refcount — both point to the same allocation.
    let orig_inner = artifact.outputs[0].payload.as_bytes().unwrap();
    let cloned_inner = cloned.outputs[0].payload.as_bytes().unwrap();
    assert!(Arc::ptr_eq(orig_inner, cloned_inner));
    assert!(Arc::ptr_eq(orig_inner, &bytes));
    assert!(Arc::ptr_eq(&artifact.stdout, &cloned.stdout));
    assert!(Arc::ptr_eq(&artifact.stderr, &cloned.stderr));
}

#[test]
fn artifact_payload_size_bytes_for_bytes_variant() {
    let p = ArtifactPayload::Bytes(Arc::new(vec![0u8; 1234]));
    assert_eq!(p.size_bytes(), 1234);
}

#[test]
fn artifact_payload_size_bytes_for_path_variant() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(tmp.path(), vec![0u8; 4321]).expect("write");
    let p = ArtifactPayload::Path(NormalizedPath::from(tmp.path()));
    assert_eq!(p.size_bytes(), 4321);
}

#[test]
fn artifact_payload_size_bytes_for_missing_path_is_zero() {
    let p = ArtifactPayload::Path(NormalizedPath::from(std::path::Path::new(
        "/this/path/does/not/exist/zccache",
    )));
    assert_eq!(p.size_bytes(), 0);
}

#[test]
fn artifact_payload_round_trips_through_bincode() {
    let bytes_variant = ArtifactPayload::Bytes(Arc::new(b"hello".to_vec()));
    let encoded = bincode::serialize(&bytes_variant).expect("serialize bytes");
    let decoded: ArtifactPayload = bincode::deserialize(&encoded).expect("deserialize bytes");
    assert_eq!(decoded, bytes_variant);

    let path_variant = ArtifactPayload::Path(NormalizedPath::from(std::path::Path::new(
        "/tmp/some/place.rlib",
    )));
    let encoded = bincode::serialize(&path_variant).expect("serialize path");
    let decoded: ArtifactPayload = bincode::deserialize(&encoded).expect("deserialize path");
    assert_eq!(decoded, path_variant);
}

#[test]
fn arc_vec_u8_roundtrip_matches_plain_vec() {
    // Prove Arc<Vec<u8>> serializes identically to Vec<u8>.
    let plain: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let arc_wrapped: Arc<Vec<u8>> = Arc::new(plain.clone());

    let plain_bytes = bincode::serialize(&plain).unwrap();
    let arc_bytes = bincode::serialize(&arc_wrapped).unwrap();
    assert_eq!(
        plain_bytes, arc_bytes,
        "Arc<Vec<u8>> must serialize identically to Vec<u8>"
    );

    // Deserialize Arc bytes back as plain Vec and vice versa.
    let decoded_plain: Vec<u8> = bincode::deserialize(&arc_bytes).unwrap();
    let decoded_arc: Arc<Vec<u8>> = bincode::deserialize(&plain_bytes).unwrap();
    assert_eq!(decoded_plain, plain);
    assert_eq!(*decoded_arc, plain);
}
