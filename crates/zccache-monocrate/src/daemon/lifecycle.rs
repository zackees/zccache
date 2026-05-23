//! Daemon lifecycle event log — thin re-export of
//! [`zccache_monocrate::core::lifecycle`].
//!
//! The writer was extracted to `zccache-core` so the CLI can also
//! append lifecycle events (notably `spawn-attempt` records that
//! capture *why* a daemon respawn was triggered). See issue #323 for
//! the diagnostic gap that motivated the move. Daemon-side code
//! continues to address this as `super::lifecycle::...` for
//! readability; this module exists only to forward those references.

pub use zccache_monocrate::core::lifecycle::{
    log_file_path, write_event, EVENT_DIED_IDLE, EVENT_DIED_SHUTDOWN, EVENT_SPAWN,
    EVENT_SPAWN_ATTEMPT, EVENT_VERSION_MISMATCH, LIVE_LOG_FILENAME, MAX_LOG_SIZE,
    REASON_GRACEFUL_SHUTDOWN, REASON_IDLE_TIMEOUT, REASON_INITIAL_START,
    REASON_REPLACED_COMM_ERROR, REASON_REPLACED_STALE_VERSION, REASON_REPLACED_UNREACHABLE,
};
