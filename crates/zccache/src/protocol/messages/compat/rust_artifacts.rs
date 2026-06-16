//! `ListRustArtifacts` / `RustArtifactList` / `RustArtifactInfo`
//! roundtrip tests.

use super::*;

#[test]
fn list_rust_artifacts_request_roundtrip() {
    roundtrip(&Request::ListRustArtifacts);
}

#[test]
fn rust_artifact_list_response_roundtrip() {
    roundtrip(&Response::RustArtifactList {
        artifacts: vec![
            RustArtifactInfo {
                cache_key: "abc123def456".into(),
                output_names: vec![
                    "libfoo-abc123.rlib".into(),
                    "libfoo-abc123.rmeta".into(),
                    "foo-abc123.d".into(),
                ],
                payload_count: 3,
            },
            RustArtifactInfo {
                cache_key: "deadbeef".into(),
                output_names: vec!["libbar-deadbeef.rlib".into()],
                payload_count: 1,
            },
        ],
    });
    // Empty list
    roundtrip(&Response::RustArtifactList { artifacts: vec![] });
}

#[test]
fn rust_artifact_info_roundtrip() {
    roundtrip(&RustArtifactInfo {
        cache_key: "0123456789abcdef".into(),
        output_names: vec!["test.o".into()],
        payload_count: 1,
    });
}
