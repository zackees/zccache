//! Tests for `handle_clear` — Request::Clear's preservation invariants.
//!
//! Issue #558: `system_includes` (and the sibling `compiler_hash_cache`)
//! store compiler-environment data keyed by `(compiler_path, mtime, size)`.
//! They are self-correcting via stat-verify on every access, so wiping
//! them across Clear pays the ~44 ms re-probe / ~50–60 ms re-hash penalty
//! on the next compile while contributing nothing to the user's intent
//! of clearing built artifacts.

use super::super::*;

/// Unit-level invariant: an entry stored in `SystemIncludeCache` survives
/// being "skipped by Clear" cleanly. The follow-up `get` stat-verifies the
/// compiler binary; an unchanged binary returns the cached entry, and any
/// post-Clear change to the compiler (the only correctness concern with
/// preservation) is detected on the next access and rejected.
///
/// This is the safety net that makes [`handle_clear`] safe to skip
/// `system_includes.clear()` (and the symmetric `compiler_hash_cache`
/// already gets this treatment — see issue #517).
#[test]
fn system_include_cache_entry_self_verifies_after_clear_skip() {
    use crate::depgraph::SystemIncludeCache;

    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("clang");
    std::fs::write(&compiler, b"compiler binary").unwrap();
    let include_dir = tmp.path().join("usr-include");
    std::fs::create_dir_all(&include_dir).unwrap();

    let mut cache = SystemIncludeCache::new();
    cache.insert(
        crate::core::NormalizedPath::new(&compiler),
        vec![crate::core::NormalizedPath::new(&include_dir)],
    );
    assert_eq!(cache.len(), 1);

    // "Clear ran but system_includes was NOT wiped" — the entry must
    // still stat-verify against the unchanged compiler.
    let hit = cache.get(&compiler);
    assert!(
        hit.is_some(),
        "preserved entry must still stat-verify against an unchanged compiler"
    );
    assert_eq!(hit.unwrap().len(), 1);

    // After a compiler change, stat-verify must reject the entry —
    // this is the safety net that makes preservation across Clear safe.
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(2_000_000_000, 0),
    )
    .unwrap();
    std::fs::write(&compiler, b"different compiler bytes after upgrade").unwrap();
    let post_change = cache.get(&compiler);
    assert!(
        post_change.is_none(),
        "stat-verify must reject the entry once the compiler binary changes"
    );
}

/// Integration-level check: after Clear, the in-memory
/// `system_includes` cache is NOT empty if it was non-empty before.
/// Uses the `#[cfg(test)]` `test_insert_system_includes` /
/// `test_system_includes_len` seams to pre-populate and observe
/// without standing up a full compile pipeline (which would require
/// clang on PATH and would couple the test to the chosen toolchain).
#[tokio::test]
#[ignore] // integration-level: instantiates a real DaemonServer
async fn handle_clear_preserves_system_includes() {
    crate::test_support::test_timeout(async {
        let endpoint = crate::ipc::unique_test_endpoint();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = crate::core::NormalizedPath::new(tmp.path());
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

        let fake_compiler = tmp.path().join("fake-clang");
        std::fs::write(&fake_compiler, b"fake compiler bytes").unwrap();
        let synthetic_include_dir = tmp.path().join("include");
        std::fs::create_dir_all(&synthetic_include_dir).unwrap();
        server
            .test_insert_system_includes(
                crate::core::NormalizedPath::new(&fake_compiler),
                vec![crate::core::NormalizedPath::new(&synthetic_include_dir)],
            )
            .await;
        assert_eq!(
            server.test_system_includes_len().await,
            1,
            "test setup: synthetic entry must be installed"
        );

        // Drive handle_clear via the same internal call site that the
        // request handler uses. This bypasses IPC so we retain access
        // to the server to observe state after Clear.
        let response = super::super::handle_clear::handle_clear(server.test_state()).await;
        assert!(
            matches!(response, Response::Cleared { .. }),
            "expected Cleared response, got: {response:?}"
        );

        assert_eq!(
            server.test_system_includes_len().await,
            1,
            "issue #558: handle_clear must preserve system_includes entries — \
             they self-verify via stat-verify and re-discovery is expensive"
        );
    })
    .await;
}

#[tokio::test]
async fn handle_clear_preserves_in_flight_private_staging() {
    crate::test_support::test_timeout(async {
        let endpoint = crate::ipc::unique_test_endpoint();
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = crate::core::NormalizedPath::new(tmp.path());
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let staged = server.test_state().staging.path().join("active-output.o");
        std::fs::write(&staged, b"compiler result").unwrap();
        let published = tmp.path().join("published-output.o");
        std::fs::write(&published, b"cached result").unwrap();
        let key = "6".repeat(64);
        persist_staged_artifact_paths(
            server.test_state().artifact_dir.as_path(),
            &key,
            &[published.into()],
        )
        .unwrap();

        let response = super::super::handle_clear::handle_clear(server.test_state()).await;
        assert!(matches!(
            response,
            Response::Cleared {
                on_disk_bytes_freed,
                ..
            } if on_disk_bytes_freed >= 13
        ));
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            b"compiler result",
            "Clear must not delete a compiler result before salvage/materialization"
        );
        assert!(load_staged_artifact_paths(
            server.test_state().artifact_dir.as_path(),
            &key,
            &[13],
        )
        .unwrap()
        .is_none());
    })
    .await;
}
