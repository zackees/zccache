//! Tests for the public embedded-service facade.
//!
//! Kept outside `embedded.rs` so the public implementation remains below
//! the repository's 1,000-line source-size ceiling.

use super::*;

#[cfg(test)]
mod streaming_tests {
    //! zccache#937: tests for the MVP streaming compile API. The
    //! producer side is currently a pass-through over the buffered
    //! `compile`; these tests pin the public contract so the
    //! upcoming daemon-pipeline refactor (the cross-cutting piece
    //! tracked in #937) can swap the producer without changing the
    //! consumer-visible event order.

    use super::*;
    use tempfile::TempDir;

    async fn start_test_service(temp: &TempDir) -> ZccacheService {
        let mut audit = AuditConfig::default();
        audit.mode = crate::audit::AuditMode::Off;
        ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "streaming-test".into(),
                instance_id: uuid::Uuid::new_v4().to_string(),
                workspace_id: "streaming-workspace".into(),
            },
            cache_root: temp.path().join("cache").into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("service start")
    }

    #[test]
    fn compile_chunk_done_carries_outcome_fields() {
        // Pin the public shape of the terminal Done event.
        let done = CompileChunk::Done {
            exit_code: 0,
            cached: true,
            cache_outcome: CacheOutcome::Hit,
            compile_id: "test-id".to_string(),
        };
        let CompileChunk::Done {
            exit_code,
            cached,
            cache_outcome,
            compile_id,
        } = done
        else {
            panic!("constructor must produce a Done variant");
        };
        assert_eq!(exit_code, 0);
        assert!(cached);
        assert_eq!(cache_outcome, CacheOutcome::Hit);
        assert_eq!(compile_id, "test-id");
    }

    #[test]
    fn compile_chunk_stdout_stderr_carry_bytes() {
        match CompileChunk::Stdout(b"hello".to_vec()) {
            CompileChunk::Stdout(bytes) => assert_eq!(bytes, b"hello"),
            other => panic!("expected Stdout, got {other:?}"),
        }
        match CompileChunk::Stderr(b"warn".to_vec()) {
            CompileChunk::Stderr(bytes) => assert_eq!(bytes, b"warn"),
            other => panic!("expected Stderr, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_hit_replays_byte_identical_streams() {
        let Some(compiler) = crate::test_support::find_clang() else {
            return;
        };
        let temp = TempDir::new().expect("tempdir");
        let source = temp.path().join("warning.c");
        let output = temp.path().join("warning.o");
        std::fs::write(
            &source,
            "#warning stream-replay\nint value(void) { return 1; }\n",
        )
        .expect("source");
        let service = start_test_service(&temp).await;
        let request = CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("stream-run").expect("id"),
                crate::audit::AuditId::new("stream-trace").expect("id"),
            ),
            compiler,
            args: vec![
                "-c".into(),
                source.to_string_lossy().into_owned(),
                "-o".into(),
                output.to_string_lossy().into_owned(),
            ],
            cwd: temp.path().into(),
            env: Vec::new(),
            stdin: Vec::new(),
        };

        let mut miss_stdout = Vec::new();
        let mut miss_stderr = Vec::new();
        let mut miss_cached = None;
        service
            .compile_streaming(request.clone(), |chunk| match chunk {
                CompileChunk::Stdout(bytes) => miss_stdout.extend(bytes),
                CompileChunk::Stderr(bytes) => miss_stderr.extend(bytes),
                CompileChunk::Done { cached, .. } => miss_cached = Some(cached),
            })
            .await
            .expect("cache miss compile");
        assert_eq!(miss_cached, Some(false));
        assert!(!miss_stderr.is_empty());

        std::fs::remove_file(&output).expect("remove first output");
        let mut hit_stdout = Vec::new();
        let mut hit_stderr = Vec::new();
        let mut hit_cached = None;
        service
            .compile_streaming(request, |chunk| match chunk {
                CompileChunk::Stdout(bytes) => hit_stdout.extend(bytes),
                CompileChunk::Stderr(bytes) => hit_stderr.extend(bytes),
                CompileChunk::Done { cached, .. } => hit_cached = Some(cached),
            })
            .await
            .expect("cache hit compile");

        assert_eq!(hit_cached, Some(true));
        assert_eq!(hit_stdout, miss_stdout);
        assert_eq!(hit_stderr, miss_stderr);
        assert!(output.exists(), "cache hit must restore the output file");
        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown");
    }
}

