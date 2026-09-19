//! Broker-mediated connect lane for the daemon client path.
//!
//! Wires the frozen `AsyncBrokerSession::adopt` one-call recipe
//! (re-exported through `protocol_v2::client_compat` per zccache#782
//! slices 25-A/25; underlying impl from running-process#433/#435)
//! in front of zccache's direct IPC connect. `adopt` runs the Hello negotiation (service_name
//! `"zccache"`, protocol min/max = 1, client_lib_name `"running-process"`,
//! wanted_version = the zccache daemon version) on a blocking worker and hands
//! back the broker-selected backend endpoint. Lane selection precedence:
//!
//! 1. `RUNNING_PROCESS_DISABLE=1` — the canonical upstream escape hatch.
//!    The broker lane (including the fake-backend test seam) is bypassed
//!    entirely and the pre-adoption direct connect is used.
//! 2. `RUNNING_PROCESS_FAKE_BACKEND=<endpoint>` — upstream TEST-ONLY seam:
//!    `connect_to_backend` dials the endpoint directly, skipping broker
//!    discovery and Hello negotiation. Never set in production.
//! 3. Default — direct connect, byte-for-byte the pre-adoption behavior.
//!
//! #1002: the production `ZCCACHE_BROKER_CONNECT=1` opt-in that gated a
//! broker-negotiation connect lane has been **retired**. It was never enabled
//! in production (and therefore untested there), and version-aware endpoint
//! naming (#1004) is now zccache's standalone conflict-prevention mechanism, so
//! the lane is no longer needed. The daemon still **publishes** its manifest +
//! BackendHandle identity for discovery tooling; only the client-side broker
//! *connect* path is gone. The fake-backend TEST seam is retained.
//!
//! On the production broker lane (Unix), the live negotiated socket handed
//! back by `AsyncBrokerSession::into_backend_io` is **adopted directly** as
//! the data connection — no re-dial. The adopted socket is byte-identical to a
//! fresh `connect()` stream (the broker hands back a `backend_pipe` the client
//! dials itself), so recv timeouts and the prost/FrameV1 wire lanes keep
//! working unchanged. On Windows `into_backend_io` is unsupported (the
//! `OwnedHandle` handoff is deferred, running-process #720), so the resolved
//! endpoint is re-dialed with zccache's own transport (resolve-and-drop). The
//! TEST-ONLY fake-backend seam also stays resolve-and-drop: it dials a raw
//! socket with no Hello negotiation, so there is no live session to adopt.

use kernal_api::broker_client::{BackendRoute, BrokerClientError, RefusalKind};
// The raw-socket reachability probe used by the `RUNNING_PROCESS_FAKE_BACKEND`
// seam lives in `ipc::probe` (extracted from the removed `broker_v2` module,
// issue #1001).
use super::probe::probe_local_socket;

use super::error::IpcError;
use super::{connect, running_process_disabled, ClientConnection};

/// Upstream TEST-ONLY seam: a non-empty value short-circuits the broker
/// negotiation and dials the given running-process endpoint directly.
///
/// The name is the facade's frozen seam constant
/// (`kernal_api::broker_client::FAKE_BACKEND_ENV`). The canonical
/// `RUNNING_PROCESS_DISABLE=1` hatch takes precedence: a disabled broker
/// ignores the fake seam too. Never set this in production.
pub const RUNNING_PROCESS_FAKE_BACKEND_ENV: &str = kernal_api::broker_client::FAKE_BACKEND_ENV;

/// How [`connect_daemon`] reached the daemon endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonConnectRoute {
    /// Existing direct connect — the default and every fallback/escape-hatch
    /// path.
    Direct,
    /// Endpoint resolved through `connect_to_backend`.
    Broker {
        /// Route reported by the running-process broker client.
        route: BackendRoute,
        /// Resolved endpoint (zccache connect form) the data connection used.
        endpoint: String,
    },
}

