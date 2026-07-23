//! Issue #967 / meta #968: a compile/link/exec handler must be abandoned
//! when the requesting client disconnects, so the daemon-owned compiler
//! child is reaped (`kill_on_drop`) and its compile-concurrency permit is
//! released — instead of the daemon parking inside `wait_with_output` on a
//! compile whose result can never be delivered (the amplifier behind the
//! #962 permit-starvation wedge).
//!
//! Moved out of `connection.rs`'s `disconnect_cancellation_tests` module
//! during the #1154 phase-0 split (`crates/CLAUDE.md` § File-size
//! discipline).

use super::super::connection::{guarded_dispatch, PendingJournalContext};
use super::super::*;
use std::time::Duration;

/// Accept one server connection and connect a client to it. Returns the
/// server-side [`IpcConnection`] and the platform client handle (kept so
/// the caller controls when the peer disconnects). The listener is dropped
/// after the handshake — established connections outlive it.
async fn connected_pair() -> (IpcConnection, impl Sized) {
    let endpoint = crate::ipc::unique_test_endpoint();
    let mut listener = crate::ipc::IpcListener::bind_async(&endpoint)
        .await
        .unwrap();
    let (server, client) = tokio::join!(listener.accept(), crate::ipc::connect(&endpoint));
    (server.unwrap(), client.unwrap())
}

#[tokio::test]
async fn guarded_dispatch_reports_client_gone_when_peer_drops() {
    crate::test_support::test_timeout(async {
        let (mut server, client) = connected_pair().await;

        // Handler that never finishes — stands in for a compile parked in
        // `child.wait_with_output()`. The guard must abandon it once the
        // client disconnects.
        let handler = std::future::pending::<(Response, Option<PendingJournalContext>)>();

        let dropper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(client); // client disconnects mid-request
        });

        let outcome = guarded_dispatch(&mut server, handler).await;
        assert!(
            outcome.is_none(),
            "handler must be cancelled (None) when the client disconnects"
        );
        dropper.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn guarded_dispatch_completes_when_handler_finishes_first() {
    crate::test_support::test_timeout(async {
        let (mut server, _client) = connected_pair().await;

        // A handler that resolves immediately must return its response even
        // though the disconnect watcher is also armed — the race is biased
        // toward the handler.
        let handler = async { (Response::Pong, None) };
        let outcome = guarded_dispatch(&mut server, handler).await;
        assert!(
            matches!(outcome, Some((Response::Pong, None))),
            "a handler that finishes before disconnect must return its response"
        );
    })
    .await;
}

#[tokio::test]
async fn wait_for_disconnect_pends_while_peer_alive_and_idle() {
    crate::test_support::test_timeout(async {
        let (mut server, _client) = connected_pair().await;

        // A live but idle peer (blocked awaiting the compile response, as a
        // real client is) must NOT be mistaken for a disconnect.
        let res =
            tokio::time::timeout(Duration::from_millis(150), server.wait_for_disconnect()).await;
        assert!(
            res.is_err(),
            "wait_for_disconnect resolved while the peer was still alive"
        );
    })
    .await;
}
