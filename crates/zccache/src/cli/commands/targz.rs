//! Shared tar+gzip helpers used by `gha-cache` and `rust-plan` backends.

use std::path::Path;

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

/// Extract a tar.gz archive into a destination directory.
pub(crate) fn tar_gz_decode(data: &[u8], dest: &Path) -> Result<(), std::io::Error> {
    use flate2::read::GzDecoder;

    let dec = GzDecoder::new(data);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dest)
}
