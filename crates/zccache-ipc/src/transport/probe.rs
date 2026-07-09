//! Running-process `BackendHandle` identity probe served on the same
//! endpoint as the zccache daemon wire. Disambiguates probe frames from
//! v15/v16 zccache traffic and the FrameV1 zccache lane.
//!
//! ## Slice 17 of #500 — v1 envelope retention decision
//!
//! This module touches three v1 envelope-layer symbols:
//!
//! | Symbol                                            | Decision       | Rationale |
//! |---------------------------------------------------|----------------|-----------|
//! | `running_process::broker::protocol::ENVELOPE_VERSION` | **keep on v1** | The probe path itself is a v1 protocol artifact (`BackendHandle` lives on the v1 broker namespace); the v2 broker uses Frame streaming (`OPEN/DATA/CLOSE/…`) instead of the v1 envelope shape. There is no v2 envelope to migrate this to. |
//! | `running_process::broker::protocol::MAX_FRAME_BYTES`  | **keep on v1** | Same reasoning — this is the cap on the v1 envelope body length; the v2 streaming layer has its own per-stream windowing primitives. |
//! | `running_process::broker::protocol::Frame`           | **keep on v1** | The probe message is itself a v1 `Frame` payload (kind `FRAME_KIND_REQUEST`, payload protocol `BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL`). Migrating the *probe* to v2 is a separate, larger migration that replaces the whole probe contract with a v2-streaming equivalent (tracked in zccache#782 Phase B). |
//!
//! The slice 17 decision is therefore "no code change to envelope
//! references in this file" — the v1 path stays valid as a parallel
//! lane per #470's coexistence table; the v2 streaming protocol
//! replaces it when the backend-handle-probe path itself is migrated
//! to a v2 control verb in a later phase. This module-level docblock
//! exists so a future reader doesn't grep for these symbols looking
//! for a "missing v2 migration" — the decision is documented here.

use bytes::{Buf, BytesMut};
use prost::Message as _;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::error::IpcError;

use super::framing::ensure_buffered;

pub(super) async fn try_serve_backend_handle_probe<R, W>(
    reader: &mut R,
    writer: &mut W,
    read_buf: &mut BytesMut,
    daemon: &running_process::broker::protocol_v2::backend_handle::DaemonProcess,
) -> Result<bool, IpcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    ensure_buffered(reader, read_buf, 8).await?;
    if read_buf.is_empty() {
        return Ok(false);
    }

    let running_process_version = running_process::broker::protocol::ENVELOPE_VERSION;
    if read_buf[0] != running_process_version {
        return Ok(false);
    }

    let zccache_len = u32::from_le_bytes([read_buf[0], read_buf[1], read_buf[2], read_buf[3]]);
    let zccache_version = u32::from_le_bytes([read_buf[4], read_buf[5], read_buf[6], read_buf[7]]);
    if zccache_len >= 4
        && matches!(
            zccache_version,
            zccache_protocol::BINCODE_PROTOCOL_VERSION | zccache_protocol::PROST_PROTOCOL_VERSION
        )
    {
        return Ok(false);
    }

    let body_len =
        u32::from_le_bytes([read_buf[1], read_buf[2], read_buf[3], read_buf[4]]) as usize;
    if body_len > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(IpcError::Endpoint(format!(
            "running-process BackendHandle probe frame too large: {body_len} bytes"
        )));
    }
    ensure_buffered(reader, read_buf, 5 + body_len).await?;

    // Decode from a peek so non-probe frames (e.g. the zccache FrameV1
    // request lane, which shares this framing) stay buffered for the
    // dispatching `recv_wire` decoder.
    let frame = running_process::broker::protocol::Frame::decode(&read_buf[5..5 + body_len])
        .map_err(|err| IpcError::Endpoint(format!("BackendHandle probe decode failed: {err}")))?;
    if !is_backend_handle_probe_request(&frame) {
        if frame.payload_protocol == zccache_protocol::wire_frame::ZCCACHE_FRAME_PAYLOAD_PROTOCOL {
            return Ok(false);
        }
        return Err(IpcError::Endpoint(
            "unexpected running-process frame on zccache daemon endpoint".to_string(),
        ));
    }
    read_buf.advance(5 + body_len);

    let response = backend_handle_probe_response(&frame, daemon)?;
    write_running_process_frame(writer, &response).await?;
    Ok(true)
}

fn is_backend_handle_probe_request(frame: &running_process::broker::protocol::Frame) -> bool {
    use running_process::broker::protocol::{FrameKind, PayloadEncoding};
    use running_process::broker::protocol_v2::backend_handle::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;

    frame.envelope_version == 1
        && FrameKind::try_from(frame.kind) == Ok(FrameKind::Request)
        && frame.payload_protocol == BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL
        && PayloadEncoding::try_from(frame.payload_encoding) == Ok(PayloadEncoding::None)
        && frame.payload.len() == 32
}

fn backend_handle_probe_response(
    request: &running_process::broker::protocol::Frame,
    daemon: &running_process::broker::protocol_v2::backend_handle::DaemonProcess,
) -> Result<running_process::broker::protocol::Frame, IpcError> {
    use running_process::broker::protocol::{Frame, FrameKind, PayloadEncoding};
    use running_process::broker::protocol_v2::backend_handle::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;

    let mut payload = Vec::with_capacity(32 + 128);
    payload.extend_from_slice(&request.payload);
    daemon.to_proto().encode(&mut payload).map_err(|err| {
        IpcError::Endpoint(format!("BackendHandle identity encode failed: {err}"))
    })?;

    Ok(Frame {
        envelope_version: 1,
        kind: FrameKind::Response as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload,
        request_id: request.request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: request.traceparent.clone(),
        tracestate: request.tracestate.clone(),
    })
}

async fn write_running_process_frame<W>(
    writer: &mut W,
    frame: &running_process::broker::protocol::Frame,
) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
{
    let mut body = Vec::new();
    frame.encode(&mut body).map_err(|err| {
        IpcError::Endpoint(format!("BackendHandle response encode failed: {err}"))
    })?;
    if body.len() > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(IpcError::Endpoint(format!(
            "BackendHandle response frame too large: {} bytes",
            body.len()
        )));
    }
    writer
        .write_all(&[running_process::broker::protocol::ENVELOPE_VERSION])
        .await?;
    writer.write_all(&(body.len() as u32).to_le_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}