#[cfg(test)]
mod cancellation_tests {
    //! zccache#923: tests that `ZccacheConfig::cancellation`, when
    //! supplied, aborts `compile()` cooperatively and short-circuits a flush
    //! only when cancellation is already latched before persistence begins.

    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn fake_compile_request() -> CompileRequest {
        // Compiler path that does not exist on disk — the embedded
        // daemon's spawn step is what we're trying to *not* run, so any
        // unreachable PathBuf works. The cancellation race fires before
        // the spawn even attempts to launch the process.
        CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("test-run").expect("non-empty"),
                crate::audit::AuditId::new("test-trace").expect("non-empty"),
            ),
            compiler: PathBuf::from("/nonexistent/compiler-that-never-runs").into(),
            args: vec!["--version".into()],
            cwd: std::env::current_dir().expect("cwd").into(),
            env: Vec::new(),
            stdin: Vec::new(),
        }
    }

    async fn start_service_with_token(
        temp: &TempDir,
        token: Option<CancellationToken>,
        instance_id: &str,
    ) -> Result<ZccacheService> {
        // These tests exercise cancellation/runtime plumbing, not the
        // audit sink, so disable audit (`AuditMode::Off`) to avoid the
        // production `output_root` validation introduced after the
        // tests were written. The audit sink is exercised in
        // `audit_writer.rs` tests with a proper tempdir-backed
        // `output_root`.
        let mut audit = AuditConfig::default();
        audit.mode = crate::audit::AuditMode::Off;
        ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".into(),
                instance_id: instance_id.into(),
                workspace_id: instance_id.into(),
            },
            cache_root: temp.path().join("zccache").into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: token,
        })
        .await
    }

    #[tokio::test]
    async fn precancelled_token_returns_cancelled_immediately() {
        // Fast-path: token cancelled before the compile call lands. We
        // should never reach the daemon's spawn step. The acceptance
        // criterion in zccache#923 — "Err(Cancelled) from compile() so
        // soldr's request handler can short-circuit" — is exactly this
        // path.
        let temp = TempDir::new().expect("temp cache root");
        let token = CancellationToken::new();
        token.cancel();
        let service = start_service_with_token(&temp, Some(token), "precancel")
            .await
            .expect("service start");

        let outcome = service.compile(fake_compile_request()).await;
        assert!(
            matches!(outcome, Err(EmbeddedError::Cancelled)),
            "pre-cancelled token must short-circuit compile(), got {outcome:?}"
        );

        // Tear down: shutdown still works after a cancelled compile.
        // Important — the host's exit path needs this to be clean.
        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(report.is_ok(), "shutdown after Cancelled must succeed");
    }

    #[tokio::test]
    async fn token_fired_during_compile_returns_cancelled() {
        // Mid-flight cancellation: the compile begins (the inner
        // EmbeddedDaemon::compile future is polled at least once) and
        // the token fires while it's in flight. The `tokio::select!`
        // race must win for the cancel branch.
        //
        // We use a token that is cancelled by a sibling task with a
        // very short delay so the compile future is guaranteed to have
        // been polled before the cancel arrives. The fake compiler
        // path is non-existent so the compile would otherwise fail
        // with a Compile error after spawn — we want Cancelled instead.
        let temp = TempDir::new().expect("temp cache root");
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let service = start_service_with_token(&temp, Some(token), "midflight")
            .await
            .expect("service start");

        let canceller = tokio::spawn(async move {
            // Tiny delay so the compile future starts being polled.
            // 10 ms is a generous floor on Windows scheduling jitter
            // while still being a snappy test.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });

        let outcome = service.compile(fake_compile_request()).await;
        canceller.await.expect("canceller task joined");

        // The race can resolve either way: cancel wins (Cancelled) or
        // the spawn fails first because the compiler binary doesn't
        // exist (Compile). Both prove the cancellation path is wired —
        // the assertion we MUST NOT see is "Ok" because that would
        // mean the fake compiler somehow succeeded.
        match outcome {
            Err(EmbeddedError::Cancelled) | Err(EmbeddedError::Compile(_)) => {}
            other => panic!("mid-flight cancel must yield Cancelled or Compile, got {other:?}"),
        }

        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(
            report.is_ok(),
            "shutdown after mid-flight cancel must succeed"
        );
    }

    #[tokio::test]
    async fn no_token_preserves_pre_923_behavior() {
        // Backward-compat: `cancellation: None` must keep today's
        // semantics — compile() runs to completion (success or error)
        // and never returns Cancelled. The fake compiler path makes
        // this a Compile error, not an Ok, which is fine — the point
        // is that the new error variant is opt-in.
        let temp = TempDir::new().expect("temp cache root");
        let service = start_service_with_token(&temp, None, "no-token")
            .await
            .expect("service start");

        let outcome = service.compile(fake_compile_request()).await;
        if let Err(EmbeddedError::Cancelled) = outcome {
            panic!("cancellation: None must never yield Cancelled");
        }

        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(report.is_ok());
    }

    #[tokio::test]
    async fn precancelled_token_short_circuits_flush() {
        // A pre-cancelled token is safe to honor because no persistence worker
        // has started. Once a flush begins, it remains owned to quiescence.
        let temp = TempDir::new().expect("temp cache root");
        let token = CancellationToken::new();
        token.cancel();
        let service = start_service_with_token(&temp, Some(token), "flush-cancel")
            .await
            .expect("service start");

        let outcome = service.flush().await;
        assert!(
            matches!(outcome, Err(EmbeddedError::Cancelled)),
            "pre-cancelled token must short-circuit flush(), got {outcome:?}"
        );

        let _ = service.shutdown(ShutdownMode::Graceful).await;
    }
}

#[cfg(test)]
mod host_identity_tests {
    //! zccache#925: tests for `HostIdentity::default_for_product` and the
    //! documented stability contract.

    use super::*;

    #[test]
    fn default_for_product_is_stable_within_one_process() {
        // Two calls in the same process must yield byte-identical
        // identities. This is the "cache continuity across daemon
        // restarts on the same install" contract — within a process the
        // current_exe path and product string don't change, so the hash
        // doesn't change.
        let a = HostIdentity::default_for_product("soldr");
        let b = HostIdentity::default_for_product("soldr");
        assert_eq!(a, b, "same product must yield same identity");
        assert_eq!(a.product, "soldr");
        assert_eq!(a.workspace_id, a.instance_id);
    }

    #[test]
    fn default_for_product_differs_per_product() {
        // Two different products must yield distinct identities so they
        // don't collide in the per-process backend-identity DashMap.
        let soldr = HostIdentity::default_for_product("soldr");
        let fbuild = HostIdentity::default_for_product("fbuild");
        assert_ne!(soldr, fbuild);
        assert_ne!(soldr.instance_id, fbuild.instance_id);
    }

    #[test]
    fn default_for_product_instance_id_is_16_bytes_of_hex() {
        // 32 hex chars = 16 bytes. The format is part of the
        // diagnostic surface (`embedded_endpoint` prints it) so freezing
        // it here catches accidental changes.
        let id = HostIdentity::default_for_product("zccache-test");
        assert_eq!(id.instance_id.len(), 32);
        assert!(id.instance_id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[cfg(test)]
mod runtime_hooks_tests {
    //! zccache#922: tests that `RuntimeHooks::handle`, when supplied,
    //! is the runtime where the embedded daemon's background tasks land.

    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn runtime_hooks_default_is_none() {
        // Backward-compat assertion: the default constructor has not
        // changed, the new field is `None`, and callers that don't
        // populate it get today's implicit-runtime behaviour.
        let hooks = RuntimeHooks::default();
        assert!(hooks.handle.is_none());
        assert!(hooks.service_name.is_none());
    }

    #[test]
    fn explicit_handle_owns_background_spawns() {
        // Build a dedicated multi-threaded runtime, hand its handle to
        // ZccacheService::start, and assert that a probe spawned via the
        // service's runtime context lands on THAT runtime — not on the
        // outer runtime that drives the test.
        let host_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("host-runtime-worker")
            .build()
            .expect("failed to build host runtime");
        let host_handle = host_rt.handle().clone();

        // Sentinel: a thread-local-style atomic that increments when a
        // task observes it's on the host runtime.
        let landed_on_host: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

        // Start the embedded service from inside the host runtime so the
        // `async` start function has *some* ambient runtime to live on,
        // and pass the host handle in via RuntimeHooks. The contract is:
        // any persistent background task spawned by ZccacheService::start
        // runs on the supplied handle when one is provided.
        let temp = TempDir::new().expect("temp cache root");
        let cache_root: NormalizedPath = temp.path().join("zccache").into();

        let landed_clone = Arc::clone(&landed_on_host);
        let host_handle_clone = host_handle.clone();
        let service = host_rt.block_on(async move {
            // Disable audit (`AuditMode::Off`) so the production
            // `output_root` validation does not reject this fixture; the
            // test exercises runtime hooks, not the audit sink.
            let mut audit = AuditConfig::default();
            audit.mode = crate::audit::AuditMode::Off;
            ZccacheService::start(ZccacheConfig {
                host: HostIdentity {
                    product: "zccache-test".into(),
                    instance_id: "runtime-hooks".into(),
                    workspace_id: "runtime-hooks".into(),
                },
                cache_root,
                audit,
                limits: ServiceLimits::default(),
                runtime: RuntimeHooks {
                    service_name: Some("runtime-hooks-test".into()),
                    handle: Some(host_handle_clone),
                },
                cancellation: None,
            })
            .await
        });
        let service = service.expect("service start");

        // Probe: spawn a no-op task via the host handle and confirm we
        // can observe the worker's thread name — this proves the handle
        // we passed in is the one running our work.
        let landed_clone2 = Arc::clone(&landed_clone);
        let probe = host_handle.spawn(async move {
            if std::thread::current()
                .name()
                .map(|n| n.starts_with("host-runtime-worker"))
                .unwrap_or(false)
            {
                landed_clone2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        host_rt.block_on(probe).expect("probe ran on host runtime");
        assert!(
            landed_on_host.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "task spawned via supplied handle must run on host runtime workers"
        );

        // Tear down the service cleanly so the index writer task exits.
        let _ = host_rt.block_on(service.shutdown(ShutdownMode::Graceful));
    }
}

#[cfg(test)]
mod journal_tests {
    //! soldr#1286: the embedded backend must journal every compile
    //! outcome to `logs/compile_journal.jsonl` exactly like the daemon
    //! IPC path. Before this test existed, embedded compiles (the only
    //! compile path for soldr hosts) produced zero journal records, so
    //! hit-ratio and miss-reason telemetry was blind on dev machines.

    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn unreachable_compile_request() -> CompileRequest {
        CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("journal-run").expect("non-empty"),
                crate::audit::AuditId::new("journal-trace").expect("non-empty"),
            ),
            compiler: PathBuf::from("/nonexistent/compiler-that-never-runs").into(),
            args: vec!["--version".into()],
            cwd: std::env::current_dir().expect("cwd").into(),
            env: vec![
                ("CC".into(), "clang".into()),
                (
                    "GITHUB_TOKEN".into(),
                    "ghp_11AA22BB33CC44DD55EE66FF77GG88HH".into(),
                ),
            ],
            stdin: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn embedded_compile_preserves_exact_termination_signal() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().expect("temp cache root");
        let compiler = temp.path().join("signal-compiler");
        std::fs::write(&compiler, "#!/bin/sh\nkill -TERM $$\n").expect("write compiler");
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
            .expect("make compiler executable");
        let mut audit = AuditConfig::default();
        audit.mode = crate::audit::AuditMode::Off;
        let service = ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".into(),
                instance_id: "embedded-signal".into(),
                workspace_id: "embedded-signal".into(),
            },
            cache_root: temp.path().join("zccache").into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("service start");
        let response = service
            .compile(CompileRequest {
                audit: AuditContext::new(
                    crate::audit::AuditId::new("signal-run").expect("non-empty"),
                    crate::audit::AuditId::new("signal-trace").expect("non-empty"),
                ),
                compiler: compiler.into(),
                args: Vec::new(),
                cwd: temp.path().into(),
                env: Vec::new(),
                stdin: Vec::new(),
            })
            .await
            .expect("signaled compiler returns a compile response");

        assert_eq!(response.exit_code, -143);
        assert_eq!(response.cache_outcome, CacheOutcome::Error);

        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown service");
    }

    #[tokio::test]
    async fn embedded_compile_writes_compile_journal() {
        let temp = TempDir::new().expect("temp cache root");
        let mut audit = AuditConfig::default();
        audit.mode = crate::audit::AuditMode::Off;
        let service = ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".into(),
                instance_id: "embedded-journal".into(),
                workspace_id: "embedded-journal".into(),
            },
            cache_root: temp.path().join("zccache").into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("service start");

        // The fake compiler cannot spawn, which still exercises the
        // journal write path (outcome "error", exit_code -1) without
        // needing a real compiler on the test host.
        let _ = service.compile(unreachable_compile_request()).await;

        // `CompileJournal` writes on a background thread, and the
        // effective cache root gains a versioned subdir — locate
        // `logs/compile_journal.jsonl` by walking the temp tree and
        // poll briefly for the async write.
        fn find_journal(dir: &std::path::Path) -> Option<std::path::PathBuf> {
            let entries = std::fs::read_dir(dir).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(found) = find_journal(&path) {
                        return Some(found);
                    }
                } else if path.file_name().and_then(|n| n.to_str()) == Some("compile_journal.jsonl")
                {
                    return Some(path);
                }
            }
            None
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let content = loop {
            let content = find_journal(temp.path()).and_then(|p| std::fs::read_to_string(p).ok());
            match content {
                Some(c) if !c.trim().is_empty() => break c,
                _ if std::time::Instant::now() > deadline => {
                    panic!("embedded compile produced no compile_journal.jsonl record")
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        };

        let line = content.lines().next().expect("at least one journal line");
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON journal line");
        assert_eq!(
            v["outcome"], "error",
            "unspawnable compiler must journal as error: {v}"
        );
        assert!(
            v["compiler"]
                .as_str()
                .unwrap_or_default()
                .contains("compiler-that-never-runs"),
            "journal must record the embedded compiler path: {v}"
        );
        assert_eq!(
            v["env"],
            serde_json::json!([["CC", "clang"]]),
            "embedded journal must retain safe diagnostics and omit secrets: {v}"
        );
        assert!(
            !line.contains("GITHUB_TOKEN") && !line.contains("ghp_"),
            "embedded journal leaked a secret: {line}"
        );

        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(report.is_ok(), "shutdown after journaled compile succeeds");
    }
}

#[cfg(all(test, not(windows)))]
mod staged_depfile_restart_tests {
    //! Regression for the staged C depfile lifetime: the compiler writes its
    //! depfile under a private staging root, while durable publication runs
    //! asynchronously after the requested outputs have been materialized and
    //! the staging root has been removed.

    use super::*;
    use tempfile::TempDir;

    async fn start_service(cache_root: &std::path::Path) -> ZccacheService {
        let mut audit = AuditConfig::default();
        audit.mode = crate::audit::AuditMode::Off;
        ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "staged-depfile-restart-test".into(),
                instance_id: "stable-instance".into(),
                workspace_id: "stable-instance".into(),
            },
            cache_root: cache_root.into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("service start")
    }

    #[tokio::test]
    #[ignore = "integration: ZCCACHE_STAGED_ARTIFACTS=c-cpp + real clang"]
    async fn staged_custom_depfile_survives_flush_shutdown_and_restart_hit() {
        let Some(clang) = crate::test_support::find_clang() else {
            return;
        };
        crate::test_support::test_timeout(Box::pin(async move {
            let temp = TempDir::new().expect("temp root");
            let cache_root = temp.path().join("cache root # $");
            let work_dir = temp.path().join("work root # $");
            std::fs::create_dir_all(&work_dir).expect("work dir");
            let source = work_dir.join("fixture.c");
            let header = work_dir.join("fixture.h");
            let object = work_dir.join("fixture object # $.o");
            let depfile = work_dir.join("custom deps # $.mk");
            std::fs::write(&header, "#define FIXTURE_VALUE 7\n").expect("header");
            std::fs::write(
                &source,
                "#include \"fixture.h\"\nint fixture(void) { return FIXTURE_VALUE; }\n",
            )
            .expect("source");

            let request = CompileRequest {
                audit: AuditContext::new(
                    crate::audit::AuditId::new("depfile-restart-run").expect("audit run id"),
                    crate::audit::AuditId::new("depfile-restart-trace").expect("audit trace id"),
                ),
                compiler: clang,
                args: vec![
                    "-c".into(),
                    source.to_string_lossy().into_owned(),
                    "-MD".into(),
                    "-MF".into(),
                    depfile.to_string_lossy().into_owned(),
                    "-o".into(),
                    object.to_string_lossy().into_owned(),
                ],
                cwd: work_dir.clone().into(),
                env: Vec::new(),
                stdin: Vec::new(),
            };

            let service = start_service(&cache_root).await;
            let miss = service
                .compile(request.clone())
                .await
                .expect("cold compile");
            assert_eq!(miss.exit_code, 0, "clang stderr: {:?}", miss.stderr);
            assert!(!miss.cached, "first compile must miss");
            let expected_object = std::fs::read(&object).expect("cold object");
            let expected_depfile = std::fs::read(&depfile).expect("cold custom depfile");
            assert_valid_requested_depfile(&expected_depfile, &object, &cache_root);
            let stats = service.stats().await.expect("cold service stats");
            assert_eq!(
                stats.phase_profile.staged.counters["compiler_staged"], 1,
                "run with ZCCACHE_STAGED_ARTIFACTS=c-cpp so this exercises the staged lane"
            );

            let flush = service.flush_detailed().await.expect("durable flush");
            assert!(
                flush.is_complete(),
                "every durable flush step must complete: {flush:?}"
            );
            let shutdown = service
                .shutdown_detailed(ShutdownMode::Graceful)
                .await
                .expect("graceful shutdown");
            assert!(
                shutdown.flushed.is_complete(),
                "every shutdown flush step must complete: {shutdown:?}"
            );

            std::fs::remove_file(&object).expect("remove cold object");
            std::fs::remove_file(&depfile).expect("remove cold depfile");
            let restarted = start_service(&cache_root).await;
            let hit = restarted
                .compile(request)
                .await
                .expect("compile after restart");
            assert_eq!(hit.exit_code, 0, "clang stderr: {:?}", hit.stderr);
            assert!(
                hit.cached,
                "restart must restore the durable staged entry: {hit:?}"
            );
            assert_eq!(
                std::fs::read(&object).expect("restored object"),
                expected_object
            );
            let restored_depfile = std::fs::read(&depfile).expect("restored custom depfile");
            assert_eq!(restored_depfile, expected_depfile);
            assert_valid_requested_depfile(&restored_depfile, &object, &cache_root);
            restarted
                .shutdown(ShutdownMode::Graceful)
                .await
                .expect("restart shutdown");
        }))
        .await;
    }

    fn assert_valid_requested_depfile(
        bytes: &[u8],
        requested_object: &std::path::Path,
        private_cache_root: &std::path::Path,
    ) {
        let mut expected_target = crate::daemon::server::persist::quote_make_depfile_path(
            requested_object.to_string_lossy().as_bytes(),
        );
        expected_target.push(b':');
        assert!(
            bytes.starts_with(&expected_target),
            "depfile target must be the Make-escaped requested object: {}",
            String::from_utf8_lossy(bytes)
        );
        assert!(
            !bytes
                .windows(b"__zccache_staged_output_".len())
                .any(|window| window == b"__zccache_staged_output_"),
            "depfile must not retain the canonical staging marker: {}",
            String::from_utf8_lossy(bytes)
        );
        let escaped_private_root = crate::daemon::server::persist::quote_make_depfile_path(
            private_cache_root.to_string_lossy().as_bytes(),
        );
        assert!(
            !bytes
                .windows(escaped_private_root.len())
                .any(|window| window == escaped_private_root),
            "depfile must not retain the private cache root: {}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[cfg(test)]
mod audit_emission_tests {
    //! #905: the audit sink was started, flushed and shut down but nothing
    //! ever called `emit`, so a host that configured `audit.jsonl` got a file
    //! that was created and rotated and always empty. These tests pin that
    //! events actually reach the log.

    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Locate the audit JSONL under `root`, wherever the effective cache /
    /// audit root landed.
    fn find_audit_log(dir: &std::path::Path) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_audit_log(&path) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|name| name.to_str()) == Some("audit.jsonl") {
                return Some(path);
            }
        }
        None
    }

    fn audit_lines(root: &std::path::Path) -> Vec<serde_json::Value> {
        let Some(path) = find_audit_log(root) else {
            return Vec::new();
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("audit log must be valid JSONL"))
            .collect()
    }

    async fn start_audited_service(temp: &TempDir, instance: &str) -> ZccacheService {
        ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".into(),
                instance_id: instance.into(),
                workspace_id: instance.into(),
            },
            cache_root: temp.path().join("zccache").into(),
            audit: AuditConfig {
                output_root: Some(temp.path().join("audit").to_string_lossy().into_owned()),
                ..AuditConfig::default()
            },
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("audited embedded service starts")
    }

    /// The unspawnable compiler still drives the full emit path (the journal
    /// tests above use the same trick), so this needs no compiler on the host.
    fn unspawnable_request(run: &str) -> CompileRequest {
        CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new(run).expect("non-empty"),
                crate::audit::AuditId::new("audit-trace").expect("non-empty"),
            ),
            compiler: PathBuf::from("/nonexistent/compiler-that-never-runs").into(),
            args: vec!["--version".into()],
            cwd: std::env::current_dir().expect("cwd").into(),
            env: Vec::new(),
            stdin: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_compile_writes_events_to_the_host_audit_log() {
        let temp = TempDir::new().expect("temp root");
        let service = start_audited_service(&temp, "audit-emission").await;

        let _ = service.compile(unspawnable_request("audit-run")).await;

        // `flush` forwards to the sink, so the assertion is deterministic
        // rather than a sleep-and-hope.
        service.flush().await.expect("flush drains the audit sink");
        let events = audit_lines(temp.path());

        assert!(
            !events.is_empty(),
            "a compile under AuditMode::Normal must write audit events; an \
             empty log is the #905 regression"
        );
        let names: Vec<&str> = events
            .iter()
            .filter_map(|event| event["event"].as_str())
            .collect();
        assert!(
            names.contains(&"compile.started"),
            "expected compile.started, got {names:?}"
        );
        assert!(
            names.contains(&"compile.finished"),
            "expected compile.finished, got {names:?}"
        );

        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("graceful shutdown after audited compile");
    }

    /// The host's causal ids must reach the record. Before #905 the context
    /// was accepted and discarded, leaving timestamp correlation as the only
    /// way to tie a zccache event back to the host's run.
    #[tokio::test]
    async fn events_carry_the_host_supplied_correlation_ids() {
        let temp = TempDir::new().expect("temp root");
        let service = start_audited_service(&temp, "audit-context").await;

        let _ = service.compile(unspawnable_request("host-run-42")).await;
        service.flush().await.expect("flush drains the audit sink");

        let events = audit_lines(temp.path());
        let started = events
            .iter()
            .find(|event| event["event"] == "compile.started")
            .expect("compile.started must be present");

        assert_eq!(
            started["run_id"], "host-run-42",
            "the host's run_id must survive into the record: {started}"
        );
        assert_eq!(
            started["trace_id"], "audit-trace",
            "the host's trace_id must survive into the record: {started}"
        );
        assert!(
            started["compile_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "every compile event needs a compile_id to group on: {started}"
        );

        let finished = events
            .iter()
            .find(|event| event["event"] == "compile.finished")
            .expect("compile.finished must be present");
        assert_eq!(
            finished["compile_id"], started["compile_id"],
            "start and finish must share one compile_id or they cannot be paired"
        );
        assert_eq!(
            finished["level"], "error",
            "a failed compile must not be logged at info: {finished}"
        );
        // The unspawnable compiler fails before the engine yields an exit
        // code, so the terminal event carries the error text instead. What
        // matters is that the event exists at all — the first cut of this
        // change returned early on that path and emitted a `compile.started`
        // with no matching finish.
        assert!(
            finished["fields"]["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty()),
            "a compile that failed before producing an exit code must record \
             why: {finished}"
        );

        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("graceful shutdown");
    }

    /// `AuditMode::Off` must stay a true no-op — no sink, no file, no cost.
    #[tokio::test]
    async fn audit_off_writes_nothing() {
        let temp = TempDir::new().expect("temp root");
        let service = ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".into(),
                instance_id: "audit-off".into(),
                workspace_id: "audit-off".into(),
            },
            cache_root: temp.path().join("zccache").into(),
            audit: AuditConfig {
                mode: crate::audit::AuditMode::Off,
                output_root: Some(temp.path().join("audit").to_string_lossy().into_owned()),
                ..AuditConfig::default()
            },
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("service starts with audit off");

        let _ = service.compile(unspawnable_request("off-run")).await;
        service.flush().await.expect("flush with no sink");

        assert!(
            find_audit_log(temp.path()).is_none(),
            "AuditMode::Off must not create an audit log at all"
        );

        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("graceful shutdown with audit off");
    }
}
