//! Broker-mediated connect lane for the daemon client path.
//!
//! Wires `running_process::broker::client::connect_to_backend` in front of
//! zccache's direct IPC connect (upstream tracking:
//! zackees/running-process#383). Lane selection precedence:
//!
//! 1. `RUNNING_PROCESS_DISABLE=1` — the canonical upstream escape hatch.
//!    The broker lane (including the fake-backend test seam) is bypassed
//!    entirely and the pre-adoption direct connect is used.
//! 2. `RUNNING_PROCESS_FAKE_BACKEND=<endpoint>` — upstream TEST-ONLY seam:
//!    `connect_to_backend` dials the endpoint directly, skipping broker
//!    discovery and Hello negotiation. Never set in production.
//! 3. `ZCCACHE_BROKER_CONNECT=1` — opt-in production lane: a broker Hello
//!    resolves the backend endpoint. Any broker failure (broker absent,
//!    negotiation refused, resolved endpoint unreachable) falls back
//!    silently to the direct connect — the broker lane must never make a
//!    previously-working build fail.
//! 4. Default — direct connect, byte-for-byte the pre-adoption behavior.
//!
//! The negotiated (or seam) connection is consumed for **endpoint
//! resolution only**; the data connection is then opened with zccache's
//! own tokio transport so recv timeouts, the Windows named-pipe client
//! backoff, and the v15/v16/FrameV1 wire lanes keep working unchanged.
//! Adopting the negotiated `interprocess` stream as the data connection is
//! deferred until the broker lane becomes the default.

use running_process::broker::client::{
    connect_local_socket, connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};

use super::error::IpcError;
use super::{connect, running_process_disabled, ClientConnection};

/// Upstream TEST-ONLY seam: a non-empty value short-circuits the broker
/// negotiation and dials the given running-process endpoint directly.
///
/// Mirrors the `running_process::broker::client` seam contract (the
/// constant ships upstream after 4.1.0; replace this local copy with the
/// upstream re-export on the next running-process bump). The canonical
/// `RUNNING_PROCESS_DISABLE=1` hatch takes precedence: a disabled broker
/// ignores the fake seam too. Never set this in production.
pub const RUNNING_PROCESS_FAKE_BACKEND_ENV: &str = "RUNNING_PROCESS_FAKE_BACKEND";

/// Opt-in switch for the production broker-negotiation lane.
///
/// The lane stays opt-in until the upstream perf gate (running-process
/// #383 item 2) is satisfied and a shared broker actually manages zccache
/// daemons; today zccache self-spawns its daemon, so the default path
/// keeps the direct connect.
pub const ZCCACHE_BROKER_CONNECT_ENV: &str = "ZCCACHE_BROKER_CONNECT";

/// How [`connect_daemon`] reached the daemon endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonConnectRoute {
    /// Existing direct connect — the default and every fallback/escape-hatch
    /// path.
    Direct,
    /// Endpoint resolved through `connect_to_backend`.
    Broker {
        /// Route reported by the running-process broker client.
        route: BackendConnectionRoute,
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

    if let Some((resolved, route)) = resolve_backend_endpoint().await {
        match connect(&resolved).await {
            Ok(conn) => {
                return Ok((
                    conn,
                    DaemonConnectRoute::Broker {
                        route,
                        endpoint: resolved,
                    },
                ));
            }
            Err(err) => {
                tracing::debug!(
                    resolved_endpoint = %resolved,
                    error = %err,
                    "broker-resolved endpoint unreachable; falling back to direct connect"
                );
            }
        }
    }

    let conn = connect(endpoint).await?;
    Ok((conn, DaemonConnectRoute::Direct))
}

/// Is the broker lane requested for this process?
///
/// True when the upstream fake-backend test seam is set (non-empty) or the
/// production opt-in is enabled. The `RUNNING_PROCESS_DISABLE=1` precedence
/// check happens in [`connect_daemon_with_route`] before this is consulted.
fn broker_lane_requested() -> bool {
    if std::env::var_os(RUNNING_PROCESS_FAKE_BACKEND_ENV).is_some_and(|value| !value.is_empty()) {
        return true;
    }
    std::env::var(ZCCACHE_BROKER_CONNECT_ENV).is_ok_and(|value| value == "1")
}

/// Run broker resolution (blocking, on a worker thread) and return the
/// resolved endpoint in zccache connect form plus the broker route.
///
/// Returns `None` on any broker-side failure; the caller falls back to the
/// direct connect. The negotiated stream is dropped here — see the module
/// docs for why endpoint resolution and the data connection are separate.
async fn resolve_backend_endpoint() -> Option<(String, BackendConnectionRoute)> {
    match tokio::task::spawn_blocking(resolve_backend_endpoint_blocking).await {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::debug!(error = %err, "broker negotiation task failed; using direct connect");
            None
        }
    }
}

