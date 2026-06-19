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

use running_process::broker::client_v2::{self, BrokerV2Error, ClientSession};

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
                assert!(
                    socket_path.contains("rpb-v2-zccache-"),
                    "Dial socket_path should reference the v2 zccache pipe, got: {socket_path}"
                );
            }
            other => panic!("expected BrokerV2Error::Dial, got: {other:?}"),
        }
    }
}
