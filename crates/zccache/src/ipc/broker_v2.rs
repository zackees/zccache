//! v2 broker client wiring for zccache (slice 10 of upstream #488).
//!
//! First zccache surface that calls the v2 broker API. Wraps
//! [`running_process::broker::client_v2::connect`] with a zccache-typed
//! result that fits this crate's existing error idioms. Each subsequent
//! slice of the migration replaces one more v1 surface with a v2 path,
//! per the burndown queue in
//! [zackees/running-process#488](https://github.com/zackees/running-process/issues/488)
//! and the consumer-side tracker
//! [zackees/zccache#777](https://github.com/zackees/zccache/issues/777).
//!
//! This slice does **not** remove any v1 caller. v2 ships alongside v1
//! per the coexistence table in upstream #470; the migration is per-
//! surface and opt-in until every reference is replaced.
//!
//! **Status (issue #844):** [`connect_v2_broker`], [`adopt_v2_session`],
//! and [`into_backend_io_v2`] have NO production call sites — only the
//! in-module smoke tests exercise them. They are `#[doc(hidden)]` to
//! signal "work-in-progress public API; do not consume from outside
//! the burndown PRs." Remove the `#[doc(hidden)]` markers when the
//! first real consumer lands (env-gated opt-in `ZCCACHE_BROKER_V2=1`
//! is the planned first hook, mirroring `ZCCACHE_BROKER_CONNECT`).
//! Only [`probe_local_socket`] is wired in production today (the
//! `RUNNING_PROCESS_FAKE_BACKEND` seam in `broker.rs`).

use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;
use running_process::broker::client_v2::{self, BrokerV2Error, ClientSession};

/// Default deadline for the [`probe_local_socket`] liveness check.
///
/// This is a probe, not a usage connect — 250ms is generous for any
/// local-socket dial that is actually going to succeed. If it doesn't
/// answer within this window the seam is assumed unreachable and the
/// caller falls back to the direct-connect path (see `broker.rs`).
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Default deadline for the v2 broker Hello round-trip (`connect_v2_broker`,
/// `adopt_v2_session`, `into_backend_io_v2`).
///
/// Mirrors v1's 3-second budget for `AsyncBrokerSession::adopt`'s
/// `await_handoff_ready`. If `client_v2::connect`'s sync read on the
/// underlying `interprocess::Stream` hangs (upstream has no internal
/// read deadline; see issue #842 upstream-coordination), this bound
/// is what saves the caller.
const DEFAULT_V2_BROKER_TIMEOUT: Duration = Duration::from_secs(3);

/// Slice 11 of #782: probe an arbitrary local-socket endpoint for
/// reachability without going through any broker negotiation.
///
/// Used by zccache's `RUNNING_PROCESS_FAKE_BACKEND` seam (a test-only
/// shortcut that bypasses the broker entirely — see #380 upstream).
/// The v1 path imported `connect_local_socket` from
/// `running_process::broker::client`; v2 owns the same primitive
/// locally so the migration stops pulling v1 broker types for what is
/// really just a `Stream::connect` call.
///
/// Returns `Ok(())` when the endpoint is reachable, `Err(io::Error)`
/// otherwise. The opened stream is closed immediately — this is a
/// liveness probe, not a connection acquisition.
pub fn probe_local_socket(endpoint: &str) -> std::io::Result<()> {
    if endpoint.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "probe_local_socket: empty endpoint",
        ));
    }
    probe_local_socket_with_deadline(endpoint, DEFAULT_PROBE_TIMEOUT)
}

/// Same as [`probe_local_socket`] but with a caller-supplied deadline.
///
/// MUST be called from inside `tokio::task::spawn_blocking` if invoked
/// from an async context — the helper-thread bound prevents an infinite
/// hang but the *outer* call still occupies the calling thread until
/// the deadline elapses.
pub fn probe_local_socket_with_deadline(endpoint: &str, deadline: Duration) -> std::io::Result<()> {
    let endpoint = endpoint.to_owned();
    call_with_io_deadline("probe_local_socket", deadline, move || {
        // Aligned with upstream `running_process::broker::server::connection::
        // local_socket_name`: pass the endpoint string verbatim on both
        // platforms. The earlier `strip_prefix(r"\\.\pipe\")` on Windows was
        // a parallel implementation that happened to work today (since
        // `to_ns_name` accepts both prefixed and bare names) but would rot
        // if `interprocess` tightened `GenericNamespaced` parsing.
        #[cfg(windows)]
        let name = {
            use interprocess::local_socket::{GenericNamespaced, ToNsName};
            ToNsName::to_ns_name::<GenericNamespaced>(endpoint.as_str())?
        };

        #[cfg(unix)]
        let name = {
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            ToFsName::to_fs_name::<GenericFilePath>(endpoint.as_str())?
        };

        let stream = Stream::connect(name)?;
        drop(stream);
        Ok(())
    })
}

