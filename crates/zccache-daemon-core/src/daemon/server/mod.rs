//! Daemon server — accepts IPC connections and handles requests.
//!
//! `mod.rs` is intentionally thin: the heavy logic lives in topic-focused
//! submodules under `server/`. This file defines `DaemonServer` itself, a
//! handful of constants shared by siblings, and the module wiring (declare +
//! `use ...::*` re-exports) that lets every submodule use `use super::*;` to
//! see the common type vocabulary.

use crate::artifact::{ArtifactIndex, ArtifactStore};
use crate::core::NormalizedPath;
use crate::depgraph::{
    CompileContext, ContextKey, DepGraph, DepfileStrategy, SessionId, SessionManager,
    SystemIncludeCache, UserDepFlags,
};
use crate::fscache::{CacheSystem, Clock};
use crate::hash::ContentHash;
use crate::ipc::{IpcConnection, IpcListener};
use crate::protocol::{ArtifactData, ArtifactOutput, ArtifactPayload, Request, Response};
use crate::watcher::{NotifyWatcher, SettleBuffer, SettledEvent};
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

use super::compile_journal::{
    extract_outcome, miss_reason, CompileJournal, JournalContext, JournalEntry, SelfProfileSpans,
};
use super::fingerprint::FingerprintManager;
use super::process::CompilePriority;
use super::stats::{HitPhases, MissPhases, PhaseProfiler, StatsCollector};

/// Cached result of a verified cache hit, enabling zero-hash fast path.
///
/// When the journal clock hasn't advanced since the last verified hit for a
/// context, we can skip all stat/hash/depgraph work and jump straight to
/// artifact lookup.
pub(crate) struct FastHitEntry {
    pub(crate) clock: Clock,
    pub(crate) artifact_key_hex: String,
    pub(crate) cached_at: std::time::Instant,
}

/// Maximum age for fast-hit cache entries. Matches the High→Medium confidence
/// decay in the metadata cache. Without watcher events, entries expire and
/// fall through to the stat-verify slow path. Set to 60s because the watcher
/// + journal provide real invalidation — this timer is just a safety net.
const FAST_HIT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);
const EPHEMERAL_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(300);
const REQUEST_CACHE_MAX_ENTRIES: usize = 4096;
/// Validation-cache entries are lightweight (~200 bytes: ContentHash + root +
/// artifact_key hex + clock) vs request_cache's parsed invocation metadata
/// (multiple KiB per entry). Sized 2× the request cache so sibling-build /
/// multi-root scenarios don't evict good validation entries while the request
/// cache is still sparse. Memory cost at the cap: ~1.6 MiB. (#453)
const REQUEST_VALIDATION_CACHE_MAX_ENTRIES: usize = 8192;
const RSP_CACHE_MAX_ENTRIES: usize = 1024;
const RUST_MISS_PROFILE_ENV: &str = "ZCCACHE_PROFILE_RUST_MISS";
const CC_MISS_PROFILE_ENV: &str = "ZCCACHE_PROFILE_CC_MISS";
const WORKTREE_ROOT_ENV: &str = "ZCCACHE_WORKTREE_ROOT";
const PATH_REMAP_ENV: &str = "ZCCACHE_PATH_REMAP";
const REQUEST_ROOT_MARKER: &str = "$ZCCACHE_WORKTREE_ROOT";
const LINK_PATH_REMAP_AUTO_KEY_FLAG: &str = "zccache:path-remap=auto";
const LINK_PATH_REMAP_ROOT_SPECIFIC_FLAG: &str = "zccache:path-remap=root-specific";

pub use loaders::DepGraphSetter;

/// The daemon server that listens for IPC connections.
pub struct DaemonServer {
    listener: IpcListener,
    shutdown: Arc<Notify>,
    state: Arc<SharedState>,
    /// Receiver for the background index-writer task. Taken in `run()`.
    index_writer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<IndexWriterCommand>>,
}

/// In-process daemon engine used by the public embedded API.
///
/// This owns the same [`SharedState`] as the IPC daemon without binding or
/// accepting an [`IpcListener`].
pub(crate) struct EmbeddedDaemon {
    state: Arc<SharedState>,
    index_writer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<IndexWriterCommand>>,
    index_writer_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub(crate) struct EmbeddedCompileRequest {
    pub(crate) compiler: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Option<Vec<(String, String)>>,
    pub(crate) stdin: Vec<u8>,
}

pub(crate) struct EmbeddedCompileResult {
    pub(crate) exit_code: i32,
    pub(crate) stdout: Arc<Vec<u8>>,
    pub(crate) stderr: Arc<Vec<u8>>,
    pub(crate) cached: bool,
}

pub(crate) struct EmbeddedStatsSnapshot {
    pub(crate) status: crate::protocol::DaemonStatus,
    pub(crate) phase_profile: crate::protocol::PhaseProfileSummary,
}

pub(crate) struct EmbeddedFlushReport {
    pub(crate) pending_writes_drained: bool,
    pub(crate) artifact_entries: u64,
    pub(crate) metadata_entries: u64,
}

mod cache_trim;
mod cached_artifact;
mod client_env;
mod compile_concurrency;
mod compiler_hash;
mod connection;
mod embedded;
mod handle_clear;
mod handle_compile;
mod handle_compile_ephemeral;
mod handle_compile_multi;
mod handle_exec;
mod handle_exec_probe;
mod handle_link;
mod handle_release_worktree_handles;
mod in_flight;
mod inner_trace;
mod keys;
mod lifecycle;
mod link_helpers;
mod loaders;
mod pch;
mod pending_writes;
pub(crate) mod persist;
mod private_daemon;
mod request_cache;
mod rsp_cache;
mod run;
mod rustc;
mod session;
mod staged_materialize;
mod staged_publish;
mod state;
mod util;
mod wal;
mod watch;

use cache_trim::*;
use cached_artifact::*;
pub(crate) use cached_artifact::{CachedArtifact, CachedPayload};
use client_env::*;
use compiler_hash::*;
use connection::handle_connection;
use handle_clear::*;
use handle_compile::handle_compile;
use handle_compile_ephemeral::*;
use handle_compile_multi::handle_compile_multi;
use handle_exec::handle_generic_tool_exec;
use handle_link::handle_link_ephemeral;
#[cfg(test)]
use handle_link::run_post_link_deploy_hook;
use handle_release_worktree_handles::handle_release_worktree_handles;
use in_flight::*;
use keys::*;
use lifecycle::*;
use link_helpers::*;
use pch::*;
use persist::*;

pub(crate) fn remove_cow_blob(path: &Path) -> std::io::Result<()> {
    remove_registered_blob(path)
}
use private_daemon::*;
use request_cache::*;
use rsp_cache::*;
use rustc::*;
use session::*;
use staged_materialize::*;
use staged_publish::*;
use state::*;
use util::*;
use wal::*;
use watch::*;

#[cfg(test)]
mod tests;
