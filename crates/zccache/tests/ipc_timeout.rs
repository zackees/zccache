//! Integration tests for the per-recv timeout API on `IpcConnection` /
//! `IpcClientConnection`. Validates four invariants:
//!
//! 1. The default is **unbounded** — preserves today's behavior for
//!    callers that don't opt in (server-side `handle_connection`,
//!    `zccache-download-client`).
//! 2. `set_recv_timeout` is honored — once the caller opts in, a
//!    `recv` that doesn't complete in time returns `Err(Timeout)`.
//! 3. `recv_with_timeout` works independent of any stored default.
//! 4. **Peer death surfaces as `Io` / `ConnectionClosed`, not
//!    `Timeout`.** The OS closes the socket / pipe when a process dies;
//!    the timeout is reserved for the rare "alive but stuck" failure
//!    mode.

use std::time::{Duration, Instant};

use zccache::ipc::{connect, unique_test_endpoint, IpcError, IpcListener};
use zccache::protocol::{Request, Response};

/// Default-unbounded: no `set_recv_timeout` call → `recv` waits as long
/// as needed. Listener delays 200ms before responding. With the new
/// `Option<Duration> = None` default, the client should still complete
/// successfully, matching today's behavior. This is the regression
/// guard for server-side and download-client callers that share the
/// IPC types and rely on unbounded reads.
#[tokio::test]
async fn recv_unbounded_by_default() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<Request> = conn.recv().await.unwrap();
        assert_eq!(msg, Some(Request::Ping));
        tokio::time::sleep(Duration::from_millis(200)).await;
        conn.send(&Response::Pong).await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    assert!(
        client.recv_timeout().is_none(),
        "fresh connect must default to unbounded (None)"
    );
    client.send(&Request::Ping).await.unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));
    server.await.unwrap();
}

/// `set_recv_timeout` opt-in: listener accepts but never responds.
/// Client opts into a 200ms deadline. `recv` returns `Err(Timeout(_))`
/// within a generous 2s wall-clock budget.
#[tokio::test]
async fn recv_honors_set_recv_timeout() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let _conn = listener.accept().await.unwrap();
        // Hold the connection alive without sending anything. Drop is
        // delayed long enough that the client's timeout fires first.
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let mut client = connect(&endpoint).await.unwrap();
    client.set_recv_timeout(Duration::from_millis(200));
    assert_eq!(client.recv_timeout(), Some(Duration::from_millis(200)));
    client.send(&Request::Ping).await.unwrap();

    let start = Instant::now();
    let result: Result<Option<Response>, _> = client.recv().await;
    let elapsed = start.elapsed();

    match result {
        Err(IpcError::Timeout(d)) => {
            assert_eq!(
                d,
                Duration::from_millis(200),
                "Timeout must carry the configured deadline"
            );
        }
        other => panic!("expected Err(Timeout(200ms)), got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "recv timeout firing took {elapsed:?}; expected <2s"
    );

    drop(client);
    server.await.unwrap();
}

/// Per-call override: `recv_with_timeout(t)` works even when no default
/// was set via `set_recv_timeout`. Proves the override is independent
/// of the stored field.
#[tokio::test]
async fn recv_with_timeout_works_without_default() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let _conn = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let mut client = connect(&endpoint).await.unwrap();
    assert!(
        client.recv_timeout().is_none(),
        "must start without default"
    );
    client.send(&Request::Ping).await.unwrap();

    let start = Instant::now();
    let result: Result<Option<Response>, _> =
        client.recv_with_timeout(Duration::from_millis(200)).await;
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(IpcError::Timeout(_))),
        "expected Err(Timeout(_)), got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "recv_with_timeout firing took {elapsed:?}; expected <2s"
    );

    drop(client);
    server.await.unwrap();
}

/// Regression guard: a normal-latency response within the configured
/// window must NOT trip the timeout. Without this guard, a tight-loop
/// race where the timer fires between read syscalls could surface a
/// spurious `Timeout` even when the peer responded in time.
#[tokio::test]
async fn recv_does_not_timeout_on_normal_response() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<Request> = conn.recv().await.unwrap();
        assert_eq!(msg, Some(Request::Ping));
        tokio::time::sleep(Duration::from_millis(50)).await;
        conn.send(&Response::Pong).await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    client.set_recv_timeout(Duration::from_secs(1));
    client.send(&Request::Ping).await.unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));
    server.await.unwrap();
}

/// Peer death is OS-detected: when the listener drops mid-recv, the
/// kernel closes the socket / pipe. The client's recv surfaces this as
/// `Ok(None)` (clean close) or `Err(Io(_))` / `Err(ConnectionClosed)` —
/// **never** `Err(Timeout(_))`. Documents the invariant the user asked
/// about: timeouts protect against "alive but stuck" only; death is the
/// OS's job.
#[tokio::test]
async fn recv_reports_io_err_when_peer_dies() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        // Accept then immediately drop the connection, simulating a
        // peer that died mid-conversation.
        let _conn = listener.accept().await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    client.set_recv_timeout(Duration::from_secs(5));

    // Give the server task time to drop its connection.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let result: Result<Option<Response>, _> = client.recv().await;
    match result {
        Ok(None) => {}                        // Clean close before timeout — acceptable.
        Err(IpcError::Io(_)) => {}            // BrokenPipe / similar — acceptable.
        Err(IpcError::ConnectionClosed) => {} // Mid-frame close — acceptable.
        Err(IpcError::Timeout(_)) => panic!(
            "peer-death must NOT surface as Timeout — that would mean we treated \
             OS-level connection close as a stuck-but-alive failure"
        ),
        other => panic!("unexpected recv result: {other:?}"),
    }

    server.await.unwrap();
}
