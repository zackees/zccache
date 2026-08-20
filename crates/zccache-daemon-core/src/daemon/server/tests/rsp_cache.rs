//! Daemon-side response-file expansion regression tests.

use super::super::*;

#[tokio::test]
async fn cached_expansion_parses_utf16le_response_file_for_cache_keys() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = dir.path().join("cache").into();
    let response = dir.path().join("compile.rsp");
    let mut utf16le = vec![0xff, 0xfe];
    for word in "/c \"\u{6e90}.cpp\" /Foobj\\\r\n".encode_utf16() {
        utf16le.extend_from_slice(&word.to_le_bytes());
    }
    std::fs::write(&response, utf16le).unwrap();

    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let args = vec![format!("@{}", response.display())];
    assert_eq!(
        expand_args_cached(server.test_state(), &args, dir.path()),
        vec!["/c", "\u{6e90}.cpp", "/Foobj\\"],
        "UTF-16 response-file flags must be visible to daemon key parsing"
    );
}

#[tokio::test]
async fn cached_expansion_reloads_a_changed_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = dir.path().join("cache").into();
    let response = dir.path().join("compile.rsp");
    std::fs::write(&response, "-DVALUE=one").unwrap();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let args = vec![format!("@{}", response.display())];

    assert_eq!(
        expand_args_cached(server.test_state(), &args, dir.path()),
        vec!["-DVALUE=one"]
    );
    std::fs::write(&response, "-DVALUE=two").unwrap();
    server
        .test_state()
        .cache_system
        .apply_changes(vec![response.clone().into()]);

    assert_eq!(
        expand_args_cached(server.test_state(), &args, dir.path()),
        vec!["-DVALUE=two"]
    );
}

#[tokio::test]
async fn cached_expansion_reloads_a_changed_nested_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = dir.path().join("cache").into();
    let outer = dir.path().join("outer.rsp");
    let nested = dir.path().join("nested.rsp");
    std::fs::write(&outer, "@nested.rsp").unwrap();
    std::fs::write(&nested, "-DVALUE=one").unwrap();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let args = vec![format!("@{}", outer.display())];

    assert_eq!(
        expand_args_cached(server.test_state(), &args, dir.path()),
        vec!["-DVALUE=one"]
    );
    std::fs::write(&nested, "-DVALUE=two").unwrap();
    server
        .test_state()
        .cache_system
        .apply_changes(vec![nested.clone().into()]);

    assert_eq!(
        expand_args_cached(server.test_state(), &args, dir.path()),
        vec!["-DVALUE=two"]
    );
}
