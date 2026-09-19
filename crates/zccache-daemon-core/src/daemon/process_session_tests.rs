use super::*;

#[tokio::test]
async fn compiler_output_has_exactly_one_capture_owner() {
    use kernal_api::{ProcessOutputChunk, ProcessOutputEvent};
    for streaming in [false, true] {
        let (tx, mut rx) = kernal_api::async_engine::channel(2);
        let sender = streaming.then_some(tx);
        let mut stdout_bytes = 0;
        let mut stderr_bytes = 0;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for chunk in [
            ProcessOutputChunk::Stdout(b"out".to_vec()),
            ProcessOutputChunk::Stderr(b"err".to_vec()),
        ] {
            let result = forward_compiler_session_event(
                ProcessOutputEvent::Chunk(chunk),
                &sender,
                &mut stdout_bytes,
                &mut stderr_bytes,
                &mut stdout,
                &mut stderr,
            )
            .await;
            assert!(matches!(result, CompilerSessionEvent::Progress));
        }
        assert_eq!((stdout_bytes, stderr_bytes), (3, 3));
        if streaming {
            assert!(stdout.is_empty() && stderr.is_empty());
            assert!(matches!(rx.try_recv().expect("stdout chunk"),
                crate::daemon::compile_output::RawOutputChunk::Stdout(bytes) if bytes == b"out"));
            assert!(matches!(rx.try_recv().expect("stderr chunk"),
                crate::daemon::compile_output::RawOutputChunk::Stderr(bytes) if bytes == b"err"));
            assert!(rx.try_recv().is_err());
        } else {
            assert_eq!(stdout, b"out");
            assert_eq!(stderr, b"err");
            assert!(rx.try_recv().is_err());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn semantic_compiler_early_stdin_close_preserves_diagnostics() {
    let input = vec![b'x'; 1024 * 1024];
    let builder = kernal_api::SpawnSpec::new("/bin/sh")
        .args(["-c", "exec 0<&-; printf 'rejected input\\n' >&2; exit 7"]);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async_builder_output_with_priority_stdin(
            builder,
            CompilePriority::Normal,
            Some(&input),
            None,
            "early stdin close fixture".to_string(),
        ),
    )
    .await
    .expect("early stdin close must not stall")
    .expect("compiler status must survive a failed stdin write");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stderr, b"rejected input\n");
    assert!(output.stdout.is_empty());
}

#[tokio::test]
async fn semantic_termination_returns_a_reaped_child_status() {
    #[cfg(unix)]
    let builder = kernal_api::SpawnSpec::new("/bin/sh").args(["-c", "exec sleep 30"]);
    #[cfg(windows)]
    let builder = kernal_api::SpawnSpec::new("powershell").args([
        "-NoProfile",
        "-Command",
        "Start-Sleep -Seconds 30",
    ]);
    let session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        builder.spawn_session(kernal_api::ProcessSessionOptions {
            kill_on_drop: true,
            ..Default::default()
        }),
    )
    .await
    .expect("fixture startup deadline")
    .expect("fixture starts");
    let status = super::session::terminate_session(&session)
        .await
        .expect("termination confirms reaping");
    assert!(!status.success());
    let polled = tokio::time::timeout(std::time::Duration::from_secs(2), session.poll())
        .await
        .expect("poll deadline")
        .expect("poll completed");
    assert_eq!(
        polled.map(kernal_api::ProcessSessionExit::exit_status),
        Some(status),
        "termination returned before reaping was recorded"
    );
}

#[cfg(unix)]
#[test]
fn canonical_spawn_waits_for_materialization_to_release() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let marker = dir.path().join("started");
    let child_marker = marker.clone();
    let exclusive = crate::daemon::spawn_exclusion::materialize_exclusive();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime");
        let builder = kernal_api::SpawnSpec::new("/bin/sh")
            .args(["-c", "printf started > \"$1\"", "fixture"])
            .arg(child_marker);
        started_tx.send(()).expect("parent listening");
        let result = runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                async_builder_output_with_priority_and_post_exit_grace(
                    builder,
                    CompilePriority::Normal,
                    None,
                ),
            )
            .await
        });
        let _ = done_tx.send(result);
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("worker started");
    let completed_while_locked = done_rx.recv_timeout(std::time::Duration::from_millis(200));
    let marker_while_locked = marker.exists();
    // Release before asserting so an assertion cannot strand the spawn thread.
    drop(exclusive);
    let waited_for_release = matches!(
        &completed_while_locked,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );
    // If the regression let the child finish early, keep that result instead
    // of waiting for a second message that will never arrive.
    let result = match completed_while_locked {
        Ok(result) => Ok(result),
        Err(_) => done_rx.recv_timeout(std::time::Duration::from_secs(12)),
    };
    if result.is_ok() {
        worker.join().expect("fixture worker");
    }
    let output = result
        .expect("worker completion")
        .expect("fixture operation deadline")
        .expect("fixture spawn");
    assert!(
        waited_for_release,
        "child completed inside materialization's exclusive section"
    );
    assert!(
        !marker_while_locked,
        "child ran inside materialization's exclusive section"
    );
    assert!(output.status.success());
    assert_eq!(std::fs::read(marker).expect("child marker"), b"started");
}

#[cfg(unix)]
#[tokio::test]
async fn semantic_compiler_drains_output_while_feeding_large_stdin() {
    let input = vec![b'x'; 1024 * 1024];
    let builder = kernal_api::SpawnSpec::new("/bin/sh")
        .args(["-c", "dd if=/dev/zero bs=4096 count=64 2>/dev/null; cat"]);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        async_builder_output_with_priority_stdin(
            builder,
            CompilePriority::Normal,
            Some(&input),
            None,
            "duplex compiler fixture".to_string(),
        ),
    )
    .await
    .expect("output-before-input must not deadlock stdin feeding")
    .expect("duplex fixture must run");
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 256 * 1024 + input.len());
    assert!(output.stdout[..256 * 1024].iter().all(|byte| *byte == 0));
    assert_eq!(&output.stdout[256 * 1024..], input.as_slice());
    assert!(output.stderr.is_empty());
}
use super::session::{forward_compiler_session_event, CompilerSessionEvent};

#[cfg(unix)]
#[tokio::test]
async fn semantic_discovery_timeout_preserves_output_and_nonzero_status() {
    let builder = kernal_api::SpawnSpec::new("/bin/sh")
        .args(["-c", "printf probe-out; printf probe-err >&2; exit 7"]);
    let output = async_builder_output_with_priority_timeout(
        builder,
        CompilePriority::Normal,
        std::time::Duration::from_secs(10),
        "discovery contract fixture".to_string(),
    )
    .await
    .expect("nonzero discovery exit is still a captured result");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"probe-out");
    assert_eq!(output.stderr, b"probe-err");
}

#[cfg(unix)]
#[tokio::test]
async fn semantic_discovery_deadline_returns_timed_out() {
    let builder = kernal_api::SpawnSpec::new("/bin/sh").args(["-c", "exec sleep 30"]);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async_builder_output_with_priority_timeout(
            builder,
            CompilePriority::Normal,
            std::time::Duration::from_millis(100),
            "discovery deadline fixture".to_string(),
        ),
    )
    .await
    .expect("discovery deadline must bound the caller's wait")
    .expect_err("sleeping discovery must time out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    // Native session cancellation/cleanup is covered in the producer tests;
    // this consumer test asserts only the discovery-facing error contract.
}