/// Connect to the zccache daemon, honoring the broker lane selection
/// described in the module docs.
///
/// Drop-in replacement for [`super::connect`] on the daemon client path:
/// returns the same platform connection type and never fails for a reason
/// the direct connect would not also fail for.
pub async fn connect_daemon(endpoint: &str) -> Result<ClientConnection, IpcError> {
    connect_daemon_with_route(endpoint)
        .await
        .map(|(conn, _route)| conn)
}

/// Like [`connect_daemon`], also reporting which route was taken.
pub async fn connect_daemon_with_route(
    endpoint: &str,
) -> Result<(ClientConnection, DaemonConnectRoute), IpcError> {
    if running_process_disabled() || !broker_lane_requested() {
        let conn = connect(endpoint).await?;
        return Ok((conn, DaemonConnectRoute::Direct));
    }

    // Fake-backend test seam keeps resolve-and-drop + re-dial: it dials a raw
    // socket with no Hello negotiation, so there is no live session to adopt.
    // This is the only remaining lane activator after #1002 retired the
    // production broker connect opt-in.
    if let Some(seam_endpoint) = fake_backend_endpoint_from_env() {
        if let Some((resolved, route)) = resolve_fake_backend_seam_async(seam_endpoint).await {
            if let Some(result) = redial_resolved(route, resolved).await {
                return Ok(result);
            }
        }
    }

    let conn = connect(endpoint).await?;
    Ok((conn, DaemonConnectRoute::Direct))
}

/// Is the broker lane actually governing connections for this process?
///
/// True when the broker lane is requested *and* not disabled by the
/// `RUNNING_PROCESS_DISABLE=1` escape hatch — i.e. the same precedence
/// [`connect_daemon_with_route`] applies before resolving a backend. Callers
/// that need to pick the version-checked FrameV1 wire when the broker carries
/// the connection (issue #720 Phase 1) consult this.
pub(crate) fn broker_lane_active() -> bool {
    !running_process_disabled() && broker_lane_requested()
}

/// Is the broker lane requested for this process?
///
/// #1002: the production `ZCCACHE_BROKER_CONNECT=1` opt-in has been retired —
/// version-aware endpoint naming (#1004) is now zccache's standalone
/// conflict-prevention mechanism, so the broker-negotiation connect lane it
/// gated (never enabled in production, untested there) is gone. Only the
/// upstream fake-backend TEST seam can still request the lane. The
/// `RUNNING_PROCESS_DISABLE=1` precedence check happens in
/// [`connect_daemon_with_route`] before this is consulted. Manifest /
/// BackendHandle publication is unaffected — the daemon still publishes its
/// identity for discovery tooling.
fn broker_lane_requested() -> bool {
    std::env::var_os(RUNNING_PROCESS_FAKE_BACKEND_ENV).is_some_and(|value| !value.is_empty())
}

/// Re-dial a broker-resolved endpoint with zccache's own transport, reporting
/// the broker route on success. Returns `None` if the endpoint is unreachable.
async fn redial_resolved(
    route: BackendRoute,
    resolved: String,
) -> Option<(ClientConnection, DaemonConnectRoute)> {
    match connect(&resolved).await {
        Ok(conn) => Some((
            conn,
            DaemonConnectRoute::Broker {
                route,
                endpoint: resolved,
            },
        )),
        Err(err) => {
            tracing::debug!(
                resolved_endpoint = %resolved,
                error = %err,
                "broker-resolved endpoint unreachable; falling back to direct connect"
            );
            None
        }
    }
}

/// Dial the upstream TEST-ONLY fake-backend seam on a worker thread.
///
/// `connect_local_socket` is a blocking dial, so it runs on `spawn_blocking`.
async fn resolve_fake_backend_seam_async(seam_endpoint: String) -> Option<(String, BackendRoute)> {
    tokio::task::spawn_blocking(move || resolve_fake_backend_seam(&seam_endpoint))
        .await
        .unwrap_or_else(|err| {
            tracing::debug!(error = %err, "fake-backend seam task failed; using direct connect");
            None
        })
}

