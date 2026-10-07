//! The transport snapshot must replay real compiler results in a new store.

use super::*;
use zccache::artifact::snapshot::{export_store_snapshot, import_snapshot};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_restores_real_multi_output_hit_and_diagnostics_without_original_store() {
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
        std::fs::remove_dir_all(&original).unwrap();
        assert_eq!(exported, import_snapshot(&snapshot, &compatibility, destination.as_path()).unwrap());
        std::fs::remove_dir_all(workspace.join("target")).unwrap();
        std::env::set_var(zccache::core::config::CACHE_DIR_ENV, &fresh);

        let (endpoint, handle) = start_daemon_like_zccache_daemon().await;
        let mut client = connect(&endpoint).await;
        let session = start_session(&mut client, &workspace).await;
        let second = compile_rustc(&mut client, &session, rustc.as_path(), &args, &workspace).await;
        assert_eq!(second.exit_code, first.exit_code);
        assert!(second.cached, "first replay from the imported store must be a hit");
        assert_eq!(second.stdout, first.stdout);
        assert_eq!(second.stderr, first.stderr);
        for (output, expected) in outputs.iter().zip(&bytes) {
            assert_eq!(&std::fs::read(output).unwrap(), expected, "{}", output.display());
        }
        end_session(&mut client, session).await;
        shutdown_daemon(client, handle).await;
    }).await;
}
