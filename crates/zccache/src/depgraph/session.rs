//! Session management for the dependency graph.
//!
//! A session represents a build client connection to the daemon. Each session
//! tracks which compilation contexts it has registered, the client's PID
//! (for dead-man's switch), and idle time for timeout-based cleanup.
//!
//! The graph itself survives across sessions â€” sessions are ephemeral
//! metadata about who is using the graph.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::core::NormalizedPath;
use dashmap::DashMap;

use super::context::ContextKey;

/// Per-session statistics tracker. Only allocated when the session opts in.
#[derive(Debug, Clone)]
pub struct SessionStatsTracker {
    /// Total compile requests in this session.
    pub compilations: u64,
    /// Cache hits.
    pub hits: u64,
    /// Cache misses.
    pub misses: u64,
    /// Non-cacheable invocations.
    pub non_cacheable: u64,
    /// Compilations with non-zero exit.
    pub errors: u64,
    /// Estimated time saved in nanoseconds.
    pub time_saved_ns: u64,
    /// Distinct source files compiled.
    pub sources: HashSet<NormalizedPath>,
    /// Artifact bytes served from cache.
    pub bytes_read: u64,
    /// Artifact bytes stored into cache.
    pub bytes_written: u64,
}

/// Finalized session statistics (plain data, no sets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedSessionStats {
    pub duration_ms: u64,
    pub compilations: u64,
    pub hits: u64,
    pub misses: u64,
    pub non_cacheable: u64,
    pub errors: u64,
    pub time_saved_ms: u64,
    pub unique_sources: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

impl SessionStatsTracker {
    /// Create a new tracker with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compilations: 0,
            hits: 0,
            misses: 0,
            non_cacheable: 0,
            errors: 0,
            time_saved_ns: 0,
            sources: HashSet::new(),
            bytes_read: 0,
            bytes_written: 0,
        }
    }

    /// Record a cache hit.
    pub fn record_hit(&mut self, source: NormalizedPath, saved_ns: u64, bytes: u64) {
        self.compilations += 1;
        self.hits += 1;
        self.time_saved_ns += saved_ns;
        self.sources.insert(source);
        self.bytes_read += bytes;
    }

    /// Record a cache miss.
    pub fn record_miss(&mut self, source: NormalizedPath, bytes: u64) {
        self.compilations += 1;
        self.misses += 1;
        self.sources.insert(source);
        self.bytes_written += bytes;
    }

    /// Record a non-cacheable invocation.
    pub fn record_non_cacheable(&mut self) {
        self.compilations += 1;
        self.non_cacheable += 1;
    }

    /// Record a compile error.
    pub fn record_error(&mut self) {
        self.errors += 1;
    }

    /// Finalize into a plain stats struct given the session's creation time.
    #[must_use]
    pub fn finalize(&self, created_at: Instant) -> FinalizedSessionStats {
        FinalizedSessionStats {
            duration_ms: created_at.elapsed().as_millis() as u64,
            compilations: self.compilations,
            hits: self.hits,
            misses: self.misses,
            non_cacheable: self.non_cacheable,
            errors: self.errors,
            time_saved_ms: self.time_saved_ns / 1_000_000,
            unique_sources: self.sources.len() as u64,
            bytes_read: self.bytes_read,
            bytes_written: self.bytes_written,
        }
    }
}

