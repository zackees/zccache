//! zccache#1775: shutdown must not rewrite unchanged state.
//!
//! Before this fix, `DaemonServer::run`'s `Shutdown` arm unconditionally
//! rewrote `index.bin` (twice — once via the WAL flush, once via the final
//! `ArtifactStore::flush`), `depgraph.bin`, `metadata.bin`, the compiler-hash
//! cache and the system-includes cache, even when nothing had changed since
//! the daemon started (or since the last save). These tests exercise the
//! real IPC `Shutdown` path end to end (the same one `zccache stop` drives)
//! and assert on the files the shutdown sequence is allowed to touch.
//!
//! Every deferred loader is warmed explicitly (mirroring what the
//! `zccache-daemon` binary does after the readiness lockfile — see
//! `tests/metadata_deferred.rs`, `tests/compiler_hash.rs`,
//! `tests/system_includes_deferred.rs`, `tests/artifact_store_deferred.rs`)
//! so the shutdown save is gated purely on the new dirty-flag logic, not on
//! the pre-existing "load still pending" skip.

use super::super::*;

/// One daemon, fully warmed (all four deferred loaders installed) and never
/// touched by a single compile or cache-hit. `Shutdown` must persist nothing:
/// no `index.bin`, no `depgraph.bin`, no `metadata.bin`, no compiler-hash
/// cache, no system-includes cache.
#[tokio::test]
async fn shutdown_of_idle_daemon_with_no_changes_persists_nothing() {
    let cache_root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = cache_root.path().join("cache").into();
    let endpoint = crate::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    // Warm every deferred loader against an empty cache root — matches a
    // real daemon shortly after startup, before any client has compiled
    // anything.
    server.metadata_cache_loader().load_and_install();
    server.compiler_hash_cache_loader().load_and_install();
    // `SystemIncludesLoader::load_and_install` uses `blocking_lock()`, which
    // panics inside a runtime; production calls it from
    // `tokio::task::spawn_blocking`, so do the same here.
    let loader = server.system_includes_loader();
    tokio::task::spawn_blocking(move || loader.load_and_install())
        .await
        .unwrap();
    server.artifact_store_loader().load_and_install();

    let state = server.test_state_arc();
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let depgraph_path = depgraph_file_path_for_cache_dir(&cache_dir);
    let metadata_path = state.metadata_path.clone();
    let compiler_hash_path = state.compiler_hash_cache_path.clone();
    let system_includes_path = state.system_includes_cache_path.clone();

    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });

    let mut client = crate::ipc::connect(&endpoint).await.unwrap();
    client.send(&Request::Shutdown).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::ShuttingDown));
    handle.await.unwrap();

    assert!(
        !index_path.as_path().exists(),
        "idle shutdown must not write index.bin"
    );
    assert!(
        !depgraph_path.as_path().exists(),
        "idle shutdown must not write depgraph.bin"
    );
    assert!(
        !metadata_path.as_path().exists(),
        "idle shutdown must not write metadata.bin"
    );
    assert!(
        !compiler_hash_path.as_path().exists(),
        "idle shutdown must not write the compiler-hash cache"
    );
    assert!(
        !system_includes_path.as_path().exists(),
        "idle shutdown must not write the system-includes cache"
    );
}

/// Companion no-data-loss test: once every subsystem has real state, the
/// same `Shutdown` path must still persist all five artifacts. Guards
/// against the dirty flag ever being over-eager and silently skipping a
/// save that was actually needed.
#[tokio::test]
async fn shutdown_persists_every_cache_when_dirty() {
    let cache_root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = cache_root.path().join("cache").into();
    let endpoint = crate::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.metadata_cache_loader().load_and_install();
    server.compiler_hash_cache_loader().load_and_install();
    // `SystemIncludesLoader::load_and_install` uses `blocking_lock()`, which
    // panics inside a runtime; production calls it from
    // `tokio::task::spawn_blocking`, so do the same here.
    let loader = server.system_includes_loader();
    tokio::task::spawn_blocking(move || loader.load_and_install())
        .await
        .unwrap();
    server.artifact_store_loader().load_and_install();

    let state = server.test_state_arc();
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let depgraph_path = depgraph_file_path_for_cache_dir(&cache_dir);
    let metadata_path = state.metadata_path.clone();
    let compiler_hash_path = state.compiler_hash_cache_path.clone();
    let system_includes_path = state.system_includes_cache_path.clone();

    // Dirty the artifact index directly (mirrors a compile-success persist).
    state.artifact_store.insert(
        "deadbeef",
        &crate::artifact::ArtifactIndex::new(
            vec!["output.o".to_string()],
            vec![4096],
            Vec::new(),
            Vec::new(),
            0,
        ),
    );

    // Dirty the depgraph via a real registration.
    let source = cache_root.path().join("main.cpp");
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    state
        .dep_graph
        .load()
        .register(crate::depgraph::CompileContext {
            source_file: source.into(),
            include_search: crate::depgraph::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
            compiler_hash: crate::hash::hash_bytes(b"fixture"),
        });

    // Dirty the metadata cache.
    let tracked = cache_root.path().join("tracked.h");
    std::fs::write(&tracked, b"tracked header").unwrap();
    state.cache_system.metadata().insert(
        crate::core::NormalizedPath::new(&tracked),
        crate::fscache::FileMetadata {
            mtime: std::time::SystemTime::now(),
            size: 14,
            content_hash: Some([0x11; 32]),
            confidence: crate::fscache::Confidence::High,
            last_verified: std::time::Instant::now(),
        },
    );

    // Dirty the compiler-hash cache via the real hashing entrypoint.
    let compiler = cache_root.path().join("cc");
    std::fs::write(&compiler, b"fake compiler").unwrap();
    state
        .compiler_hash_cache
        .get_or_hash_with(&compiler, |_| Some(crate::hash::hash_bytes(b"compiler-id")));

    // Dirty the system-includes cache.
    {
        let mut includes = state.system_includes.lock().await;
        includes.insert(
            crate::core::NormalizedPath::new(&compiler),
            vec![crate::core::NormalizedPath::new(cache_root.path())],
        );
    }

    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });

    let mut client = crate::ipc::connect(&endpoint).await.unwrap();
    client.send(&Request::Shutdown).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::ShuttingDown));
    handle.await.unwrap();

    assert!(
        index_path.as_path().exists(),
        "a dirty artifact store must still be persisted at shutdown"
    );
    assert!(
        depgraph_path.as_path().exists(),
        "a dirty depgraph must still be persisted at shutdown"
    );
    assert!(
        metadata_path.as_path().exists(),
        "a dirty metadata cache must still be persisted at shutdown"
    );
    assert!(
        compiler_hash_path.as_path().exists(),
        "a dirty compiler-hash cache must still be persisted at shutdown"
    );
    assert!(
        system_includes_path.as_path().exists(),
        "a dirty system-includes cache must still be persisted at shutdown"
    );
}
