//! Disk persistence for the dependency graph via rkyv zero-copy serialization.
//!
//! Saves/loads the graph to `~/.zccache/depgraph/depgraph.bin` so warm contexts
//! survive daemon restarts and cache hits resume immediately.
//!
//! Split into focused submodules so each file stays under 1,000 LOC:
//! - this file: snapshot types, error type, [`DepGraph::to_snapshot`] /
//!   [`DepGraph::from_snapshot`] conversion methods, and the tiny
//!   `paths_to_strings` / `strings_to_paths` helpers used by both
//!   conversion and tests.
//! - [`persistence`]: file I/O — [`save_to_file`], [`load_from_file`],
//!   [`classify_load`], [`depgraph_file_path`], and [`DepGraphLoadOutcome`].
//! - [`tests`] (cfg(test) only): split per concern — roundtrip, persistence,
//!   behavioral.

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rayon::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use zccache::core::NormalizedPath;
use zccache::hash::ContentHash;

use super::super::context::{ArtifactKey, CompileContext, ContextKey};
use super::super::graph::{ContextEntry, ContextState, DepGraph, FileEntry};
use super::super::scanner::{IncludeDirective, IncludeKind};
use super::super::search_paths::IncludeSearchPaths;

mod persistence;
#[cfg(test)]
mod tests;

pub use persistence::{
    classify_load, depgraph_file_path, load_from_file, save_to_file, DepGraphLoadOutcome,
};

/// On-disk format version. Bump when snapshot layout changes.
pub const DEPGRAPH_VERSION: u32 = 4;

/// Magic bytes identifying a depgraph snapshot file ("ZCDG").
pub const DEPGRAPH_MAGIC: [u8; 4] = [0x5A, 0x43, 0x44, 0x47];

/// Header size: 4 (magic) + 4 (version) + 8 (payload len) = 16 bytes.
pub(crate) const HEADER_SIZE: usize = 16;