fn resolve_backend_endpoint_blocking() -> Option<(String, BackendConnectionRoute)> {
    // The fake-backend seam dials the endpoint directly, skipping broker
    // discovery, Hello negotiation, and version checks entirely — matching
    // the upstream connect_to_backend seam semantics.
    if let Some(seam_endpoint) = fake_backend_endpoint_from_env() {
        return match connect_local_socket(&seam_endpoint) {
            Ok(stream) => {
                drop(stream);
                Some((
                    to_zccache_endpoint(&seam_endpoint),
                    BackendConnectionRoute::HelloSkip,
                ))
            }
            Err(err) => {
                tracing::warn!(
                    endpoint = %seam_endpoint,
                    error = %err,
                    "RUNNING_PROCESS_FAKE_BACKEND endpoint unreachable; using direct connect"
                );
                None
            }
        };
    }

    let broker_endpoint = default_broker_endpoint()?;
    let request = ConnectBackendRequest::new(
        &broker_endpoint,
        "zccache",
        crate::core::VERSION,
        crate::core::VERSION,
    );
    match connect_to_backend(request) {
        Ok(connection) => Some((to_zccache_endpoint(&connection.endpoint), connection.route)),
        Err(err) => {
            tracing::debug!(
                error = %err,
                "running-process broker negotiation failed; using direct connect"
            );
            None
        }
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

/// Derive the per-user shared-broker endpoint for this host.
fn default_broker_endpoint() -> Option<String> {
    let sid_hash = running_process::broker::lifecycle::user_sid_hash().ok()?;
    let pipe = running_process::broker::lifecycle::names::shared_broker_pipe(&sid_hash).ok()?;
    #[cfg(windows)]
    {
        pipe.windows
    }
    #[cfg(unix)]
    {
        pipe.unix.map(|path| path.to_string_lossy().into_owned())
    }
}

/// Translate a running-process backend endpoint into zccache connect form.
///
/// running-process uses bare pipe names on Windows (`interprocess`
/// namespaced names) while zccache's transport expects the full
/// `\\.\pipe\` form. Unix socket paths are shared verbatim.
fn to_zccache_endpoint(endpoint: &str) -> String {
    #[cfg(windows)]
    {
        if endpoint.starts_with(r"\\.\pipe\") {
            endpoint.to_string()
        } else {
            format!(r"\\.\pipe\{endpoint}")
        }
    }
    #[cfg(unix)]
    {
        endpoint.to_string()
    }
}

/// Strip a zccache endpoint down to the running-process local-socket form.
///
/// Inverse of the private `to_zccache_endpoint`; used by tests and tooling
/// that hand
/// a zccache endpoint to the upstream fake-backend seam.
#[must_use]
pub fn to_running_process_endpoint(endpoint: &str) -> String {
    #[cfg(windows)]
    {
        endpoint
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(endpoint)
            .to_string()
    }
    #[cfg(unix)]
    {
        endpoint.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::test_env::EnvVarGuard;
    use crate::ipc::{unique_test_endpoint, IpcListener, RUNNING_PROCESS_DISABLE_ENV};
    use crate::protocol::{Request, Response};

    /// Spawn a ping server that accepts up to `accepts` connections;
    /// connections that close without sending a request are tolerated
    /// (the broker lane's resolution dial closes immediately).
    fn spawn_ping_server(
        mut listener: IpcListener,
        accepts: usize,
    ) -> tokio::task::JoinHandle<usize> {
        tokio::spawn(async move {
            let mut answered = 0;
            for _ in 0..accepts {
                let Ok(mut conn) = listener.accept().await else {
                    break;
                };
                match conn.recv::<Request>().await {
                    Ok(Some(Request::Ping)) => {
                        conn.send(&Response::Pong).await.unwrap();
                        answered += 1;
                        break;
                    }
                    // Resolution dial dropped without a request — keep
                    // accepting until the data connection arrives.
                    Ok(None) | Err(_) => continue,
                    Ok(Some(other)) => panic!("unexpected request: {other:?}"),
                }
            }
            answered
        })
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
            ZCCACHE_BROKER_CONNECT_ENV,
        ]);

        let endpoint = unique_test_endpoint();
        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_ping_server(listener, 1);

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
            (ZCCACHE_BROKER_CONNECT_ENV, None),
        ]);

        let listener = IpcListener::bind(&endpoint).unwrap();
        // Two accepts: the connect_to_backend resolution dial (dropped) and
        // the zccache data connection.
        let server = spawn_ping_server(listener, 2);

        let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
        match route {
            DaemonConnectRoute::Broker {
                route: BackendConnectionRoute::HelloSkip,
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
            (ZCCACHE_BROKER_CONNECT_ENV, Some("1".to_string())),
        ]);

        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_ping_server(listener, 1);

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
            (ZCCACHE_BROKER_CONNECT_ENV, None),
        ]);

        let listener = IpcListener::bind(&endpoint).unwrap();
        let server = spawn_ping_server(listener, 1);

        let (mut conn, route) = connect_daemon_with_route(&endpoint).await.unwrap();
        assert_eq!(route, DaemonConnectRoute::Direct);
        ping_roundtrip(&mut conn).await;

        assert_eq!(server.await.unwrap(), 1);
    }

    #[test]
    fn endpoint_translation_round_trips() {
        let endpoint = unique_test_endpoint();
        assert_eq!(
            to_zccache_endpoint(&to_running_process_endpoint(&endpoint)),
            endpoint
        );
        #[cfg(windows)]
        {
            assert_eq!(to_zccache_endpoint("name"), r"\\.\pipe\name");
            assert_eq!(to_running_process_endpoint(r"\\.\pipe\name"), "name");
        }
    }
}