/// Dial the upstream TEST-ONLY fake-backend seam endpoint directly.
fn resolve_fake_backend_seam(seam_endpoint: &str) -> Option<(String, BackendRoute)> {
    match probe_local_socket(seam_endpoint) {
        Ok(()) => Some((to_zccache_endpoint(seam_endpoint), BackendRoute::HelloSkip)),
        Err(err) => {
            tracing::warn!(
                endpoint = %seam_endpoint,
                error = %err,
                "RUNNING_PROCESS_FAKE_BACKEND endpoint unreachable; using direct connect"
            );
            None
        }
    }
}

/// Typed classification of a broker refusal, surfaced for diagnostics and for
/// callers that want to branch on *why* the broker declined rather than always
/// falling back silently. zccache always falls back to the direct connect on a
/// refusal (the broker lane must never make a working build fail), but the
/// classification is logged and exposed for the diagnostics command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerRefusal {
    /// The requested daemon version is below the registered `min_version`.
    VersionUnsupported,
    /// The requested daemon version is explicitly blocked (e.g. yanked).
    VersionBlocked,
    /// No `zccache.servicedef` is installed for this broker.
    ServiceUnknown,
    /// The broker is rate-limiting this peer. `retry_after_ms` is the
    /// broker-supplied backoff hint (0 = no hint). Callers can honor it
    /// directly: `Duration::from_millis(retry_after_ms)`.
    RateLimited { retry_after_ms: u64 },
    /// The broker is shutting down.
    ShuttingDown,
    /// Any other refusal code the broker reported.
    Other,
}

impl BrokerRefusal {
    /// Classify a broker client error as a `BrokerRefusal`.
    ///
    /// Returns `Some(BrokerRefusal)` only when the broker explicitly declined
    /// the Hello — disabled, dial, IO, and protocol failures return `None`
    /// (the caller falls back to the direct connect path). The typed
    /// granularity is what `soldr doctor` and the connect-route logs depend on
    /// (rate-limit / version-pin / shutdown all surface distinctly instead of
    /// collapsing to `Other`).
    ///
    /// Unknown codes (including a future broker shipping a wire code this
    /// client predates) fall through to `Other`. `retry_after_ms` is threaded
    /// through to `RateLimited`; callers can honor it directly via
    /// `Duration::from_millis(retry_after_ms)`.
    #[must_use]
    pub fn classify(err: &BrokerClientError) -> Option<Self> {
        let refusal = err.refusal()?;
        Some(match refusal.kind() {
            RefusalKind::VersionUnsupported => Self::VersionUnsupported,
            RefusalKind::VersionBlocked => Self::VersionBlocked,
            RefusalKind::ServiceUnknown => Self::ServiceUnknown,
            RefusalKind::RateLimited => Self::RateLimited {
                retry_after_ms: refusal.retry_after_ms(),
            },
            RefusalKind::ShuttingDown => Self::ShuttingDown,
            RefusalKind::Other(_) => Self::Other,
        })
    }
}

/// Read the fake-backend seam, honoring the disable-hatch precedence.
fn fake_backend_endpoint_from_env() -> Option<String> {
    let value = std::env::var_os(RUNNING_PROCESS_FAKE_BACKEND_ENV)?;
    let value = value.to_string_lossy();
    if value.is_empty() {
        return None;
    }
    Some(value.into_owned())
}

/// Translate a running-process backend endpoint into zccache connect form.
///
/// running-process uses bare pipe names on Windows (`interprocess`
/// namespaced names) while zccache's transport expects the full
/// `\\.\pipe\` form. Unix socket paths are shared verbatim.
fn to_zccache_endpoint(endpoint: &str) -> String {
    crate::platform::ipc::Endpoint::from_running_process(endpoint).to_string()
}

