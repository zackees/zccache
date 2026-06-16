//! Shared framing helpers for the IPC transport: length-prefixed bincode
//! and dual-wire prost decode loops.

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::ipc::error::IpcError;

/// Decode a dual-wire response message into the internal [`Response`]
/// type, mapping prost conversion failures into [`IpcError::Protocol`].
///
/// [`Response`]: crate::protocol::Response
pub(super) fn decode_response_wire(
    message: Option<
        crate::protocol::DecodedWireMessage<
            crate::protocol::Response,
            crate::protocol::wire_prost::zccache_v1::Response,
        >,
    >,
) -> Result<Option<crate::protocol::Response>, IpcError> {
    message
        .map(crate::protocol::wire_prost::response_from_decoded_wire)
        .transpose()
        .map_err(IpcError::Protocol)
}

pub(super) async fn recv_bincode_loop<R, T>(
    reader: &mut R,
    read_buf: &mut BytesMut,
) -> Result<Option<T>, IpcError>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    loop {
        if let Some(msg) = crate::protocol::decode_message::<T>(read_buf)? {
            return Ok(Some(msg));
        }
        if !read_next_chunk(reader, read_buf).await? {
            return Ok(None);
        }
    }
}

pub(super) async fn recv_wire_loop<R, Bincode, Prost>(
    reader: &mut R,
    read_buf: &mut BytesMut,
) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
where
    R: AsyncRead + Unpin,
    Bincode: serde::de::DeserializeOwned,
    Prost: prost::Message + Default,
{
    loop {
        if let Some(msg) = crate::protocol::decode_wire_message::<Bincode, Prost>(read_buf)? {
            return Ok(Some(msg));
        }
        if !read_next_chunk(reader, read_buf).await? {
            return Ok(None);
        }
    }
}

pub(super) async fn read_next_chunk<R>(
    reader: &mut R,
    read_buf: &mut BytesMut,
) -> Result<bool, IpcError>
where
    R: AsyncRead + Unpin,
{
    let mut tmp = [0u8; 4096];
    let n = reader.read(&mut tmp).await?;
    if n == 0 {
        if read_buf.is_empty() {
            return Ok(false);
        }
        return Err(IpcError::ConnectionClosed);
    }
    read_buf.extend_from_slice(&tmp[..n]);
    Ok(true)
}

pub(super) async fn ensure_buffered<R>(
    reader: &mut R,
    read_buf: &mut BytesMut,
    min_len: usize,
) -> Result<(), IpcError>
where
    R: AsyncRead + Unpin,
{
    while read_buf.len() < min_len {
        if !read_next_chunk(reader, read_buf).await? {
            if read_buf.is_empty() {
                return Ok(());
            }
            return Err(IpcError::ConnectionClosed);
        }
    }
    Ok(())
}
