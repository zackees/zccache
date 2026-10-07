//! The transport snapshot must replay real compiler results in a new store.

use super::*;
use zccache::artifact::snapshot::{export_store_snapshot, import_snapshot};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_restores_real_multi_output_hit_and_diagnostics_without_original_store() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).with_test_writer().try_init();
    let rustc = zccache::test_support::find_rustc().expect("rustc is required for snapshot replay");
    zccache::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let fresh = original.clone();
        let workspace = temp.path().join("workspace");
        create_tiny_project(&workspace);
        // Require actual diagnostics rather than a vacuous empty-stream check.
        std::fs::write(workspace.join("src/lib.rs"),
            "pub fn answer() -> i32 { 398 }\nfn unused_snapshot_warning() {}\n").unwrap();
        let _cache_env = CacheEnvGuard::new(&original);
        let args = rustc_multi_output_args();
        let outputs = expected_outputs(&workspace);
        let (endpoint, handle) = start_daemon_like_zccache_daemon().await;
        let mut client = connect(&endpoint).await;
        let session = start_session(&mut client, &workspace).await;
        let first = compile_rustc(&mut client, &session, rustc.as_path(), &args, &workspace).await;
        assert_eq!(first.exit_code, 0, "{}", String::from_utf8_lossy(&first.stderr));
        assert!(!first.cached);
        assert!(!first.stderr.is_empty(), "the compiler must emit a real warning");
        let bytes: Vec<Vec<u8>> = outputs.iter().map(|p| std::fs::read(p).unwrap()).collect();
        end_session(&mut client, session).await;
        shutdown_daemon(client, handle).await;

        let source = zccache::core::config::effective_cache_root_from_top_level(
            &NormalizedPath::from(&original));
        let destination = zccache::core::config::effective_cache_root_from_top_level(
            &NormalizedPath::from(&fresh));
        let snapshot = temp.path().join("transport");
        let compatibility = "a".repeat(64);
        let exported = export_store_snapshot(source.as_path(), &compatibility, &snapshot).unwrap();
        assert!(exported.entries > 0);
        let graph = std::fs::read(source.join("depgraph/depgraph.bin")).unwrap();
        let before = zccache::artifact::ArtifactStore::open(&source.join("index.bin")).unwrap();
        std::fs::remove_dir_all(&original).unwrap();
        assert_eq!(exported, import_snapshot(&snapshot, &compatibility, destination.as_path()).unwrap());
        std::fs::create_dir_all(destination.join("depgraph")).unwrap();
        std::fs::write(destination.join("depgraph/depgraph.bin"), graph).unwrap();
        let after = zccache::artifact::ArtifactStore::open(&destination.join("index.bin")).unwrap();
        for (key, meta) in before.load_all() {
            eprintln!("snapshot row key={key} outputs={:?} sizes={:?} staged_env={:?}", meta.output_names, meta.output_sizes, std::env::var_os("ZCCACHE_STAGED_ARTIFACTS"));
            let restored = after.get(&key).expect("imported index must retain every row");
            assert_eq!(serde_json::to_value(&meta).unwrap(), serde_json::to_value(&restored).unwrap());
            assert!(zccache::artifact::resolve_artifact_payloads(
                destination.join("artifacts").as_path(), &key, &meta.output_sizes, true,
                "snapshot-replay-test").unwrap().is_some());
        }
        std::fs::remove_dir_all(workspace.join("target")).unwrap();
        std::env::set_var(zccache::core::config::CACHE_DIR_ENV, &fresh);

        let (endpoint, handle) = start_daemon_like_zccache_daemon().await;
        let mut client = connect(&endpoint).await;
        let log = temp.path().join("replay.log");
        let session = start_session_with_log(&mut client, &workspace,
                                             Some(NormalizedPath::from(&log))).await;
        let second = compile_rustc(&mut client, &session, rustc.as_path(), &args, &workspace).await;
        assert_eq!(second.exit_code, first.exit_code);
        let status = get_status(&mut client).await;
        end_session(&mut client, session).await;
        assert!(second.cached, "first replay must hit: status={status:?}; log={}",
                std::fs::read_to_string(&log).unwrap_or_default());
        assert_eq!(second.stdout, first.stdout);
        assert_eq!(second.stderr, first.stderr);
        for (output, expected) in outputs.iter().zip(&bytes) {
            assert_eq!(&std::fs::read(output).unwrap(), expected, "{}", output.display());
        }
        shutdown_daemon(client, handle).await;
    }).await;
}
