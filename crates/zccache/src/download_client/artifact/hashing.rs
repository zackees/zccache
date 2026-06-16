use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArtifactFingerprint {
    pub(super) sha256: String,
    pub(super) bytes: u64,
}

pub(super) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) fn compute_artifact_fingerprint(path: &Path) -> io::Result<ArtifactFingerprint> {
    let sha256 = sha256_file(path)?;
    let bytes = fs::metadata(path)?.len();
    Ok(ArtifactFingerprint { sha256, bytes })
}

pub(super) fn temp_download_path(destination: &Path) -> PathBuf {
    destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .map(|ext| format!("{}.", ext.to_string_lossy()))
            .unwrap_or_default()
    ))
}