/// Async bridge for [`probe_local_socket`].
///
/// The underlying local-socket dial is synchronous and can block at the OS
/// layer. Async production callers should use this wrapper so that work is
/// charged to Tokio's blocking pool instead of an executor worker.
pub async fn probe_local_socket_async(endpoint: &str) -> std::io::Result<()> {
    probe_local_socket_with_deadline_async(endpoint, DEFAULT_PROBE_TIMEOUT).await
}

/// Async bridge for [`probe_local_socket_with_deadline`].
pub async fn probe_local_socket_with_deadline_async(
    endpoint: &str,
    deadline: Duration,
) -> std::io::Result<()> {
    let endpoint = endpoint.to_owned();
    tokio::task::spawn_blocking(move || probe_local_socket_with_deadline(&endpoint, deadline))
        .await
        .map_err(join_error_to_io)?
}

/// Run a blocking `io::Result<T>`-returning closure on a helper thread,
/// bounded by `deadline`. On deadline the helper thread is leaked
/// (there is no portable way to cancel a `Stream::connect` mid-call)
/// but the calling thread returns promptly with an `ErrorKind::TimedOut`.
///
/// Mirrors v1's `await_handoff_ready` pattern (`mpsc::channel` +
/// `thread::spawn` + `recv_timeout`) — local-socket streams have no
/// portable read deadline, so the helper-thread approach is the only
/// way to bound a sync dial / framed read.
fn call_with_io_deadline<T, F>(label: &'static str, deadline: Duration, f: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{label}: exceeded deadline of {deadline:?}"),
        )),
    }
}

/// Run a blocking closure returning `Result<T, BrokerV2Error>` on a
/// helper thread, bounded by `deadline`. On deadline returns
/// `BrokerV2Error::Io(ErrorKind::TimedOut)` (mirroring upstream's
/// `connect_with_deadline` shape after running-process#517 — same
/// classification: `from_brokerv2_error` returns `None`, caller routes
/// to direct-connect fallback). Synthesizing `Dial { socket_path:
/// "<deadline-exceeded>" }` here would break downstream substring
/// assertions that expect the real v2 broker namespace.
fn call_with_brokerv2_deadline<T, F>(deadline: Duration, f: F) -> Result<T, BrokerV2Error>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, BrokerV2Error> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => Err(BrokerV2Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("v2 broker call exceeded deadline of {deadline:?}"),
        ))),
    }
}

/// Dial the v2 broker for zccache and return the negotiated session.
///
/// `wanted_version` is the daemon version zccache wants — the upstream
/// Hello carries it as `wanted_version`. The stub v2 broker (slices 3c/3d
/// of #488) currently accepts any well-formed Hello and replies with a
/// `Negotiated` carrying the broker binary's own package version as
/// `daemon_version`, so this entry point gives zccache observable
/// evidence that the v2 path is wired without depending on a fully-built
/// production broker.
///
/// # WIP — no production consumer
///
/// `#[doc(hidden)]` per #844: only smoke tests call this. Remove the
/// marker when the first production caller lands.
///
/// # Deadline
///
/// Bounded by [`DEFAULT_V2_BROKER_TIMEOUT`]. On timeout returns
/// `BrokerV2Error::Dial { source: ErrorKind::TimedOut }` so the caller
/// routes through the direct-connect fallback. Upstream's
/// `client_v2::connect` itself has no internal read deadline (see
/// #842 upstream-coordination); this wrapper is the defense.
#[doc(hidden)]
pub fn connect_v2_broker(wanted_version: &str) -> Result<ClientSession, BrokerV2Error> {
    let wanted = wanted_version.to_owned();
    call_with_brokerv2_deadline(DEFAULT_V2_BROKER_TIMEOUT, move || {
        client_v2::connect("zccache", &wanted)
    })
}

/// Async bridge for [`connect_v2_broker`].
///
/// This keeps the v2 broker's synchronous Hello round-trip out of async
/// callers while preserving the existing typed `BrokerV2Error` surface.
#[doc(hidden)]
pub async fn connect_v2_broker_async(wanted_version: &str) -> Result<ClientSession, BrokerV2Error> {
    let wanted = wanted_version.to_owned();
    tokio::task::spawn_blocking(move || connect_v2_broker(&wanted))
        .await
        .map_err(join_error_to_brokerv2)?
}

