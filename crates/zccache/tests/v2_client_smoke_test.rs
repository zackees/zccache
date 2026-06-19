//! Downstream-consumer smoke test for `running_process::broker::client_v2`.
//!
//! Tracks #777. Slice 4 of upstream #488 added a typed v2 broker client
//! (`client_v2::connect`) returning `Result<ClientSession, BrokerV2Error>`.
//! Asserting on the typed-error path here proves the v2 client API is
//! importable from a downstream consumer and that the `BrokerV2Error::Dial`
//! variant fires when no broker is listening — the exact behavior zccache's
//! eventual v1 → v2 migration will pattern-match on.

use running_process::broker::client_v2::{connect, BrokerV2Error};

#[test]
fn client_v2_returns_dial_error_when_no_broker_running() {
    // Use a unique program name so a parallel test never accidentally
    // satisfies the connect.
    let err = connect("zccache-v2-smoke-no-broker-12345", "0.0.0")
        .expect_err("no broker running => Dial error");
    match err {
        BrokerV2Error::Dial { socket_path, .. } => {
            assert!(
                socket_path.contains("rpb-v2-zccache-v2-smoke-no-broker-12345-"),
                "Dial socket_path should reference the v2 pipe namespace, got: {socket_path}"
            );
        }
        other => panic!("expected BrokerV2Error::Dial, got: {other:?}"),
    }
}