// ---------------------------------------------------------------------------
// Snapshot types (rkyv-serializable mirrors of the in-memory types)
// ---------------------------------------------------------------------------

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct DepGraphSnapshot {
    pub files: Vec<FileEntrySnapshot>,
    pub contexts: Vec<ContextEntrySnapshot>,
    pub stats: SnapshotStats,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct FileEntrySnapshot {
    pub path: String,
    pub includes: Vec<IncludeDirectiveSnapshot>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct IncludeDirectiveSnapshot {
    /// 0=Quoted, 1=AngleBracket, 2=Computed
    pub kind: u8,
    pub path: String,
    pub line: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct ContextEntrySnapshot {
    pub context_key: [u8; 32],
    pub key_root: Option<String>,
    pub source_file: String,
    pub iquote: Vec<String>,
    pub user: Vec<String>,
    pub system: Vec<String>,
    pub after: Vec<String>,
    pub defines: Vec<String>,
    pub flags: Vec<String>,
    pub force_includes: Vec<String>,
    pub unknown_flags: Vec<String>,
    pub resolved_includes: Vec<String>,
    pub unresolved_includes: Vec<String>,
    pub has_computed_includes: bool,
    pub artifact_key: Option<[u8; 32]>,
    pub last_file_hashes: Vec<(String, [u8; 32])>,
    /// 0=Cold, 1=Warm, 2=Stale
    pub state: u8,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct SnapshotStats {
    pub saved_at_epoch_secs: u64,
    pub file_count: u64,
    pub context_count: u64,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad magic bytes in depgraph file")]
    BadMagic,

    #[error("depgraph version mismatch: file has v{file}, expected v{expected}")]
    VersionMismatch { file: u32, expected: u32 },

    #[error("corrupt depgraph file: {0}")]
    Corrupt(String),
}

// ---------------------------------------------------------------------------
// Conversion: DepGraph <-> Snapshot
// ---------------------------------------------------------------------------

impl DepGraph {
    /// Create a serializable snapshot of the current graph state.
    pub fn to_snapshot(&self) -> DepGraphSnapshot {
        let files: Vec<FileEntrySnapshot> = self
            .files_iter()
            .map(|entry| {
                let path = entry.key().to_string_lossy().into_owned();
                let includes = entry
                    .value()
                    .includes
                    .iter()
                    .map(|d| IncludeDirectiveSnapshot {
                        kind: match &d.kind {
                            IncludeKind::Quoted => 0,
                            IncludeKind::AngleBracket => 1,
                            IncludeKind::Computed(_) => 2,
                        },
                        path: d.path.clone(),
                        line: d.line,
                    })
                    .collect();
                FileEntrySnapshot { path, includes }
            })
            .collect();

        let contexts: Vec<ContextEntrySnapshot> = self
            .contexts_iter()
            .map(|entry| {
                let key = entry.key();
                let ctx = entry.value();
                ContextEntrySnapshot {
                    context_key: *key.hash().as_bytes(),
                    key_root: ctx
                        .key_root
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    source_file: ctx.context.source_file.to_string_lossy().into_owned(),
                    iquote: paths_to_strings(&ctx.context.include_search.iquote),
                    user: paths_to_strings(&ctx.context.include_search.user),
                    system: paths_to_strings(&ctx.context.include_search.system),
                    after: paths_to_strings(&ctx.context.include_search.after),
                    defines: ctx.context.defines.clone(),
                    flags: ctx.context.flags.clone(),
                    force_includes: paths_to_strings(&ctx.context.force_includes),
                    unknown_flags: ctx.context.unknown_flags.clone(),
                    resolved_includes: paths_to_strings(&ctx.resolved_includes),
                    unresolved_includes: ctx.unresolved_includes.clone(),
                    has_computed_includes: ctx.has_computed_includes,
                    artifact_key: ctx.artifact_key.map(|k| *k.hash().as_bytes()),
                    last_file_hashes: ctx
                        .last_file_hashes
                        .iter()
                        .map(|(p, h)| (p.to_string_lossy().into_owned(), *h.as_bytes()))
                        .collect(),
                    state: match ctx.state {
                        ContextState::Cold => 0,
                        ContextState::Warm => 1,
                        ContextState::Stale => 2,
                    },
                }
            })
            .collect();

        DepGraphSnapshot {
            stats: SnapshotStats {
                saved_at_epoch_secs: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                file_count: files.len() as u64,
                context_count: contexts.len() as u64,
            },
            files,
            contexts,
        }
    }

    /// Reconstruct a `DepGraph` from a deserialized snapshot.
    pub fn from_snapshot(snap: DepGraphSnapshot) -> Self {
        let files: DashMap<NormalizedPath, FileEntry> = DashMap::new();
        snap.files.into_par_iter().for_each(|f| {
            let path = NormalizedPath::from(f.path.as_str());
            let includes = f
                .includes
                .into_iter()
                .map(|d| {
                    let kind = match d.kind {
                        0 => IncludeKind::Quoted,
                        1 => IncludeKind::AngleBracket,
                        _ => IncludeKind::Computed(d.path.clone()),
                    };
                    IncludeDirective {
                        kind,
                        path: d.path,
                        line: d.line,
                    }
                })
                .collect();
            files.insert(
                path,
                FileEntry {
                    includes,
                    scanned_at: Instant::now(),
                },
            );
        });

        let contexts: DashMap<ContextKey, ContextEntry> = DashMap::new();
        snap.contexts.into_par_iter().for_each(|c| {
            let key = ContextKey::from_raw(c.context_key);
            let context = CompileContext {
                source_file: NormalizedPath::from(c.source_file.as_str()),
                include_search: IncludeSearchPaths {
                    iquote: strings_to_paths(c.iquote),
                    user: strings_to_paths(c.user),
                    system: strings_to_paths(c.system),
                    after: strings_to_paths(c.after),
                },
                defines: c.defines,
                flags: c.flags,
                force_includes: strings_to_paths(c.force_includes),
                unknown_flags: c.unknown_flags,
            };
            let entry = ContextEntry {
                context,
                key_root: c.key_root.map(|root| NormalizedPath::from(root.as_str())),
                resolved_includes: strings_to_paths(c.resolved_includes),
                unresolved_includes: c.unresolved_includes,
                has_computed_includes: c.has_computed_includes,
                artifact_key: c.artifact_key.map(ArtifactKey::from_raw),
                last_file_hashes: c
                    .last_file_hashes
                    .into_iter()
                    .map(|(p, h)| (NormalizedPath::from(p.as_str()), ContentHash::from_bytes(h)))
                    .collect(),
                last_accessed: Instant::now(),
                state: match c.state {
                    0 => ContextState::Cold,
                    1 => ContextState::Warm,
                    _ => ContextState::Stale,
                },
            };
            contexts.insert(key, entry);
        });

        DepGraph::from_maps(files, contexts)
    }
}

pub(crate) fn paths_to_strings<P: AsRef<Path>>(paths: &[P]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.as_ref().to_string_lossy().into_owned())
        .collect()
}

pub(crate) fn strings_to_paths(strings: Vec<String>) -> Vec<NormalizedPath> {
    strings.into_iter().map(NormalizedPath::from).collect()
}