/// Slice 13 of #782: v2 adopt path counterpart of v1's
/// `AsyncBrokerSession::adopt` / `OwnedBackendIo`.
///
/// Performs the v2 Hello round-trip and consumes the resulting
/// `ClientSession` into its raw `(Stream, Negotiated)` parts. zccache's
/// existing adopt flow can layer its `IpcConnection::from_*_stream`
/// helpers on top of the returned stream once subsequent slices wire
/// the call sites over. v1's adopt remains untouched until that
/// per-call-site migration lands.
///
/// `wanted_version` is the daemon version zccache wants (`Hello.wanted_version`).
/// Errors mirror `client_v2::connect` exactly — no zccache-typed
/// re-wrapping happens at this layer so callers can pattern-match on
/// the upstream typed variants.
///
/// # WIP — no production consumer
///
/// `#[doc(hidden)]` per #844: only smoke tests call this. Remove the
/// marker when the first production caller lands.
///
/// # Deadline
///
/// Bounded by [`DEFAULT_V2_BROKER_TIMEOUT`] via [`connect_v2_broker`].
#[doc(hidden)]
pub fn adopt_v2_session(
    wanted_version: &str,
) -> Result<
    (
        interprocess::local_socket::Stream,
        running_process::broker::protocol::Negotiated,
    ),
    BrokerV2Error,
> {
    let session = connect_v2_broker(wanted_version)?;
    Ok(session.into_inner())
}

/// Async bridge for [`adopt_v2_session`].
#[doc(hidden)]
pub async fn adopt_v2_session_async(
    wanted_version: &str,
) -> Result<
    (
        interprocess::local_socket::Stream,
        running_process::broker::protocol::Negotiated,
    ),
    BrokerV2Error,
> {
    let wanted = wanted_version.to_owned();
    tokio::task::spawn_blocking(move || adopt_v2_session(&wanted))
        .await
        .map_err(join_error_to_brokerv2)?
}

/// Slice 14 of #782: v2 counterpart of v1's
/// `AsyncBrokerSession::into_backend_io`.
///
/// Same shape as [`adopt_v2_session`] but drops the `Negotiated` reply
/// — for callers that only need the raw byte stream after the Hello
/// completes. Mirrors the v1 convenience overload so v2 call-site
/// rewrites are mechanical: every `into_backend_io` becomes
/// `into_backend_io_v2`.
///
/// # WIP — no production consumer
///
/// `#[doc(hidden)]` per #844: only smoke tests call this. Remove the
/// marker when the first production caller lands.
#[doc(hidden)]
pub fn into_backend_io_v2(
    wanted_version: &str,
) -> Result<interprocess::local_socket::Stream, BrokerV2Error> {
    let (stream, _negotiated) = adopt_v2_session(wanted_version)?;
    Ok(stream)
}

/// Async bridge for [`into_backend_io_v2`].
#[doc(hidden)]
pub async fn into_backend_io_v2_async(
    wanted_version: &str,
) -> Result<interprocess::local_socket::Stream, BrokerV2Error> {
    let wanted = wanted_version.to_owned();
    tokio::task::spawn_blocking(move || into_backend_io_v2(&wanted))
        .await
        .map_err(join_error_to_brokerv2)?
}

fn join_error_to_io(err: tokio::task::JoinError) -> std::io::Error {
    std::io::Error::other(format!("broker-v2 async bridge worker failed: {err}"))
}

