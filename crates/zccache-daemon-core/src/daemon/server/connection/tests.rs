//! Coverage for the compile-progress heartbeat emitter (#1216 / #1223).
//!
//! `guarded_dispatch_with_progress` is the only code that puts a
//! `CompileProgress` frame onto a real connection, and it had no tests at
//! all — `connection/` had no test module. The gauge arithmetic in
//! `compile_progress.rs` and the client-side receive loop in
//! `wrap/ipc.rs` were both well covered, so the *ends* were tested and the
//! wire between them was not.
//!
//! These tests drive the real loop over a real `IpcConnection` pair and
//! assert the frames arrive, then that the terminal response still lands.

use super::*;
use crate::protocol::{Request, Response};

/// A handler future standing in for a compile that takes long enough to be
/// worth reporting on. Returns a terminal response the caller can identify.
async fn slow_handler(duration: std::time::Duration) -> (Response, Option<PendingJournalContext>) {
    tokio::time::sleep(duration).await;
    (Response::Pong, None)
}

#[test]
fn an_unset_interval_uses_the_default_cadence() {
    assert_eq!(
        compile_progress_interval_from(None),
        Some(COMPILE_PROGRESS_INTERVAL)
    );
}

#[test]
fn zero_disables_heartbeats_entirely() {
    assert_eq!(
        compile_progress_interval_from(Some("0")),
        None,
        "0 is the documented kill switch; it must not fall back to the default"
    );
}

#[test]
fn an_explicit_interval_is_honored_and_whitespace_tolerated() {
    assert_eq!(
        compile_progress_interval_from(Some("250")),
        Some(std::time::Duration::from_millis(250))
    );
    assert_eq!(
        compile_progress_interval_from(Some("  250  ")),
        Some(std::time::Duration::from_millis(250))
    );
}

#[test]
fn an_unparseable_interval_falls_back_rather_than_disabling() {
    // The dangerous failure mode is silently disabling heartbeats on a
    // typo, which would look exactly like a wedged daemon to the client.
    assert_eq!(
        compile_progress_interval_from(Some("soon")),
        Some(COMPILE_PROGRESS_INTERVAL)
    );
    assert_eq!(
        compile_progress_interval_from(Some("")),
        Some(COMPILE_PROGRESS_INTERVAL)
    );
}