impl Default for SessionStatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Unique identifier for a session (UUID v4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(uuid::Uuid);

impl SessionId {
    /// Create a new random session ID.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(uuid::Uuid::parse_str(s)?))
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Configuration for creating a new session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// PID of the client process (for dead-man's switch).
    pub client_pid: u32,
    /// Working directory of the client.
    pub working_dir: NormalizedPath,
    /// Optional log file path for session-scoped logging.
    pub log_file: Option<NormalizedPath>,
    /// Whether to track per-session statistics.
    pub track_stats: bool,
    /// Path for per-session JSONL compile journal (must end in .jsonl).
    pub journal_path: Option<NormalizedPath>,
    /// Issue #256: opt in to the extended journal schema. When true,
    /// the daemon populates crate_name, crate_type, output_ext, and
    /// self_profile_ns on every compile-journal line for this session.
    pub profile: bool,
}

/// An active session.
#[derive(Debug, Clone)]
pub struct Session {
    /// Unique session identifier.
    pub id: SessionId,
    /// PID of the client process.
    pub client_pid: u32,
    /// Client's working directory.
    pub working_dir: NormalizedPath,
    /// Optional log file path for session-scoped logging.
    pub log_file: Option<NormalizedPath>,
    /// Context keys registered by this session.
    pub context_keys: HashSet<ContextKey>,
    /// When the session was created.
    pub created_at: Instant,
    /// When the session was last active (compile request or heartbeat).
    pub last_activity: Instant,
    /// Per-session stats tracker (only present when opted in).
    pub stats_tracker: Option<SessionStatsTracker>,
    /// Path to the per-session JSONL journal file (if journal was requested).
    pub journal_path: Option<NormalizedPath>,
    /// Issue #256: extended-journal opt-in. See SessionConfig::profile.
    pub profile: bool,
}

/// Manages active sessions.
///
/// Thread-safe via `DashMap`. The daemon uses this to track which build
/// clients are connected and clean up expired sessions.
pub struct SessionManager {
    sessions: DashMap<SessionId, Session>,
    idle_timeout: Duration,
}

impl SessionManager {
    /// Create a new session manager with the given idle timeout.
    #[must_use]
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            sessions: DashMap::new(),
            idle_timeout,
        }
    }

    /// Create a new session. Returns the session ID.
    pub fn create(&self, config: SessionConfig) -> SessionId {
        let id = SessionId::new();
        let now = Instant::now();

        let stats_tracker = if config.track_stats {
            Some(SessionStatsTracker::new())
        } else {
            None
        };

        let journal_path = config.journal_path;

        let session = Session {
            id,
            client_pid: config.client_pid,
            working_dir: config.working_dir,
            log_file: config.log_file,
            context_keys: HashSet::new(),
            created_at: now,
            last_activity: now,
            stats_tracker,
            journal_path,
            profile: config.profile,
        };

        self.sessions.insert(id, session);
        id
    }

    /// End a session explicitly. Returns the removed session if it existed.
    pub fn end(&self, id: &SessionId) -> Option<Session> {
        self.sessions.remove(id).map(|(_, s)| s)
    }

    /// Update the last activity timestamp for a session.
    /// Returns `false` if the session doesn't exist.
    pub fn touch(&self, id: &SessionId) -> bool {
        if let Some(mut session) = self.sessions.get_mut(id) {
            session.last_activity = Instant::now();
            true
        } else {
            false
        }
    }

    /// Register a context key with a session.
    /// Returns `false` if the session doesn't exist.
    pub fn add_context(&self, id: &SessionId, key: ContextKey) -> bool {
        if let Some(mut session) = self.sessions.get_mut(id) {
            session.context_keys.insert(key);
            session.last_activity = Instant::now();
            true
        } else {
            false
        }
    }

    /// Get a snapshot of a session.
    #[must_use]
    pub fn get(&self, id: &SessionId) -> Option<Session> {
        self.sessions.get(id).map(|s| s.clone())
    }

    /// Get a session's working directory.
    #[must_use]
    pub fn working_dir(&self, id: &SessionId) -> Option<NormalizedPath> {
        self.sessions.get(id).map(|s| s.working_dir.clone())
    }

    /// Get the number of context keys in a session.
    #[must_use]
    pub fn context_count(&self, id: &SessionId) -> Option<usize> {
        self.sessions.get(id).map(|s| s.context_keys.len())
    }

    /// Remove sessions that have been idle longer than the timeout.
    /// Returns the removed sessions.
    pub fn cleanup_expired(&self) -> Vec<Session> {
        let cutoff = match Instant::now().checked_sub(self.idle_timeout) {
            Some(c) => c,
            None => return Vec::new(), // timeout exceeds uptime; nothing can be expired
        };
        let mut expired = Vec::new();

        self.sessions.retain(|_, session| {
            if session.last_activity < cutoff {
                expired.push(session.clone());
                false
            } else {
                true
            }
        });

        expired
    }

    /// Remove sessions whose client PID is no longer alive.
    /// The caller provides a function that checks if a PID is alive.
    pub fn cleanup_dead_pids<F>(&self, is_alive: F) -> Vec<Session>
    where
        F: Fn(u32) -> bool,
    {
        let mut dead = Vec::new();

        self.sessions.retain(|_, session| {
            if is_alive(session.client_pid) {
                true
            } else {
                dead.push(session.clone());
                false
            }
        });

        dead
    }

    /// Mutate a session in-place. Returns `false` if the session doesn't exist.
    pub fn mutate<F>(&self, id: &SessionId, f: F) -> bool
    where
        F: FnOnce(&mut Session),
    {
        if let Some(mut session) = self.sessions.get_mut(id) {
            f(&mut session);
            true
        } else {
            false
        }
    }

    /// Number of active sessions.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    /// Check if a session exists.
    #[must_use]
    pub fn exists(&self, id: &SessionId) -> bool {
        self.sessions.contains_key(id)
    }

    /// Get a snapshot of all active session IDs.
    #[must_use]
    pub fn active_ids(&self) -> Vec<SessionId> {
        self.sessions.iter().map(|e| *e.key()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SessionConfig {
        SessionConfig {
            client_pid: 1234,
            working_dir: "/home/user/project".into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
        }
    }

    #[test]
    fn create_session_returns_unique_ids() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id1 = mgr.create(test_config());
        let id2 = mgr.create(test_config());
        assert_ne!(id1, id2);
    }

    #[test]
    fn session_id_display() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(test_config());
        let display = format!("{id}");
        // UUID format: 8-4-4-4-12 hex digits
        assert_eq!(display.len(), 36);
        assert!(display.contains('-'));
    }

    #[test]
    fn session_id_roundtrip() {
        use std::str::FromStr;
        let id = SessionId::new();
        let s = id.to_string();
        let parsed = SessionId::from_str(&s).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn end_session_removes_it() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(test_config());
        assert_eq!(mgr.active_count(), 1);

        let session = mgr.end(&id).unwrap();
        assert_eq!(session.client_pid, 1234);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn end_nonexistent_returns_none() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        assert!(mgr.end(&SessionId::new()).is_none());
    }

    #[test]
    fn touch_updates_activity() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(test_config());
        assert!(mgr.touch(&id));
        assert!(!mgr.touch(&SessionId::new()));
    }

    #[test]
    fn add_context_to_session() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(test_config());

        let ctx = super::super::context::CompileContext {
            source_file: "/src/main.c".into(),
            include_search: super::super::search_paths::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = ctx.context_key();

        assert!(mgr.add_context(&id, key));
        assert_eq!(mgr.context_count(&id), Some(1));

        // Adding same key again doesn't increase count.
        assert!(mgr.add_context(&id, key));
        assert_eq!(mgr.context_count(&id), Some(1));
    }

    #[test]
    fn add_context_nonexistent_session_returns_false() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let ctx = super::super::context::CompileContext {
            source_file: "/src/main.c".into(),
            include_search: super::super::search_paths::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        assert!(!mgr.add_context(&SessionId::new(), ctx.context_key()));
    }

    #[test]
    fn cleanup_expired_removes_old_sessions() {
        let mgr = SessionManager::new(Duration::from_secs(1));
        let id = mgr.create(test_config());
        assert_eq!(mgr.active_count(), 1);

        // Artificially age the session.
        if let Some(mut s) = mgr.sessions.get_mut(&id) {
            s.last_activity = Instant::now() - Duration::from_secs(10);
        }

        let expired = mgr.cleanup_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].client_pid, 1234);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn cleanup_expired_keeps_active_sessions() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        mgr.create(test_config());
        let expired = mgr.cleanup_expired();
        assert!(expired.is_empty());
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn cleanup_dead_pids() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let mut config1 = test_config();
        config1.client_pid = 100;
        let mut config2 = test_config();
        config2.client_pid = 200;

        mgr.create(config1);
        mgr.create(config2);
        assert_eq!(mgr.active_count(), 2);

        // PID 100 is dead, PID 200 is alive.
        let dead = mgr.cleanup_dead_pids(|pid| pid != 100);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].client_pid, 100);
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn exists_check() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(test_config());
        assert!(mgr.exists(&id));
        assert!(!mgr.exists(&SessionId::new()));
    }

    #[test]
    fn active_ids_lists_all() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id1 = mgr.create(test_config());
        let id2 = mgr.create(test_config());
        let ids = mgr.active_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[test]
    fn concurrent_session_creation() {
        use std::sync::Arc;
        use std::thread;

        let mgr = Arc::new(SessionManager::new(Duration::from_secs(900)));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let mgr = Arc::clone(&mgr);
            handles.push(thread::spawn(move || {
                for _ in 0..25 {
                    mgr.create(test_config());
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(mgr.active_count(), 100);
    }

    // â”€â”€â”€ SessionStatsTracker tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn tracker_new_is_zero() {
        let t = SessionStatsTracker::new();
        assert_eq!(t.compilations, 0);
        assert_eq!(t.hits, 0);
        assert_eq!(t.misses, 0);
        assert_eq!(t.non_cacheable, 0);
        assert_eq!(t.errors, 0);
        assert_eq!(t.bytes_read, 0);
        assert_eq!(t.bytes_written, 0);
        assert!(t.sources.is_empty());
    }

    #[test]
    fn tracker_record_hit() {
        let mut t = SessionStatsTracker::new();
        t.record_hit("/src/a.c".into(), 5_000_000, 1024);
        t.record_hit("/src/a.c".into(), 3_000_000, 2048); // same source
        assert_eq!(t.compilations, 2);
        assert_eq!(t.hits, 2);
        assert_eq!(t.time_saved_ns, 8_000_000);
        assert_eq!(t.bytes_read, 3072);
        assert_eq!(t.sources.len(), 1); // deduplicated
    }

    #[test]
    fn tracker_record_miss() {
        let mut t = SessionStatsTracker::new();
        t.record_miss("/src/b.c".into(), 4096);
        assert_eq!(t.compilations, 1);
        assert_eq!(t.misses, 1);
        assert_eq!(t.bytes_written, 4096);
        assert_eq!(t.sources.len(), 1);
    }

    #[test]
    fn tracker_record_non_cacheable_and_error() {
        let mut t = SessionStatsTracker::new();
        t.record_non_cacheable();
        t.record_error();
        assert_eq!(t.compilations, 1);
        assert_eq!(t.non_cacheable, 1);
        assert_eq!(t.errors, 1);
    }

    #[test]
    fn tracker_finalize() {
        let mut t = SessionStatsTracker::new();
        t.record_hit("/src/a.c".into(), 5_000_000, 1024);
        t.record_miss("/src/b.c".into(), 2048);
        t.record_non_cacheable();

        let created_at = Instant::now() - Duration::from_millis(500);
        let f = t.finalize(created_at);

        assert!(f.duration_ms >= 500);
        assert_eq!(f.compilations, 3);
        assert_eq!(f.hits, 1);
        assert_eq!(f.misses, 1);
        assert_eq!(f.non_cacheable, 1);
        assert_eq!(f.time_saved_ms, 5); // 5_000_000ns / 1_000_000
        assert_eq!(f.unique_sources, 2);
        assert_eq!(f.bytes_read, 1024);
        assert_eq!(f.bytes_written, 2048);
    }

    #[test]
    fn session_with_stats_tracking() {
        let mut config = test_config();
        config.track_stats = true;
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(config);

        let session = mgr.get(&id).unwrap();
        assert!(session.stats_tracker.is_some());
    }

    #[test]
    fn session_without_stats_tracking() {
        let mgr = SessionManager::new(Duration::from_secs(900));
        let id = mgr.create(test_config()); // track_stats: false
        let session = mgr.get(&id).unwrap();
        assert!(session.stats_tracker.is_none());
    }
}