/// Strip a zccache endpoint down to the running-process local-socket form.
///
/// Inverse of the private `to_zccache_endpoint`; used by tests and tooling
/// that hand
/// a zccache endpoint to the upstream fake-backend seam.
#[must_use]
pub fn to_running_process_endpoint(endpoint: &str) -> String {
    crate::platform::ipc::Endpoint::from_native(endpoint).to_running_process()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvVarGuard;
    use crate::{unique_test_endpoint, IpcListener, RUNNING_PROCESS_DISABLE_ENV};
    use kernal_api::broker_client::RefusalCode;
    use zccache_protocol::{Request, Response};

    /// Spawn a ping server that accepts connections until it has answered
    /// one Ping. The accept loop is unbounded on purpose: the broker
    /// lane's resolution dial closes immediately, and on loaded Linux
    /// runners it can surface as extra reset connections, so budgeting a
    /// fixed number of accepts is racy — the listener must stay alive
    /// until the data connection's Ping is answered (seen as ECONNRESET
    /// in CI Integration runs otherwise).
    fn spawn_ping_server(listener: IpcListener) -> tokio::task::JoinHandle<usize> {
        spawn_counting_ping_server(listener, 1)
    }

    async fn ping_roundtrip(conn: &mut super::ClientConnection) {
        conn.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = conn.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));
    }

    #[tokio::test]
    async fn default_route_is_direct() {
        let _env = EnvVarGuard::unset_all(&[
            RUNNING_PROCESS_DISABLE_ENV,
            RUNNING_PROCESS_FAKE_BACKEND_ENV,
        ]);

        let endpoint = unique_test_endpoint();
        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_ping_server(listener);

        let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
        assert_eq!(route, DaemonConnectRoute::Direct);
        ping_roundtrip(&mut conn).await;

        assert_eq!(server.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn fake_backend_seam_routes_through_connect_to_backend() {
        let endpoint = unique_test_endpoint();
        let _env = EnvVarGuard::set_all(&[
            (RUNNING_PROCESS_DISABLE_ENV, None),
            (
                RUNNING_PROCESS_FAKE_BACKEND_ENV,
                Some(to_running_process_endpoint(&endpoint)),
            ),
        ]);

        let listener = IpcListener::bind(&endpoint).unwrap();
        // The server sees the connect_to_backend resolution dial (dropped)
        // before the zccache data connection.
        let server = spawn_ping_server(listener);

        let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
        match route {
            DaemonConnectRoute::Broker {
                route: BackendRoute::HelloSkip,
                endpoint: resolved,
            } => assert_eq!(resolved, endpoint),
            other => panic!("expected broker HelloSkip route, got {other:?}"),
        }
        ping_roundtrip(&mut conn).await;

        assert_eq!(server.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn disable_env_bypasses_broker_lane_entirely() {
        // The seam points at a guaranteed-dead endpoint. If the disable
        // hatch failed to take precedence, the broker lane would dial it;
        // with the hatch honored, the direct connect must succeed.
        let endpoint = unique_test_endpoint();
        let _env = EnvVarGuard::set_all(&[
            (RUNNING_PROCESS_DISABLE_ENV, Some("1".to_string())),
            (
                RUNNING_PROCESS_FAKE_BACKEND_ENV,
                Some(to_running_process_endpoint(&unique_test_endpoint())),
            ),
        ]);

        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_ping_server(listener);

        let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
        assert_eq!(route, DaemonConnectRoute::Direct);
        ping_roundtrip(&mut conn).await;

        assert_eq!(server.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn broker_failure_falls_back_to_direct_connect() {
        // Seam points at a dead endpoint: connect_to_backend errors, and
        // the wrapper must fall back to the direct connect.
        let endpoint = unique_test_endpoint();
        let _env = EnvVarGuard::set_all(&[
            (RUNNING_PROCESS_DISABLE_ENV, None),
            (
                RUNNING_PROCESS_FAKE_BACKEND_ENV,
                Some(to_running_process_endpoint(&unique_test_endpoint())),
            ),
        ]);

        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_ping_server(listener);

        let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
        assert_eq!(route, DaemonConnectRoute::Direct);
        ping_roundtrip(&mut conn).await;

        assert_eq!(server.await.unwrap(), 1);
    }

    /// Sorted-percentile helper matching the convention in
    /// `tests/perf/daemon_perf_test.rs`.
    fn percentile_ms(sorted: &[f64], pct: f64) -> f64 {
        let idx = ((sorted.len() as f64 * pct) as usize).min(sorted.len() - 1);
        sorted[idx]
    }

    /// Ping server for the latency evidence test: keeps accepting until it
    /// has answered `pings` Ping requests, tolerating the broker lane's
    /// dropped resolution dials in between.
    fn spawn_counting_ping_server(
        mut listener: IpcListener,
        pings: usize,
    ) -> tokio::task::JoinHandle<usize> {
        tokio::spawn(async move {
            let mut answered = 0;
            while answered < pings {
                let Ok(mut conn) = listener.accept().await else {
                    break;
                };
                match conn.recv::<Request>().await {
                    Ok(Some(Request::Ping)) => {
                        conn.send(&Response::Pong).await.unwrap();
                        answered += 1;
                    }
                    Ok(None) | Err(_) => continue,
                    Ok(Some(other)) => panic!("unexpected request: {other:?}"),
                }
            }
            answered
        })
    }

    /// Measure connect + Ping/Pong round-trip latency for `samples`
    /// iterations against a fresh listener, returning per-iteration
    /// milliseconds. `expect_broker` asserts the route per iteration.
    async fn measure_connect_roundtrip_ms(samples: usize, expect_broker: bool) -> Vec<f64> {
        let endpoint = unique_test_endpoint();
        if expect_broker {
            // Re-point the seam at this run's endpoint (the caller holds
            // the env lock for the whole measurement).
            std::env::set_var(
                RUNNING_PROCESS_FAKE_BACKEND_ENV,
                to_running_process_endpoint(&endpoint),
            );
        }
        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_counting_ping_server(listener, samples);

        let mut samples_ms = Vec::with_capacity(samples);
        for _ in 0..samples {
            let start = std::time::Instant::now();
            let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
            ping_roundtrip(&mut conn).await;
            samples_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            drop(conn);
            match (&route, expect_broker) {
                (DaemonConnectRoute::Broker { .. }, true) => {}
                (DaemonConnectRoute::Direct, false) => {}
                (other, _) => panic!("unexpected route {other:?} (expect_broker={expect_broker})"),
            }
        }
        assert_eq!(server.await.unwrap(), samples);
        samples_ms
    }

    /// Hot-path latency evidence for running-process#383 item 2: p50/p99 of
    /// connect + Ping round-trip over the direct lane vs the broker lane
    /// (fake-backend seam, which exercises the full lane wiring: env
    /// dispatch, spawn_blocking resolution, resolution dial, endpoint
    /// translation, re-dial).
    ///
    /// Sanctioned perf shape per PERF.md: a `#[test]` with a generous
    /// absolute Duration budget; the printed numbers are the evidence
    /// recorded in the adoption PR.
    #[tokio::test]
    async fn broker_lane_connect_latency_p50_p99() {
        const WARMUP: usize = 5;
        const SAMPLES: usize = 100;

        let _env = EnvVarGuard::set_all(&[
            (RUNNING_PROCESS_DISABLE_ENV, None),
            (RUNNING_PROCESS_FAKE_BACKEND_ENV, None),
        ]);

        // Warmup both lanes (first-connect costs: pipe namespace setup,
        // thread-pool spinup for spawn_blocking).
        measure_connect_roundtrip_ms(WARMUP, false).await;
        let mut direct = measure_connect_roundtrip_ms(SAMPLES, false).await;

        measure_connect_roundtrip_ms(WARMUP, true).await;
        let mut broker = measure_connect_roundtrip_ms(SAMPLES, true).await;
        std::env::remove_var(RUNNING_PROCESS_FAKE_BACKEND_ENV);

        direct.sort_by(|a, b| a.partial_cmp(b).unwrap());
        broker.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let report = |label: &str, sorted: &[f64]| {
            let p50 = percentile_ms(sorted, 0.50);
            let p99 = percentile_ms(sorted, 0.99);
            println!(
                "  {label:<28} p50={p50:>8.3}ms  p99={p99:>8.3}ms  min={:>8.3}ms  max={:>8.3}ms  (n={})",
                sorted[0],
                sorted[sorted.len() - 1],
                sorted.len()
            );
            (p50, p99)
        };
        println!("broker lane connect+ping latency (running-process#383 evidence):");
        let (_direct_p50, direct_p99) = report("direct lane", &direct);
        let (_broker_p50, broker_p99) = report("broker lane (seam)", &broker);

        // Generous absolute budgets: local IPC connect + one round-trip
        // must stay well under a second even on loaded CI runners. These
        // exist to catch order-of-magnitude regressions, not to be tight.
        assert!(
            direct_p99 < 1000.0,
            "direct lane p99 {direct_p99:.3}ms exceeded 1000ms budget"
        );
        assert!(
            broker_p99 < 1000.0,
            "broker lane p99 {broker_p99:.3}ms exceeded 1000ms budget"
        );
    }

    fn refused(code: RefusalCode, retry_after_ms: u64) -> BrokerClientError {
        BrokerClientError::refused(code, "test", retry_after_ms)
    }

    /// Each actionable refusal code routes to the matching `BrokerRefusal`;
    /// everything else (PeerRejected, Unspecified, …) lands on `Other`.
    #[test]
    fn classify_maps_typed_refusals() {
        let cases = [
            (
                RefusalCode::VersionUnsupported,
                BrokerRefusal::VersionUnsupported,
            ),
            (RefusalCode::VersionBlocked, BrokerRefusal::VersionBlocked),
            (RefusalCode::ServiceUnknown, BrokerRefusal::ServiceUnknown),
            (
                RefusalCode::RateLimited,
                BrokerRefusal::RateLimited { retry_after_ms: 0 },
            ),
            (RefusalCode::ShuttingDown, BrokerRefusal::ShuttingDown),
            (RefusalCode::PeerRejected, BrokerRefusal::Other),
            (RefusalCode::Unspecified, BrokerRefusal::Other),
        ];
        for (code, expected) in cases {
            assert_eq!(
                BrokerRefusal::classify(&refused(code, 0)),
                Some(expected),
                "{code:?}"
            );
        }
    }

    #[test]
    fn classify_returns_none_for_disabled() {
        // Disabled is the escape hatch, not a refusal — no classification.
        assert_eq!(BrokerRefusal::classify(&BrokerClientError::Disabled), None);
    }

    /// `retry_after_ms` is threaded all the way through to
    /// `BrokerRefusal::RateLimited`. Catches the half-done fix where the typed
    /// surface drops the hint.
    #[test]
    fn classify_propagates_retry_after_ms_on_rate_limited() {
        assert_eq!(
            BrokerRefusal::classify(&refused(RefusalCode::RateLimited, 2500)),
            Some(BrokerRefusal::RateLimited {
                retry_after_ms: 2500
            })
        );
    }

    /// Adversarial: a future broker shipping a code this client predates
    /// (e.g. 999) must fall through to `BrokerRefusal::Other`, never panic.
    #[test]
    fn classify_maps_unknown_code_to_other() {
        assert_eq!(
            BrokerRefusal::classify(&refused(RefusalCode::from(999), 0)),
            Some(BrokerRefusal::Other),
            "unknown RefusalCode must classify as Other, not panic"
        );
    }

    /// Non-refusal failures are transport / protocol errors — they MUST
    /// classify as `None` so callers fall back to the direct-connect path.
    #[test]
    fn classify_treats_transport_failures_as_none() {
        let dial = BrokerClientError::BrokerConnect {
            endpoint: "/nowhere".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no broker"),
        };
        assert_eq!(BrokerRefusal::classify(&dial), None);

        let backend = BrokerClientError::BackendConnect(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pipe died",
        ));
        assert_eq!(BrokerRefusal::classify(&backend), None);

        let protocol = BrokerClientError::Protocol {
            detail: "missing result".to_string(),
        };
        assert_eq!(BrokerRefusal::classify(&protocol), None);
    }

    #[test]
    fn endpoint_translation_round_trips() {
        let endpoint = unique_test_endpoint();
        assert_eq!(
            to_zccache_endpoint(&to_running_process_endpoint(&endpoint)),
            endpoint
        );
        let native = crate::platform::ipc::Endpoint::from_running_process("name");
        assert_eq!(to_zccache_endpoint("name"), native.as_str());
        assert_eq!(to_running_process_endpoint(native.as_str()), "name");
    }
}
