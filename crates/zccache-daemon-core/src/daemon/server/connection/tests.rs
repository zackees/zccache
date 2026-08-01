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

/// The end-to-end shape criterion B of #1223 asks for: a compile that does
/// not finish promptly must push progress frames to the waiting client, and
/// the terminal response must still arrive afterwards.
#[tokio::test]
async fn a_slow_compile_pushes_progress_frames_then_the_terminal_response() {
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
        // Fast cadence supplied directly. Reading it from the environment
        // would race every other test in this binary.
        Some(std::time::Duration::from_millis(40)),
        &mut conn,
        &ResponseWire::BincodeV15,
        &server.state,
        slow_handler(std::time::Duration::from_millis(260)),
    )
    .await;

    let (response, _journal) = outcome.expect("handler completes rather than disconnecting");
    super::send_response_for_wire(&mut conn, &ResponseWire::BincodeV15, &response)
        .await
        .expect("terminal response is written");

    let (progress_frames, terminal) = client.await.expect("client task");
    assert!(
        progress_frames >= 1,
        "a compile outliving the heartbeat interval must push at least one \
         CompileProgress frame; got {progress_frames}"
    );
    assert_eq!(
        terminal,
        Response::Pong,
        "progress frames must not replace or corrupt the terminal response"
    );
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
        &ResponseWire::BincodeV15,
        &server.state,
        slow_handler(std::time::Duration::from_millis(120)),
    )
    .await;

    let (response, _journal) = outcome.expect("handler completes");
    super::send_response_for_wire(&mut conn, &ResponseWire::BincodeV15, &response)
        .await
        .expect("terminal response is written");

    let (progress_frames, terminal) = client.await.expect("client task");
    assert_eq!(
        progress_frames, 0,
        "a disabled interval must emit nothing at all"
    );
    assert_eq!(terminal, Response::Pong);
}
