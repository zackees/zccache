//! Artifact cache protocol payloads.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zccache_core::NormalizedPath;
/// Result of a cache lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LookupResult {
    /// Cache hit.
    Hit {
        /// The cached artifact data.
        artifact: ArtifactData,
    },
    /// Cache miss.
    Miss,
}

/// Result of storing an artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StoreResult {
    /// Successfully stored.
    Stored,
    /// Already existed in cache.
    AlreadyExists,
}

/// Artifact data exchanged over the protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactData {
    /// The output files (filename to contents).
    pub outputs: Vec<ArtifactOutput>,
    /// Captured stdout from the compiler.
    pub stdout: Arc<Vec<u8>>,
    /// Captured stderr from the compiler.
    pub stderr: Arc<Vec<u8>>,
    /// Compiler exit code.
    pub exit_code: i32,
}

/// Where an artifact output's bytes live on the daemon's filesystem at the
/// moment a request is built.
///
/// `Bytes` is the only variant any current client emits — `Path` is reserved
/// for future sccache-emulation paths where the client already has the bytes
/// on disk and the daemon can hardlink directly via `persist_artifact_file`
/// (falling back to copy on cross-volume failure).
///
/// The variant was introduced pre-emptively in PR for issue #296 so that
/// landing the eventual `Request::Store` handler won't require a second
/// `PROTOCOL_VERSION` bump. See `crates/zccache-daemon/src/server.rs` —
/// `CachedPayload` is the internal sibling of this type and predates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ArtifactPayload {
    /// Bytes shipped inline in the IPC message. Used by every current call
    /// site; future remote-daemon scenarios may also need this.
    Bytes(Arc<Vec<u8>>),
    /// Path on the daemon's filesystem. The daemon hardlinks from this path
    /// into the cache (falling back to copy on cross-volume failure). Path
    /// must be absolute and readable by the daemon process (same user).
    /// No current client emits this variant.
    Path(NormalizedPath),
}

impl ArtifactPayload {
    /// Size in bytes of the underlying output. For `Path`, stats the file;
    /// returns 0 on I/O error (matches the prior `unwrap_or_default()`
    /// semantics elsewhere in the daemon for missing-output cases).
    #[must_use]
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::Path(p) => std::fs::metadata(p.as_path()).map(|m| m.len()).unwrap_or(0),
        }
    }

    /// Returns `Some` of the inline bytes when this is the `Bytes` variant.
    /// Useful for daemon-internal sites that still want the byte path —
    /// `None` signals "the bytes live on disk; route through a hardlink/read
    /// helper instead."
    #[must_use]
    pub fn as_bytes(&self) -> Option<&Arc<Vec<u8>>> {
        match self {
            Self::Bytes(b) => Some(b),
            Self::Path(_) => None,
        }
    }
}

/// A single output file from compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactOutput {
    /// Relative filename (e.g., "foo.o").
    pub name: String,
    /// Where the bytes live — inline in the message or on disk for hardlink.
    /// See `ArtifactPayload` for the variant rationale.
    pub payload: ArtifactPayload,
}

/// Information about a cached Rust compilation artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RustArtifactInfo {
    /// Cache key hex.
    pub cache_key: String,
    /// Output file names (e.g., ["libfoo-abc123.rlib", "libfoo-abc123.rmeta", "foo-abc123.d"]).
    pub output_names: Vec<String>,
    /// Number of payload files.
    pub payload_count: usize,
}
