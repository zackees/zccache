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

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;
use running_process::broker::client_v2::{self, BrokerV2Error, ClientSession};

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
    #[cfg(windows)]
    let name = {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        let bare = endpoint
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(endpoint);
        ToNsName::to_ns_name::<GenericNamespaced>(bare)?
    };

    #[cfg(unix)]
    let name = {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        ToFsName::to_fs_name::<GenericFilePath>(endpoint)?
    };

    let stream = Stream::connect(name)?;
    drop(stream);
    Ok(())
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
pub fn connect_v2_broker(wanted_version: &str) -> Result<ClientSession, BrokerV2Error> {
    client_v2::connect("zccache", wanted_version)
}

#[cfg(test)]
mod tests {
    use super::*;

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
