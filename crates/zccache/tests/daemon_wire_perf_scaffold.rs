#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use bytes::BytesMut;
use std::time::Instant;
use zccache::protocol::wire_prost::{decode_prost_message, encode_prost_message, zccache_v1 as pb};

#[test]
fn prost_roundtrip_perf_scaffold_records_nonzero_samples() {
    let request = pb::Request {
        body: Some(pb::request::Body::CompileEphemeral(pb::CompileEphemeral {
            client_pid: 123,
            working_dir: Some(pb::Path {
                value: "/work/project".to_string(),
            }),
            compiler: Some(pb::Path {
                value: "/usr/bin/clang++".to_string(),
            }),
            args: vec![
                "-c".to_string(),
                "main.cc".to_string(),
                "-o".to_string(),
                "main.o".to_string(),
            ],
            cwd: Some(pb::Path {
                value: "/work/project".to_string(),
            }),
            env: vec![pb::EnvVar {
                name: "PATH".to_string(),
                value: "/usr/bin".to_string(),
            }],
            env_is_set: true,
            stdin: Vec::new(),
        })),
        request_id: "perf-sample".to_string(),
    };

    let start = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..128 {
        let encoded = encode_prost_message(&request).unwrap();
        bytes += encoded.len();
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = decode_prost_message::<pb::Request>(&mut buf)
            .unwrap()
            .unwrap();
        assert!(matches!(
            decoded.body,
            Some(pb::request::Body::CompileEphemeral(_))
        ));
    }

    let elapsed = start.elapsed();
    assert!(bytes > 0);
    assert!(elapsed.as_nanos() > 0);
}