fn join_error_to_brokerv2(err: tokio::task::JoinError) -> BrokerV2Error {
    BrokerV2Error::Io(join_error_to_io(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `call_with_io_deadline` returns `Err(TimedOut)` when the closure
    /// outlives the deadline.
    #[test]
    fn call_with_io_deadline_fires_on_slow_closure() {
        let result: std::io::Result<()> =
            call_with_io_deadline("test", Duration::from_millis(50), || {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            });
        let err = result.expect_err("slow closure must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("exceeded deadline"));
    }

    /// `call_with_io_deadline` returns the closure's `Ok` when fast.
    #[test]
    fn call_with_io_deadline_passes_through_fast_ok() {
        let result: std::io::Result<u32> =
            call_with_io_deadline("test", Duration::from_secs(5), || Ok(42));
        assert_eq!(result.expect("fast closure returns Ok"), 42);
    }

    /// Stress test: 100 consecutive `call_with_brokerv2_deadline` calls,
    /// each closure stalls for 60s while the deadline fires at 10ms.
    /// Total wall-clock should be much less than 100 × 10ms = 1s if the
    /// helper threads truly run independently (i.e. each call returns
    /// on its own thread's deadline, not serialized).
    ///
    /// Catches: the round-2 audit's "helper-thread leak amplification"
    /// concern — under a retry storm, threads must not deadlock on a
    /// shared mutex / pool, and the parent caller must not block while
    /// helper threads accumulate. If the helpers truly leaked
    /// indefinitely without harming the parent, the test still passes
    /// (correctness, not resource accounting) — pure leak detection
    /// requires fd/pid inspection which is platform-specific. This
    /// test pins the wall-clock contract; resource accounting is a
    /// follow-up (see ledger #842 round-2 finding #1).
    #[test]
    fn call_with_brokerv2_deadline_stress_repeated_timeouts() {
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let result: Result<(), BrokerV2Error> =
                call_with_brokerv2_deadline(Duration::from_millis(10), || {
                    std::thread::sleep(Duration::from_secs(60));
                    Ok(())
                });
            match result {
                Err(BrokerV2Error::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => {}
                other => panic!("expected Io(TimedOut), got: {other:?}"),
            }
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "100 repeated timeouts took {elapsed:?}; expected ~1s (5s budget)"
        );
    }

    /// `call_with_brokerv2_deadline` returns `BrokerV2Error::Io(TimedOut)`
    /// when the closure outlives the deadline — mirrors upstream
    /// `connect_with_deadline`'s shape. `BrokerRefusal::from_brokerv2_error`
    /// routes Io as transport (returns `None`) so callers still fall
    /// back to direct connect.
    #[test]
    fn call_with_brokerv2_deadline_fires_on_slow_closure() {
        let result: Result<(), BrokerV2Error> =
            call_with_brokerv2_deadline(Duration::from_millis(50), || {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            });
        match result.expect_err("slow closure must time out") {
            BrokerV2Error::Io(io) => {
                assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
                assert!(
                    io.to_string().contains("exceeded deadline"),
                    "io error message should self-document: {io}"
                );
            }
            other => panic!("expected BrokerV2Error::Io(TimedOut), got: {other:?}"),
        }
    }

    /// Smoke test: with no v2 broker running, `connect_v2_broker` returns
    /// the typed `BrokerV2Error::Dial` path. This is the same shape the
    /// rest of zccache will pattern-match on as more surfaces migrate.
    #[test]
    fn connect_v2_broker_no_broker_returns_dial_error() {
        let err = connect_v2_broker("0.0.0").expect_err("no broker => Dial error");
        match err {
            BrokerV2Error::Dial { socket_path, .. } => {
                // The v2 broker uses different endpoint encodings per OS:
                // Windows pipes name the consumer in the filename
                // (`rpb-v2-zccache-<key>`), while Unix sockets put every v2
                // broker file under `.rp-<uid>-broker-v2/` and use a
                // content-hashed `<hex>.sock`. The universal invariant is
                // that the path is routed through the v2 broker namespace.
                let v2_marker = if cfg!(windows) {
                    "rpb-v2-zccache-"
                } else {
                    "broker-v2"
                };
                assert!(
                    socket_path.contains(v2_marker),
                    "Dial socket_path should reference the v2 broker namespace \
                     (expected substring `{v2_marker}`), got: {socket_path}"
                );
            }
            // Sid lookup failure is acceptable in environments without
            // `/etc/machine-id` (CI containers, restricted launchd contexts).
            // Either path proves the v2 client surface is callable from
            // a downstream consumer — which is what this smoke test gates.
            BrokerV2Error::Sid(_) => {}
            other => panic!("expected BrokerV2Error::Dial or Sid, got: {other:?}"),
        }
    }

    /// Slice 13: `adopt_v2_session` propagates the typed
    /// `BrokerV2Error` paths from `client_v2::connect` directly —
    /// no extra wrapping. With no broker running, the result must be
    /// either `Dial` (transport) or `Sid` (machine-id missing in CI
    /// containers).
    #[test]
    fn adopt_v2_session_no_broker_returns_typed_error() {
        let err = adopt_v2_session("0.0.0").expect_err("no broker => error");
        match err {
            BrokerV2Error::Dial { .. } | BrokerV2Error::Sid(_) => {}
            other => panic!("expected Dial or Sid, got: {other:?}"),
        }
    }

    /// Slice 14: `into_backend_io_v2` returns the same typed error
    /// shape as the underlying `adopt_v2_session` — the convenience
    /// overload doesn't introduce its own error variants.
    #[test]
    fn into_backend_io_v2_no_broker_returns_typed_error() {
        let err = into_backend_io_v2("0.0.0").expect_err("no broker => error");
        match err {
            BrokerV2Error::Dial { .. } | BrokerV2Error::Sid(_) => {}
            other => panic!("expected Dial or Sid, got: {other:?}"),
        }
    }

    /// Slice 11: `probe_local_socket` returns `Err` when nothing is
    /// listening on the given endpoint. Same shape the v1
    /// `connect_local_socket` returned, but now owned by the v2 module
    /// — no more import from `running_process::broker::client`.
    #[test]
    fn probe_local_socket_no_listener_returns_err() {
        let endpoint = if cfg!(windows) {
            r"\\.\pipe\zccache-slice11-probe-no-listener"
        } else {
            "/tmp/zccache-slice11-probe-no-listener.sock"
        };
        let err = probe_local_socket(endpoint).expect_err("no listener => Err");
        assert!(
            !err.to_string().is_empty(),
            "io error should carry a message"
        );
    }
}
