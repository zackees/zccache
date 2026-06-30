#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::ffi::OsString;

use zccache::ipc::{
    daemon_control_roundtrip, unique_test_endpoint, DaemonControlRequest, IpcError, IpcListener,
};
use zccache::protocol::wire_prost::{wire_format_from_env_value, WireFormat, WIRE_FORMAT_ENV};
use zccache::protocol::{
    ProtocolError, Request, Response, BINCODE_PROTOCOL_VERSION, PROST_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};

struct WireEnvGuard {
    previous: Option<OsString>,
}

impl WireEnvGuard {
    fn unset_for_auto() -> Self {
        let previous = std::env::var_os(WIRE_FORMAT_ENV);
        std::env::remove_var(WIRE_FORMAT_ENV);
        Self { previous }
    }
}

impl Drop for WireEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(WIRE_FORMAT_ENV, value),
            None => std::env::remove_var(WIRE_FORMAT_ENV),
        }
    }
}

#[test]
fn current_protocol_version_remains_v15_bincode() {
    assert_eq!(PROTOCOL_VERSION, BINCODE_PROTOCOL_VERSION);
    assert_ne!(PROTOCOL_VERSION, PROST_PROTOCOL_VERSION);
}

#[test]
fn wire_env_accepts_current_and_planned_modes() {
    assert_eq!(
        wire_format_from_env_value(None).unwrap(),
        WireFormat::ProstV16
    );
    assert_eq!(
        wire_format_from_env_value(Some("auto")).unwrap(),
        WireFormat::ProstV16
    );
    assert_eq!(
        wire_format_from_env_value(Some("bincode")).unwrap(),
        WireFormat::BincodeV15
    );
    assert_eq!(
        wire_format_from_env_value(Some("prost")).unwrap(),
        WireFormat::ProstV16
    );
    assert_eq!(
        wire_format_from_env_value(Some("v16")).unwrap(),
        WireFormat::ProstV16
    );
}

#[test]
fn wire_env_rejects_unknown_values() {
    let err = wire_format_from_env_value(Some("json")).unwrap_err();
    assert!(err.contains(WIRE_FORMAT_ENV));
    assert!(err.contains("bincode"));
    assert!(err.contains("prost"));
}

#[tokio::test]
async fn auto_client_falls_back_against_previous_release_v15_daemon() {
    let _env = WireEnvGuard::unset_for_auto();
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut first = listener.accept().await.unwrap();
        let err = first
            .recv::<Request>()
            .await
            .expect_err("previous-release v15 daemon must reject the first v16 prost frame");
        assert!(matches!(
            err,
            IpcError::Protocol(ProtocolError::VersionMismatch {
                expected: BINCODE_PROTOCOL_VERSION,
                received: PROST_PROTOCOL_VERSION,
            })
        ));
        first
            .send(&Response::Error {
                message: format!(
                    "protocol version mismatch: expected v{BINCODE_PROTOCOL_VERSION}, received \
                     v{PROST_PROTOCOL_VERSION}"
                ),
            })
            .await
            .unwrap();

        let mut second = listener.accept().await.unwrap();
        let request: Option<Request> = second.recv().await.unwrap();
        assert_eq!(request, Some(Request::Ping));
        second.send(&Response::Pong).await.unwrap();
    });

    let response = daemon_control_roundtrip(&endpoint, DaemonControlRequest::Ping, None)
        .await
        .unwrap();
    assert_eq!(response, Some(Response::Pong));

    server.await.unwrap();
}
