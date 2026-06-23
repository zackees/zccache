//! Shared tar+gzip helpers used by `gha-cache` and `rust-plan` backends.

use crate::core::NormalizedPath;
use std::path::Path;

async fn join_io<T>(
    handle: tokio::task::JoinHandle<Result<T, std::io::Error>>,
) -> Result<T, std::io::Error> {
    handle
        .await
        .map_err(|err| std::io::Error::other(format!("blocking archive task failed: {err}")))?
}

/// Create a tar.gz archive from a directory path.
pub(crate) fn tar_gz_encode(src: &Path) -> Result<Vec<u8>, std::io::Error> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let buf = Vec::new();
    let enc = GzEncoder::new(buf, Compression::fast());
    let mut tar = tar::Builder::new(enc);
    // Use the last component of the path as the archive prefix so that
    // extraction recreates the directory structure relative to the target.
    let prefix = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    tar.append_dir_all(&prefix, src)?;
    let enc = tar.into_inner()?;
    enc.finish()
}

/// Create a tar.gz archive from a directory path on Tokio's blocking pool.
pub(crate) async fn tar_gz_encode_async(src: NormalizedPath) -> Result<Vec<u8>, std::io::Error> {
    join_io(tokio::task::spawn_blocking(move || {
        tar_gz_encode(src.as_path())
    }))
    .await
}

/// Extract a tar.gz archive into a destination directory.
pub(crate) fn tar_gz_decode(data: &[u8], dest: &Path) -> Result<(), std::io::Error> {
    use flate2::read::GzDecoder;

    let dec = GzDecoder::new(data);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dest)
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
