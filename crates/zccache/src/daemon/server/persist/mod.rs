//! Artifact pack format, atomic writes, hardlinking, cached output materialization.
//!
//! Originally a single 1K-LOC `persist.rs`; split per domain so each file
//! stays well under the 1,000 LOC cap. Submodules:
//!
//! - [`artifact_io`] — Atomic writes (`persist_artifact_output`,
//!   `persist_artifact_file`, `replace_artifact_cache_file`),
//!   `PersistArtifactFileStats`, error enrichment, and the Windows
//!   AV-scanner retry helper.
//! - [`pack`] — Experimental `.pack` artifact format (env-gated via
//!   `ZCCACHE_PACK_ARTIFACTS`): magic header, builder, parser, and
//!   per-payload extractor.
//! - [`write_cached`] — Materialize cached output to its target path
//!   (`write_cached_output`, `write_cached_file`, `write_cached_payload`,
//!   and the parallel batch entry points).
//! - [`hardlink`] — Cross-platform hardlink helpers
//!   (`break_output_hardlink_before_compile`, `hard_link_count`,
//!   `same_file`, Windows `get_file_id`).
//! - [`mtime`] — Mtime preservation + sibling-floor refinement
//!   (`touch_mtime`, `floor_materialized_outputs_to_input_max`).
//!
//! All `pub(super)` items are re-exported here so the parent `use
//! persist::*;` glob still sees the original surface.

use super::*;

mod artifact_io;
mod hardlink;
mod mtime;
mod pack;
mod write_cached;

pub(in crate::daemon::server) use artifact_io::*;
pub(in crate::daemon::server) use hardlink::*;
pub(in crate::daemon::server) use mtime::*;
pub(in crate::daemon::server) use pack::*;
pub(in crate::daemon::server) use write_cached::*;

#[cfg(test)]
mod tests;
