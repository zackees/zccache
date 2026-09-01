#![allow(clippy::missing_errors_doc)]

pub(crate) use zccache_platform as platform;

pub mod daemon_mgmt;

use serde::{Deserialize, Serialize};
use zccache_core::NormalizedPath;
use zccache_download::{DownloadDaemonStatus, DownloadOptions, DownloadStatus};

pub const PROTOCOL_VERSION: u32 = 1;

/// Largest bincode payload accepted by the download daemon protocol.
///
/// Download protocol messages contain control metadata rather than artifact
/// bytes. Keeping this finite prevents a peer-controlled length prefix from
/// retaining an unbounded IPC read buffer.
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Errors produced while framing or decoding download daemon messages.
#[derive(Debug)]
pub enum ProtocolError {
    /// The bincode payload could not be serialized or decoded.
    Bincode(bincode::Error),
    /// A peer announced, or an encoder produced, a payload beyond the cap.
    MessageTooLarge { size: usize, max: usize },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bincode(error) => error.fmt(formatter),
            Self::MessageTooLarge { size, max } => {
                write!(
                    formatter,
                    "download protocol message too large: {size} bytes (max {max})"
                )
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bincode(error) => Some(error.as_ref()),
            Self::MessageTooLarge { .. } => None,
        }
    }
}

impl From<bincode::Error> for ProtocolError {
    fn from(error: bincode::Error) -> Self {
        Self::Bincode(error)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Status,
    Shutdown,
    DownloadAttach {
        url: String,
        destination: NormalizedPath,
        options: DownloadOptions,
    },
    DownloadStatus,
    DownloadWait {
        timeout_ms: Option<u64>,
    },
    DownloadCancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Pong,
    Status(DownloadDaemonStatus),
    ShuttingDown,
    DownloadAttached {
        download_id: String,
        initiator: bool,
        status: DownloadStatus,
    },
    DownloadStatusResult {
        status: DownloadStatus,
    },
    DownloadFinished {
        status: DownloadStatus,
    },
    DownloadCancelled {
        status: DownloadStatus,
    },
    Error {
        message: String,
    },
}

pub fn encode_message<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = bincode::serialize(msg)?;
    ensure_message_size(payload.len())?;
    let mut out = Vec::with_capacity(4 + payload.len());
    // The 1 MiB cap above is well within the u32 length prefix.
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_message<T: serde::de::DeserializeOwned>(
    buf: &mut bytes::BytesMut,
) -> Result<Option<T>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    ensure_message_size(len)?;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let payload = buf.split_to(4 + len).freeze();
    let msg = bincode::deserialize::<T>(&payload[4..])?;
    Ok(Some(msg))
}

fn ensure_message_size(size: usize) -> Result<(), ProtocolError> {
    if size > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge {
            size,
            max: MAX_MESSAGE_SIZE,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_limit_payload_encodes_and_decodes() {
        // Bincode prefixes `Vec<u8>` with an eight-byte length.
        let message = vec![0xA5; MAX_MESSAGE_SIZE - 8];
        let encoded = encode_message(&message).expect("payload at the cap encodes");
        assert_eq!(encoded.len() - 4, MAX_MESSAGE_SIZE);

        let mut buf = bytes::BytesMut::from(encoded.as_slice());
        let decoded: Option<Vec<u8>> =
            decode_message(&mut buf).expect("payload at the cap decodes");
        assert_eq!(decoded, Some(message));
        assert!(buf.is_empty());
    }

    #[test]
    fn oversize_payload_is_rejected_by_both_encoder_and_decoder() {
        let message = vec![0xA5; MAX_MESSAGE_SIZE];
        assert!(matches!(
            encode_message(&message),
            Err(ProtocolError::MessageTooLarge { .. })
        ));

        let mut buf = bytes::BytesMut::from(&(MAX_MESSAGE_SIZE as u32 + 1).to_le_bytes()[..]);
        assert!(matches!(
            decode_message::<Vec<u8>>(&mut buf),
            Err(ProtocolError::MessageTooLarge {
                size,
                max: MAX_MESSAGE_SIZE,
            }) if size == MAX_MESSAGE_SIZE + 1
        ));
        assert_eq!(buf.len(), 4, "oversize prefix remains unconsumed");
    }

    #[test]
    fn incomplete_frame_waits_without_consuming_its_prefix() {
        let mut buf = bytes::BytesMut::new();
        buf.extend_from_slice(&4_u32.to_le_bytes());
        buf.extend_from_slice(&[0x00, 0x00]);

        let decoded: Option<Vec<u8>> = decode_message(&mut buf).expect("incomplete frame waits");
        assert_eq!(decoded, None);
        assert_eq!(buf.len(), 6);
    }
}
