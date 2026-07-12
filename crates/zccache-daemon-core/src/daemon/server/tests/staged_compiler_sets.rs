//! Real compiler coverage for staged multi-source C/C++ output sets.

use super::super::*;

async fn start_daemon(
    cache_dir: &NormalizedPath,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<Notify>,
    Arc<SharedState>,
) {
    let endpoint = crate::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, cache_dir).unwrap();
    let shutdown = server.shutdown_handle();
    let state = server.test_state_arc();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown, state)
}

fn assert_compile(response: Option<Response>, expected_cached: bool) {
    match response {
        Some(Response::CompileResult {
            exit_code,
            cached,
            stdout,
            stderr,
            ..
        }) => {
            assert_eq!(
                exit_code,
                0,
                "compiler stdout:\n{}\ncompiler stderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(cached, expected_cached);
        }
        other => panic!("expected CompileResult, got {other:?}"),
    }
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "integration: real clang-cl + daemon IPC"]
async fn staged_multi_source_clang_cl_restores_directory_outputs_from_response_file() {
    let Some(clang_cl) = crate::test_support::find_clang_cl() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let work_dir = temp.path().join("unicode-\u{03bb}");
        let object_dir = work_dir.join("objects");
        std::fs::create_dir_all(&object_dir).unwrap();
        let first = work_dir.join("first.c");
        let second = work_dir.join("second.cpp");
        std::fs::write(&first, "int first(void) { return 1; }\n").unwrap();
        std::fs::write(&second, "int second() { return 2; }\n").unwrap();

        let cache_dir: NormalizedPath = temp.path().join("cache").into();
        let (endpoint, server, shutdown, state) = start_daemon(&cache_dir).await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        let cwd: NormalizedPath = work_dir.clone().into();
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };

        let mut args = vec![
            "/c".to_string(),
            "/nologo".to_string(),
            format!("/Fo{}\\", object_dir.display()),
            "/Tc".to_string(),
            first.to_string_lossy().into_owned(),
            format!("/Tp{}", second.display()),
        ];
        for index in 0..1_600 {
            args.push(format!("/DZCCACHE_LONG_ARGUMENT_{index}=1"));
        }
        let request = || Request::Compile {
            session_id: session_id.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            compiler: clang_cl.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        };

        client.send(&request()).await.unwrap();
        assert_compile(client.recv().await.unwrap(), false);
        let first_object = object_dir.join("first.obj");
        let second_object = object_dir.join("second.obj");
        let expected_first = std::fs::read(&first_object).unwrap();
        let expected_second = std::fs::read(&second_object).unwrap();

        std::fs::remove_file(&first_object).unwrap();
        std::fs::remove_file(&second_object).unwrap();
        client.send(&request()).await.unwrap();
        assert_compile(client.recv().await.unwrap(), true);
        assert_eq!(std::fs::read(&first_object).unwrap(), expected_first);
        assert_eq!(std::fs::read(&second_object).unwrap(), expected_second);

        std::fs::write(&first_object, b"stale").unwrap();
        std::fs::write(&second_object, b"stale").unwrap();
        let mut first_permissions = std::fs::metadata(&first_object).unwrap().permissions();
        let mut second_permissions = std::fs::metadata(&second_object).unwrap().permissions();
        first_permissions.set_readonly(true);
        second_permissions.set_readonly(true);
        std::fs::set_permissions(&first_object, first_permissions).unwrap();
        std::fs::set_permissions(&second_object, second_permissions).unwrap();
        client.send(&request()).await.unwrap();
        assert_compile(client.recv().await.unwrap(), true);
        assert_eq!(std::fs::read(&first_object).unwrap(), expected_first);
        assert_eq!(std::fs::read(&second_object).unwrap(), expected_second);

        for output in [&first_object, &second_object] {
            let mut permissions = std::fs::metadata(output).unwrap().permissions();
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "this Windows-only test must clear FILE_ATTRIBUTE_READONLY before tempdir cleanup"
            )]
            permissions.set_readonly(false);
            std::fs::set_permissions(output, permissions).unwrap();
        }
        assert!(std::fs::read_dir(&state.depfile_tmpdir)
            .unwrap()
            .flatten()
            .all(|entry| entry.path().extension().is_none_or(|ext| ext != "rsp")));

        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "integration: real clang + daemon IPC"]
