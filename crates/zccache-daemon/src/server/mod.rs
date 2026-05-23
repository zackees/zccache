//! Daemon server — accepts IPC connections and handles requests.
//!
//! `mod.rs` is intentionally thin: the heavy logic lives in topic-focused
//! submodules under `server/`. This file defines `DaemonServer` itself, a
//! handful of constants shared by siblings, and the module wiring (declare +
//! `use ...::*` re-exports) that lets every submodule use `use super::*;` to
//! see the common type vocabulary.

use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use zccache_artifact::{ArtifactIndex, ArtifactStore};
use zccache_monocrate::core::NormalizedPath;
use zccache_depgraph::{
    CompileContext, ContextKey, DepGraph, DepfileStrategy, SessionId, SessionManager,
    SystemIncludeCache, UserDepFlags,
};
use zccache_fscache::{CacheSystem, Clock};
use zccache_monocrate::hash::ContentHash;
use zccache_monocrate::ipc::{IpcConnection, IpcListener};
use zccache_monocrate::protocol::{ArtifactData, ArtifactOutput, ArtifactPayload, Request, Response};
use zccache_watcher::{NotifyWatcher, SettleBuffer, SettledEvent};

use crate::compile_journal::{extract_outcome, CompileJournal, JournalContext, JournalEntry};
use crate::fingerprint::FingerprintManager;
use crate::process::CompilePriority;
use crate::stats::{HitPhases, MissPhases, PhaseProfiler, StatsCollector};

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
const RSP_CACHE_MAX_ENTRIES: usize = 1024;
const RUST_MISS_PROFILE_ENV: &str = "ZCCACHE_PROFILE_RUST_MISS";
const WORKTREE_ROOT_ENV: &str = "ZCCACHE_WORKTREE_ROOT";
const PATH_REMAP_ENV: &str = "ZCCACHE_PATH_REMAP";
const REQUEST_ROOT_MARKER: &str = "$ZCCACHE_WORKTREE_ROOT";
const LINK_PATH_REMAP_AUTO_KEY_FLAG: &str = "zccache:path-remap=auto";
const LINK_PATH_REMAP_ROOT_SPECIFIC_FLAG: &str = "zccache:path-remap=root-specific";

/// The daemon server that listens for IPC connections.
pub struct DaemonServer {
    listener: IpcListener,
    shutdown: Arc<Notify>,
    state: Arc<SharedState>,
    /// Receiver for the background index-writer task. Taken in `run()`.
    index_writer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, ArtifactIndex)>>,
}

mod cache_trim;
mod cached_artifact;
mod client_env;
mod compiler_hash;
mod connection;
mod handle_clear;
mod handle_compile;
mod handle_compile_ephemeral;
mod handle_compile_multi;
mod handle_link;
mod in_flight;
mod keys;
mod lifecycle;
mod link_helpers;
mod pch;
mod persist;
mod request_cache;
mod rsp_cache;
mod run;
mod rustc;
mod session;
mod state;
mod util;
mod wal;
mod watch;

pub(crate) use cache_trim::trim_fast_hit_cache;
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
use handle_link::handle_link_ephemeral;
#[cfg(test)]
use handle_link::run_post_link_deploy_hook;
use in_flight::*;
use keys::*;
use lifecycle::*;
use link_helpers::*;
use pch::*;
use persist::*;
use request_cache::*;
use rsp_cache::*;
use rustc::*;
use session::*;
use state::*;
use util::*;
use wal::*;
use watch::*;

#[cfg(test)]
mod tests;
