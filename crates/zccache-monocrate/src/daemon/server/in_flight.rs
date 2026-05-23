//! RAII guard for tracking in-flight artifact persistence bytes.

use super::*;

/// RAII guard that decrements `in_flight_bytes` on drop, even during panic unwind.
/// Prevents permanent counter inflation if a `spawn_blocking` task panics.
pub(super) struct InFlightGuard {
    pub(super) state: Arc<SharedState>,
    pub(super) size: usize,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.state
            .in_flight_bytes
            .fetch_sub(self.size, Ordering::Relaxed);
    }
}
