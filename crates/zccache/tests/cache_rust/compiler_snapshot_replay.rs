//! The transport snapshot must replay real compiler results in a new store.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_restores_real_multi_output_hit_and_diagnostics_without_original_store() {
    let rustc = zccache::test_support::find_rustc().expect("rustc is required for snapshot replay");
    zccache::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
                let workspace = temp.path().join("workspace");
        create_tiny_project(&workspace);
        // Require actual diagnostics rather than a vacuous empty-stream check.
        std::fs::write(
            workspace.join("src/lib.rs"),
            "pub fn answer() -> i32 { 398 }\nfn unused_snapshot_warning() {}\n",
        )
        .unwrap();
        let _cache_env = CacheEnvGuard::new(&original);
        let args = rustc_multi_output_args();
        let outputs = expected_outputs(&workspace);
        let (endpoint, handle) = start_daemon_like_zccache_daemon().await;
        let mut client = connect(&endpoint).await;
        let session = start_session(&mut client, &workspace).await;
        let first = compile_rustc(&mut client, &session, rustc.as_path(), &args, &workspace).await;
        assert_eq!(
            first.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(!first.cached);
        assert!(
            !first.stderr.is_empty(),
            "the compiler must emit a real warning"
        );
        let bytes: Vec<Vec<u8>> = outputs.iter().map(|p| std::fs::read(p).unwrap()).collect();
        end_session(&mut client, session).await;
        shutdown_daemon(client, handle).await;

        std::fs::remove_dir_all(workspace.join("target")).unwrap();

        let (endpoint, handle) = start_daemon_like_zccache_daemon().await;
        let mut client = connect(&endpoint).await;
        let log = temp.path().join("replay.log");
        let session =
            start_session_with_log(&mut client, &workspace, Some(NormalizedPath::from(&log))).await;
        let second = compile_rustc(&mut client, &session, rustc.as_path(), &args, &workspace).await;
        assert_eq!(second.exit_code, first.exit_code);
        let status = get_status(&mut client).await;
        end_session(&mut client, session).await;
        assert!(
            second.cached,
            "first replay must hit: status={status:?}; log={}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert_eq!(second.stdout, first.stdout);
        assert_eq!(second.stderr, first.stderr);
        for (output, expected) in outputs.iter().zip(&bytes) {
            assert_eq!(
                &std::fs::read(output).unwrap(),
                expected,
                "{}",
                output.display()
            );
        }
        std::fs::write(
            workspace.join("src/lib.rs"),
            "pub fn answer() -> i32 { 399 }\nfn unused_snapshot_warning() {}\n",
        )
        .unwrap();
        let changed_log = temp.path().join("changed.log");
        let session = start_session_with_log(&mut client, &workspace,
            Some(NormalizedPath::from(&changed_log))).await;
        let changed =
            compile_rustc(&mut client, &session, rustc.as_path(), &args, &workspace).await;
        end_session(&mut client, session).await;
        assert_eq!(changed.exit_code, 0);
        assert!(
            !changed.cached,
            "an imported context must not authorize stale source bytes: {}",
            std::fs::read_to_string(&changed_log).unwrap_or_default()
        );
        assert_ne!(std::fs::read(&outputs[0]).unwrap(), bytes[0]);
        shutdown_daemon(client, handle).await;
    })
    .await;
}
