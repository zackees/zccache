//! Durable blob-integrity sidecars shared by publication and transport.
//! Contents are the raw 32-byte BLAKE3 digest, never a process-local identity.

use std::path::Path;
use zccache_core::NormalizedPath;

/// Return the canonical sidecar name for a blob's final filename.
pub fn sidecar_path(blob: &Path) -> NormalizedPath {
    let name = blob.file_name().unwrap_or_default().to_string_lossy();
    let sidecar = format!(".cowhash-{}", kernal_api::hash::blake3_bytes(name.as_bytes()).to_hex());
    blob.parent().unwrap_or_else(|| Path::new(".")).join(sidecar).into()
}
