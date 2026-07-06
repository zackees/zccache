//! Shared tar+gzip helpers used by `gha-cache` and `rust-plan` backends.
//!
//! The synchronous codec lives in the library at
//! `crate::artifact::rust_plan::{tar_gz_encode, tar_gz_decode}` (single home);
//! these async wrappers run it on Tokio's blocking pool.

use crate::artifact::{tar_gz_decode, tar_gz_encode};
use crate::core::NormalizedPath;

async fn join_io<T>(
    handle: tokio::task::JoinHandle<Result<T, std::io::Error>>,
) -> Result<T, std::io::Error> {
    handle
        .await
        .map_err(|err| std::io::Error::other(format!("blocking archive task failed: {err}")))?
}

/// Create a tar.gz archive from a directory path on Tokio's blocking pool.
pub(crate) async fn tar_gz_encode_async(src: NormalizedPath) -> Result<Vec<u8>, std::io::Error> {
    join_io(tokio::task::spawn_blocking(move || {
        tar_gz_encode(src.as_path())
    }))
    .await
}

/// Extract a tar.gz archive into a destination directory on Tokio's blocking pool.
pub(crate) async fn tar_gz_decode_async(
    data: Vec<u8>,
    dest: NormalizedPath,
) -> Result<(), std::io::Error> {
    join_io(tokio::task::spawn_blocking(move || {
        tar_gz_decode(&data, dest.as_path())
    }))
    .await
}
