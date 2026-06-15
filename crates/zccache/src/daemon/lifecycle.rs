//! Daemon lifecycle event log — thin re-export of
//! [`crate::core::lifecycle`].
//!
//! The writer was extracted to `zccache-core` so the CLI can also
//! append lifecycle events (notably `spawn-attempt` records that
//! capture *why* a daemon respawn was triggered). See issue #323 for
//! the diagnostic gap that motivated the move. Daemon-side code
//! continues to address this as `super::lifecycle::...` for
//! readability; this module exists only to forward those references.

pub use crate::core::lifecycle::{
    client_meta, emit_takeover_lifecycle_events, is_live_lifecycle_log_name, live_log_filename,
    log_file_path, write_event, CAUSE_COMM_ERROR, CAUSE_PIPE_CLOSED_MID_WRITE,
    CAUSE_REPLACED_BY_OTHER_VERSION, CAUSE_TIMEOUT, EVENT_CLIENT_DISCONNECTED, EVENT_DAEMON_DIED,
    EVENT_DIED_IDLE, EVENT_DIED_SHUTDOWN, EVENT_PIPE_HANDOVER, EVENT_SPAWN, EVENT_SPAWN_ATTEMPT,
    EVENT_VERSION_MISMATCH, LIVE_LOG_FILENAME, MAX_LOG_SIZE, REASON_FORCED_REPLACE,
    REASON_GRACEFUL_SHUTDOWN, REASON_IDLE_TIMEOUT, REASON_INITIAL_START, REASON_PIPE_STALE,
    REASON_PREVIOUS_DIED, REASON_REPLACED_COMM_ERROR, REASON_REPLACED_STALE_VERSION,
    REASON_REPLACED_UNREACHABLE, REASON_TAKEOVER,
};
