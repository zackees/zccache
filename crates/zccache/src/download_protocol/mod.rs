#![allow(clippy::missing_errors_doc)]

pub mod daemon_mgmt;

use serde::{Deserialize, Serialize};
use crate::core::NormalizedPath;
use crate::download::{DownloadDaemonStatus, DownloadOptions, DownloadStatus};

pub const PROTOCOL_VERSION: u32 = 1;

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

pub fn encode_message<T: Serialize>(msg: &T) -> Result<Vec<u8>, bincode::Error> {
    let payload = bincode::serialize(msg)?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_message<T: serde::de::DeserializeOwned>(
    buf: &mut bytes::BytesMut,
) -> Result<Option<T>, bincode::Error> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let payload = buf.split_to(4 + len).freeze();
    let msg = bincode::deserialize::<T>(&payload[4..])?;
    Ok(Some(msg))
}