/// Criterion B of #1223: saturating the production compile gate must produce
/// a truthful queued heartbeat on the request connection, then preserve the
/// terminal response after the permit becomes available.
///
/// This intentionally isolates the gate/connection contract without spawning
/// a compiler. Cache hits return before this gate, so the criterion here is
/// continued service on the original connection, not cached-result semantics.
#[tokio::test]
async fn a_saturated_compile_gate_reports_queued_then_delivers_the_terminal_response() {
    let temp = tempfile::tempdir().expect("temp cache root");
    let endpoint = crate::ipc::unique_test_endpoint();
    let mut server = super::super::tests::bind_isolated_server_at(&endpoint, temp.path());
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    Arc::get_mut(&mut server.state)
        .expect("test owns the only SharedState reference")
        .compile_concurrency = Some(Arc::clone(&semaphore));

    // Occupy the sole slot through the same helper used by compile_exec. The
    // request below must queue until its heartbeat reaches the client.
    let (held_gate, _) = super::super::compile_progress::acquire_compile_gate(
        server.state.compile_concurrency.as_ref(),
        &server.state.compile_queue,
    )
    .await;
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
    let release = tokio::spawn(async move {
        let did_observe_queued_progress =
            tokio::time::timeout(std::time::Duration::from_secs(5), queued_rx)
                .await
                .is_ok_and(|result| result.is_ok());
        drop(held_gate);
        did_observe_queued_progress
    });

    let client_endpoint = endpoint.clone();
    let client = tokio::spawn(async move {
        let mut client = crate::ipc::connect(&client_endpoint)
            .await
            .expect("client connects");
        client.send(&Request::Ping).await.expect("client sends");
        let mut queued_tx = Some(queued_tx);
        loop {
            match client.recv::<Response>().await.expect("client receives") {
                Some(Response::CompileProgress {
                    queue_position,
                    queue_depth,
                    in_flight,
                    phase,
                }) if queued_tx.is_some() => {
                    assert_eq!(queue_position, 0, "queued request is next in line");
                    assert_eq!(queue_depth, 1, "exactly one request is waiting");
                    assert_eq!(in_flight, 1, "the sole compile slot is occupied");
                    assert_eq!(phase, super::super::compile_progress::PHASE_QUEUED);
                    if let Some(tx) = queued_tx.take() {
                        tx.send(()).expect("release gate holder");
                    }
                }
                Some(Response::CompileProgress { .. }) => {}
                Some(terminal) => return terminal,
                None => panic!("connection closed before a terminal response"),
            }
        }
    });

    let mut conn = server.listener.accept().await.expect("server accepts");
    let _ = conn.recv::<Request>().await.expect("server reads request");

    let outcome = guarded_dispatch_with_progress_every(
        Some(std::time::Duration::from_millis(40)),
        &mut conn,
        &ResponseWire::ProstV16 {
            request_id: "progress-test".to_string(),
        },
        &server.state,
        async {
            let (_gate, _) = super::super::compile_progress::acquire_compile_gate(
                server.state.compile_concurrency.as_ref(),
                &server.state.compile_queue,
            )
            .await;
            (Response::Pong, None)
        },
    )
    .await;

    let (response, _journal) = outcome.expect("handler completes rather than disconnecting");
    super::send_response_for_wire(
        &mut conn,
        &ResponseWire::ProstV16 {
            request_id: "progress-test".to_string(),
        },
        &response,
    )
    .await
    .expect("terminal response is written");

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("client receives progress and terminal response before deadline")
        .expect("client task");
    assert!(
        release.await.expect("gate holder task"),
        "client must observe queued progress before the held slot is released"
    );
    assert_eq!(terminal, Response::Pong);
}

/// The documented kill switch has to actually switch off, and the compile
/// must still complete.
#[tokio::test]
async fn a_disabled_interval_pushes_no_frames_but_still_completes() {
    let temp = tempfile::tempdir().expect("temp cache root");
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = super::super::tests::bind_isolated_server_at(&endpoint, temp.path());

    let mut server = server;
    let client_endpoint = endpoint.clone();
    let client = tokio::spawn(async move {
        let mut client = crate::ipc::connect(&client_endpoint)
            .await
            .expect("client connects");
        client.send(&Request::Ping).await.expect("client sends");
        let mut progress_frames = 0_usize;
        loop {
            match client.recv::<Response>().await.expect("client receives") {
                Some(Response::CompileProgress { .. }) => progress_frames += 1,
                Some(terminal) => return (progress_frames, terminal),
                None => panic!("connection closed before a terminal response"),
            }
        }
    });

    let mut conn = server.listener.accept().await.expect("server accepts");
    let _ = conn.recv::<Request>().await.expect("server reads request");

    let outcome = guarded_dispatch_with_progress_every(
        None,
        &mut conn,
        &ResponseWire::ProstV16 {
            request_id: "disabled-progress-test".to_string(),
        },
        &server.state,
        slow_handler(std::time::Duration::from_millis(120)),
    )
    .await;

    let (response, _journal) = outcome.expect("handler completes");
    super::send_response_for_wire(
        &mut conn,
        &ResponseWire::ProstV16 {
            request_id: "disabled-progress-test".to_string(),
        },
        &response,
    )
    .await
    .expect("terminal response is written");

    let (progress_frames, terminal) = client.await.expect("client task");
    assert_eq!(
        progress_frames, 0,
        "a disabled interval must emit nothing at all"
    );
    assert_eq!(terminal, Response::Pong);
}
