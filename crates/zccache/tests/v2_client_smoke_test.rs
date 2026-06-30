//! Downstream-consumer smoke test for `running_process::broker::client_v2`.
//!
//! Tracks #777. Slice 4 of upstream #488 added a typed v2 broker client
//! (`client_v2::connect`) returning `Result<ClientSession, BrokerV2Error>`.
//! Asserting on the typed-error path here proves the v2 client API is
//! importable from a downstream consumer and that the `BrokerV2Error::Dial`
//! variant fires when no broker is listening — the exact behavior zccache's
//! eventual v1 → v2 migration will pattern-match on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use running_process::broker::client_v2::{connect, BrokerV2Error};

#[test]
fn client_v2_returns_dial_error_when_no_broker_running() {
    // Use a unique program name so a parallel test never accidentally
    // satisfies the connect.
    let err = connect("zccache-v2-smoke-no-broker-12345", "0.0.0")
        .expect_err("no broker running => Dial error");
    match err {
        BrokerV2Error::Dial { socket_path, .. } => {
            // The v2 broker uses different endpoint encodings per OS:
            // Windows pipes name the consumer (`rpb-v2-<program>-…`),
            // Linux abstract namespace likewise, but macOS hashes the bare
            // pipe name into `$TMPDIR/.rp-<uid>-broker-v2/<hex>.sock`
            // (104-byte sun_path limit). Universal invariant: the path
            // is routed through the v2 broker namespace.
            let v2_marker = if cfg!(target_os = "macos") {
                "broker-v2"
            } else {
                "rpb-v2-zccache-v2-smoke-no-broker-12345-"
            };
            assert!(
                socket_path.contains(v2_marker),
                "Dial socket_path should reference the v2 broker namespace \
                 (expected substring `{v2_marker}`), got: {socket_path}"
            );
        }
        // Sid lookup failure is acceptable in environments without
        // `/etc/machine-id` (CI containers, restricted launchd contexts).
        BrokerV2Error::Sid(_) => {}
        other => panic!("expected BrokerV2Error::Dial or Sid, got: {other:?}"),
    }
}
