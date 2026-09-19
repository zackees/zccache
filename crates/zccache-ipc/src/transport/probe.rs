//! Frozen v1 daemon identity probe served on the same endpoint as the
//! zccache daemon wire. Disambiguates probe frames from direct zccache
//! traffic (including the retired v25 header) and the FrameV1 zccache lane.
//!
//! The probe contract (envelope, nonce proof, identity encoding) is owned by
//! `kernal_api::daemon_identity::ProbeResponder`. zccache only decides which
//! leading bytes belong to its own legacy wire
//! ([`zccache_protocol::wire_frame::buffer_starts_running_process_frame`]) and
//! buffers enough of the connection for the responder to classify it.

use bytes::{Buf, BytesMut};
use kernal_api::daemon_frame_v1::{DAEMON_FRAME_V1_MAX_BODY_BYTES, DAEMON_FRAME_V1_VERSION};
use kernal_api::daemon_identity::{LegacyPrefix, ProbeMuxResult, ProbeResponder};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::error::IpcError;

use super::framing::ensure_buffered;

/// v1 header: one version byte plus a little-endian `u32` body length.
const FRAME_V1_HEADER_BYTES: usize = 5;

pub(super) async fn try_serve_backend_handle_probe<R, W>(
    reader: &mut R,
    writer: &mut W,
    read_buf: &mut BytesMut,
    responder: &ProbeResponder,
) -> Result<bool, IpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    ensure_buffered(reader, read_buf, 8).await?;
    if read_buf.is_empty() || read_buf[0] != DAEMON_FRAME_V1_VERSION {
        return Ok(false);
    }

    let legacy_prefix =
        match zccache_protocol::wire_frame::buffer_starts_running_process_frame(read_buf) {
            Some(false) => return Ok(false),
            Some(true) => LegacyPrefix::NotLegacy,
            None => LegacyPrefix::NeedMoreBytes,
        };

    let body_len =
        u32::from_le_bytes([read_buf[1], read_buf[2], read_buf[3], read_buf[4]]) as usize;
    if body_len > DAEMON_FRAME_V1_MAX_BODY_BYTES {
        return Err(IpcError::Endpoint(format!(
            "daemon identity probe frame too large: {body_len} bytes"
        )));
    }
    ensure_buffered(reader, read_buf, FRAME_V1_HEADER_BYTES + body_len).await?;

    // Non-probe frames (the zccache FrameV1 request lane shares this framing)
    // are never consumed here, so the dispatching `recv_wire` decoder sees them.
    match responder.poll(read_buf, legacy_prefix) {
        Ok(ProbeMuxResult::ProbeReply { reply, consumed }) => {
            read_buf.advance(consumed);
            writer.write_all(&reply).await?;
            writer.flush().await?;
            Ok(true)
        }
        Ok(
            ProbeMuxResult::ProductFrame { .. }
            | ProbeMuxResult::Legacy
            | ProbeMuxResult::NeedMoreBytes,
        ) => Ok(false),
        Err(err) => Err(IpcError::Endpoint(format!(
            "daemon identity probe rejected on zccache daemon endpoint: {err}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use kernal_api::daemon_frame_v1::{DaemonFrameCodec, DaemonFrameDecode};
    use kernal_api::daemon_identity::{DaemonEndpoint, DaemonIdentity, DaemonIdentityHashPolicy};
    use prost::Message;

    use super::*;

    /// Frozen v1 identity-probe payload protocol (running-process
    /// `BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL`), spelled here only so the test
    /// can play the prober's role.
    const PROBE_PAYLOAD_PROTOCOL: u32 = 0xB232;

    /// The daemon identity shape decoded by brokers released before BLAKE3.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyDaemonProcess {
        #[prost(uint32, tag = "1")]
        pid: u32,
        #[prost(string, tag = "2")]
        exe_path: String,
        #[prost(bytes = "vec", tag = "3")]
        exe_sha256: Vec<u8>,
    }

    #[test]
    fn probe_reply_leaves_the_legacy_sha256_identity_empty() {
        let current = DaemonIdentity::current_process_with_hash_policy(
            DaemonEndpoint::new("zccache-ipc-test", "zccache-ipc-test.sock"),
            None,
            DaemonIdentityHashPolicy::Blake3Only,
        )
        .expect("current daemon identity");
        let mut record = current.to_record();
        record.executable_path = std::path::PathBuf::from("missing-after-daemon-start");
        let responder = crate::backend_probe_responder(DaemonIdentity::from_record(record));

        let nonce = vec![0xa5; 32];
        let request = DaemonFrameCodec::encode_request(PROBE_PAYLOAD_PROTOCOL, nonce.clone(), 7)
            .expect("encode probe request");
        let reply = match responder.poll(&request, LegacyPrefix::NotLegacy) {
            Ok(ProbeMuxResult::ProbeReply { reply, consumed }) => {
                assert_eq!(consumed, request.len());
                reply
            }
            other => panic!("expected a probe reply, got {other:?}"),
        };
        let frame = match DaemonFrameCodec::decode(&reply).expect("decode probe reply") {
            DaemonFrameDecode::Frame { frame, .. } => frame,
            DaemonFrameDecode::NeedMoreBytes => panic!("probe reply must be complete"),
        };
        let payload = frame.payload();
        assert_eq!(&payload[..32], nonce.as_slice());
        let legacy = LegacyDaemonProcess::decode(&payload[32..])
            .expect("stable broker decodes probe identity");

        assert_eq!(legacy.exe_sha256.len(), 32);
        assert_eq!(legacy.exe_sha256, [0; 32]);
    }
}
