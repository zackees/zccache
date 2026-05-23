//! File I/O for the dependency-graph snapshot: save, load, and structured
//! classification of load outcomes for the daemon's startup path.

use std::path::Path;
use std::time::Duration;

use rkyv::Deserialize;
use zccache_core::NormalizedPath;

use crate::graph::DepGraph;

use super::{DepGraphSnapshot, SnapshotError, DEPGRAPH_MAGIC, DEPGRAPH_VERSION, HEADER_SIZE};

/// Entries older than this are trimmed before persisting the snapshot.
const GC_TTL: Duration = Duration::from_secs(86_400); // 1 day

/// Initial scratch-space size for rkyv serialization.
const SERIALIZE_SCRATCH: usize = 4096;

/// Returns the default path for the depgraph snapshot file.
#[must_use]
pub fn depgraph_file_path() -> NormalizedPath {
    zccache_core::config::depgraph_dir().join("depgraph.bin")
}

/// Save the dependency graph to disk with atomic write.
///
/// GC is applied first (1-day TTL) to avoid persisting stale entries.
pub fn save_to_file(graph: &DepGraph, path: &Path) -> Result<(), SnapshotError> {
    // GC: trim stale entries before saving.
    graph.trim(GC_TTL);

    let snapshot = graph.to_snapshot();

    let payload = rkyv::to_bytes::<_, SERIALIZE_SCRATCH>(&snapshot)
        .map_err(|e| SnapshotError::Corrupt(format!("serialize: {e}")))?;

    // Build header: magic + version (LE u32) + payload len (LE u64)
    let mut header = Vec::with_capacity(HEADER_SIZE);
    header.extend_from_slice(&DEPGRAPH_MAGIC);
    header.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    header.extend_from_slice(&(payload.len() as u64).to_le_bytes());

    // Atomic write: write to .tmp, then rename.
    let tmp_path = path.with_extension("bin.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    {
        let mut file = std::fs::File::create(&tmp_path)?;
        use std::io::Write;
        file.write_all(&header)?;
        file.write_all(&payload)?;
        file.flush()?;
    }

    // Windows: remove target before rename (rename doesn't overwrite on Windows).
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp_path, path)?;

    Ok(())
}

/// Outcome of attempting to load the persisted depgraph from a cache directory.
///
/// Returned by [`classify_load`] so the daemon can both seed its in-memory
/// graph and surface the load result to operators (stderr + `last-session.log`).
/// The variants mirror the failure modes the daemon must handle distinctly:
///
/// - `Loaded` — file present, magic + version + payload all valid; the graph
///   is ready to serve hits from the very first lookup.
/// - `Missing` — no `depgraph.bin` in the cache dir. Genuine cold start.
/// - `VersionMismatch` — file present but the embedded version tag does not
///   match this build. The on-disk format changed since the prior session.
/// - `Corrupt` — magic mismatch, truncated, or payload validation failed.
/// - `IoError` — any other I/O failure reading the file.
#[derive(Debug)]
pub enum DepGraphLoadOutcome {
    Loaded {
        graph: DepGraph,
    },
    Missing,
    VersionMismatch {
        file_version: u32,
        expected_version: u32,
    },
    Corrupt {
        message: String,
    },
    IoError {
        message: String,
    },
}

impl DepGraphLoadOutcome {
    /// Returns the loaded graph if this outcome is `Loaded`, else `None`.
    #[must_use]
    pub fn into_graph(self) -> Option<DepGraph> {
        match self {
            Self::Loaded { graph } => Some(graph),
            _ => None,
        }
    }

    /// Returns a human-readable warning message for non-`Loaded`, non-`Missing`
    /// outcomes. Used by the daemon to emit a clear notice on stderr AND in the
    /// per-session log so operators can see exactly why the warm-load failed
    /// and the session fell back to cold behavior.
    #[must_use]
    pub fn warning(&self, path: &Path) -> Option<String> {
        match self {
            Self::Loaded { .. } | Self::Missing => None,
            Self::VersionMismatch {
                file_version,
                expected_version,
            } => Some(format!(
                "warning: persisted depgraph at {} has version {file_version}, expected {expected_version}; treating session as cold",
                path.display()
            )),
            Self::Corrupt { message } => Some(format!(
                "warning: persisted depgraph at {} is corrupt ({message}); treating session as cold",
                path.display()
            )),
            Self::IoError { message } => Some(format!(
                "warning: failed to read persisted depgraph at {} ({message}); treating session as cold",
                path.display()
            )),
        }
    }
}

/// Classify a load attempt at `path` into a structured outcome.
///
/// This is the load-and-classify helper called by the daemon at startup so a
/// fresh session pointed at a populated cache dir is automatically treated as
/// warm — no caller-side opt-in required. See issue #320.
///
/// On non-`Loaded` outcomes the returned value carries enough information to
/// generate a stderr/session-log warning via [`DepGraphLoadOutcome::warning`].
#[must_use]
pub fn classify_load(path: &Path) -> DepGraphLoadOutcome {
    match load_from_file(path) {
        Ok(graph) => DepGraphLoadOutcome::Loaded { graph },
        Err(SnapshotError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
            DepGraphLoadOutcome::Missing
        }
        Err(SnapshotError::Io(e)) => DepGraphLoadOutcome::IoError {
            message: e.to_string(),
        },
        Err(SnapshotError::VersionMismatch { file, expected }) => {
            DepGraphLoadOutcome::VersionMismatch {
                file_version: file,
                expected_version: expected,
            }
        }
        Err(SnapshotError::BadMagic) => DepGraphLoadOutcome::Corrupt {
            message: "bad magic bytes".into(),
        },
        Err(SnapshotError::Corrupt(message)) => DepGraphLoadOutcome::Corrupt { message },
    }
}

/// Load the dependency graph from disk, validating header and payload.
pub fn load_from_file(path: &Path) -> Result<DepGraph, SnapshotError> {
    let data = std::fs::read(path)?;

    if data.len() < HEADER_SIZE {
        return Err(SnapshotError::Corrupt("file too small for header".into()));
    }

    // Validate magic.
    if data[0..4] != DEPGRAPH_MAGIC {
        return Err(SnapshotError::BadMagic);
    }

    // Validate version.
    let version = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    if version != DEPGRAPH_VERSION {
        return Err(SnapshotError::VersionMismatch {
            file: version,
            expected: DEPGRAPH_VERSION,
        });
    }

    // Validate payload length (use checked arithmetic to avoid overflow).
    let payload_len_u64 = u64::from_le_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    let payload_len: usize = usize::try_from(payload_len_u64).map_err(|_| {
        SnapshotError::Corrupt(format!("payload length too large: {payload_len_u64}"))
    })?;

    let available = data.len() - HEADER_SIZE;
    if available < payload_len {
        return Err(SnapshotError::Corrupt(format!(
            "truncated: expected {payload_len} payload bytes, got {available}",
        )));
    }

    let payload = &data[HEADER_SIZE..HEADER_SIZE + payload_len];

    // Validate and deserialize.
    let archived = rkyv::check_archived_root::<DepGraphSnapshot>(payload)
        .map_err(|e| SnapshotError::Corrupt(format!("validation: {e}")))?;

    let snapshot: DepGraphSnapshot = archived
        .deserialize(&mut rkyv::Infallible)
        .expect("infallible deserialization");

    Ok(DepGraph::from_snapshot(snapshot))
}
