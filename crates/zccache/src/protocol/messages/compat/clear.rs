//! `Request::Clear` + `Response::Cleared` roundtrip tests.

use super::*;

#[test]
fn clear_request_roundtrip() {
    roundtrip(&Request::Clear);
}

#[test]
fn cleared_response_roundtrip() {
    roundtrip(&Response::Cleared {
        artifacts_removed: 42,
        metadata_cleared: 100,
        dep_graph_contexts_cleared: 25,
        on_disk_bytes_freed: 1024 * 1024,
    });
}
