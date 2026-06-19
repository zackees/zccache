//! Downstream-consumer smoke test for `running_process::broker::protocol_v2`.
//!
//! Coordinates with [zackees/zccache#777](https://github.com/zackees/zccache/issues/777)
//! and [zackees/running-process#483](https://github.com/zackees/running-process/issues/483).
//! Slice 1 of the broker-v2 work (running-process PR #484) added the
//! `protocol_v2::ServiceDefinition` envelope + `HttpServerCapability`
//! optional sub-message. This test:
//!
//! 1. Proves the new v2 types are importable from a downstream consumer.
//! 2. Exercises a prost encode/decode round-trip end-to-end so the path-dep
//!    swap (zccache workspace `[patch.crates-io]` → local running-process)
//!    surfaces immediately if the proto shape regresses upstream.
//!
//! The "real" v2 migration (running-process broker binary, v2 client, v2
//! Frame streaming) is tracked in #777 — those slices land once the
//! upstream v2 baseline ships.

use prost::Message;
use running_process::broker::protocol_v2::{HttpServerCapability, ServiceDefinition};

#[test]
fn protocol_v2_service_definition_round_trips_without_http() {
    let original = ServiceDefinition {
        service_name: "zccache".to_owned(),
        http_server: None,
    };

    let bytes = original.encode_to_vec();
    let decoded =
        ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

    assert_eq!(decoded.service_name, "zccache");
    assert!(decoded.http_server.is_none());
}

#[test]
fn protocol_v2_service_definition_round_trips_with_http_capability() {
    let original = ServiceDefinition {
        service_name: "zccache".to_owned(),
        http_server: Some(HttpServerCapability {
            bind_addr: "127.0.0.1".to_owned(),
            health_path: "/health".to_owned(),
            display_name: "zccache status".to_owned(),
        }),
    };

    let bytes = original.encode_to_vec();
    let decoded =
        ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

    let cap = decoded
        .http_server
        .expect("http_server survives round-trip");
    assert_eq!(decoded.service_name, "zccache");
    assert_eq!(cap.bind_addr, "127.0.0.1");
    assert_eq!(cap.health_path, "/health");
    assert_eq!(cap.display_name, "zccache status");
}