async fn staged_multi_source_publication_failure_salvages_and_retries_one_unit() {
    let Some(clang) = crate::test_support::find_clang() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir: NormalizedPath = temp.path().join("cache").into();
        let first = temp.path().join("first.c");
        let second = temp.path().join("second.c");
        std::fs::write(&first, "int first(void) { return 1; }\n").unwrap();
        std::fs::write(&second, "int second(void) { return 2; }\n").unwrap();
        let (endpoint, server, shutdown, state) = start_daemon(&cache_dir).await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        let cwd: NormalizedPath = temp.path().into();
        let args = vec![
            "-c".to_string(),
            "-MMD".to_string(),
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };
        let request = || Request::Compile {
            session_id: session_id.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        };

        let fault = StagedFaultGuard::arm(&state.artifact_dir, [StagedFaultPoint::PointerCommit]);
        client.send(&request()).await.unwrap();
        assert_compile(client.recv().await.unwrap(), false);
        let first_object = temp.path().join("first.o");
        let second_object = temp.path().join("second.o");
        let first_depfile = temp.path().join("first.d");
        let second_depfile = temp.path().join("second.d");
        assert!(first_object.is_file() && second_object.is_file());
        assert!(first_depfile.is_file() && second_depfile.is_file());
        assert!(!std::fs::read_to_string(&first_depfile)
            .unwrap()
            .contains(".multi-"));
        assert!(!std::fs::read_to_string(&second_depfile)
            .unwrap()
            .contains(".multi-"));
        fault.assert_all_consumed();
        let pointer_count = std::fs::read_dir(state.artifact_dir.join(".staged-v2"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "current"))
            .count();
        assert_eq!(pointer_count, 1);
        let staged = state.profiler.staged.snapshot();
        assert_eq!(staged.counters["publication_failure"], 1);
        assert_eq!(staged.counters["salvage_success"], 1);

        std::fs::remove_file(&first_object).unwrap();
        std::fs::remove_file(&second_object).unwrap();
        std::fs::remove_file(&first_depfile).unwrap();
        std::fs::remove_file(&second_depfile).unwrap();
        client.send(&request()).await.unwrap();
        assert_compile(client.recv().await.unwrap(), false);
        assert!(first_object.is_file() && second_object.is_file());
        assert!(first_depfile.is_file() && second_depfile.is_file());
        let pointer_count = std::fs::read_dir(state.artifact_dir.join(".staged-v2"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "current"))
            .count();
        assert_eq!(pointer_count, 2);

        std::fs::remove_file(&first_object).unwrap();
        std::fs::remove_file(&second_object).unwrap();
        std::fs::remove_file(&first_depfile).unwrap();
        std::fs::remove_file(&second_depfile).unwrap();
        client.send(&request()).await.unwrap();
        assert_compile(client.recv().await.unwrap(), true);
        assert!(first_object.is_file() && second_object.is_file());
        assert!(first_depfile.is_file() && second_depfile.is_file());
        let staged = state.profiler.staged.snapshot();
        assert_eq!(staged.counters["compiler_staged"], 3);
        assert_eq!(staged.counters["publication_success"], 2);

        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "integration: real clang + daemon IPC"]
async fn staged_multi_source_compiler_failure_publishes_and_materializes_nothing() {
    let Some(clang) = crate::test_support::find_clang() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir: NormalizedPath = temp.path().join("cache").into();
        let first = temp.path().join("broken.c");
        let second = temp.path().join("second.c");
        std::fs::write(&first, "this is not valid C\n").unwrap();
        std::fs::write(&second, "int second(void) { return 2; }\n").unwrap();
        let (endpoint, server, shutdown, state) = start_daemon(&cache_dir).await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        let cwd: NormalizedPath = temp.path().into();

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };
        client
            .send(&Request::Compile {
                session_id,
                args: vec![
                    "-c".into(),
                    first.to_string_lossy().into_owned(),
                    second.to_string_lossy().into_owned(),
                ],
                cwd: cwd.clone(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => assert_ne!(exit_code, 0),
            other => panic!("expected failed CompileResult, got {other:?}"),
        }
        assert!(!temp.path().join("broken.o").exists());
        assert!(!temp.path().join("second.o").exists());
        let pointer_count = std::fs::read_dir(state.artifact_dir.join(".staged-v2"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "current"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(pointer_count, 0);
        assert_eq!(
            state.profiler.staged.snapshot().counters["compiler_staged"],
            2
        );

        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "integration: real clang + daemon IPC"]
async fn staged_multi_source_materialization_failure_reports_error_before_index_visibility() {
    let Some(clang) = crate::test_support::find_clang() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir: NormalizedPath = temp.path().join("cache").into();
        let first = temp.path().join("first.c");
        let second = temp.path().join("second.c");
        let first_object = temp.path().join("first.o");
        let second_object = temp.path().join("second.o");
        std::fs::write(&first, "int first(void) { return 1; }\n").unwrap();
        std::fs::write(&second, "int second(void) { return 2; }\n").unwrap();
        let (endpoint, server, shutdown, state) = start_daemon(&cache_dir).await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        let cwd: NormalizedPath = temp.path().into();

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };
        let fault = StagedFaultGuard::arm(&second_object, [StagedFaultPoint::MaterializeOutput(0)]);
        client
            .send(&Request::Compile {
                session_id,
                args: vec![
                    "-c".into(),
                    first.to_string_lossy().into_owned(),
                    second.to_string_lossy().into_owned(),
                ],
                cwd,
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::Error { message }) => {
                assert!(message.contains("failed to materialize multi-source output"));
            }
            other => panic!("expected materialization Error, got {other:?}"),
        }
        fault.assert_all_consumed();
        assert!(first_object.exists());
        assert!(!second_object.exists());
        assert!(state.artifacts.is_empty());
        let staged = state.profiler.staged.snapshot();
        assert_eq!(staged.counters["materialize_failure"], 1);

        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}
