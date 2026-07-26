//! Issue #1216 — compile-queue visibility.
//!
//! Under contention a `Compile` request can sit for minutes inside
//! `Semaphore::acquire_owned().await` (see
//! [`super::compile_concurrency`]). Before this module that wait was
//! completely silent: the wrapper did exactly one blocking `recv` with a
//! 180 s wedge budget, so a legitimately-queued compile was
//! indistinguishable from a hung daemon and got the #753/#955 wedge
//! treatment — probe, then either an ephemeral re-run that threw away the
//! daemon's in-progress work or a kill.
//!
//! Two pieces live here:
//!
//! - [`CompileQueueGauge`] — the daemon-global counters the semaphore
//!   itself cannot report. `tokio::sync::Semaphore` exposes
//!   `available_permits()` but no waiter count, and the initial capacity is
//!   unrecoverable once permits are handed out. Every gated compile
//!   registers via [`CompileQueueGauge::enqueue`] and calls
//!   [`CompileQueueGuard::admit`] the moment its permit is granted.
//! - [`CompileProgressSlot`] — the *per-request* view (this request's
//!   queue ticket), published through a task-local so the connection layer
//!   can build a heartbeat frame without plumbing a progress handle through
//!   `handle_compile` → `pipeline` → `compile_exec`.
//!
//! ## Why a task-local
//!
//! The heartbeat is emitted by the connection layer
//! ([`super::connection::guarded_dispatch_with_progress`]) because that is
//! the only place holding the `IpcConnection`. The queue ticket, however, is
//! only known deep inside the compile pipeline. The compile handler is
//! `await`ed inline by `guarded_dispatch` — never spawned — so both ends run
//! on the same tokio task and a task-local scoped around the handler
//! future reaches the acquire site without touching five function
//! signatures. If the ticket is ever taken on a *different* task the
//! task-local lookup simply misses and the heartbeat reports position 0 —
//! degraded, never wrong.
//!
//! ## Queue position semantics
//!
//! Tickets are monotonic and `admitted` counts how many tickets have left
//! the queue (granted a permit, or cancelled — a cancelled waiter still
//! advances the cursor so the requests behind it do not stall). tokio's
//! semaphore is FIFO-fair, so `ticket - admitted` is the number of compiles
//! still ahead of this one.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::protocol::Response;

/// Phase label reported while the request is waiting for a permit.
pub(in crate::daemon) const PHASE_QUEUED: &str = "queued";

/// Phase label reported once the request holds a permit (or the
/// concurrency cap is disabled entirely).
pub(in crate::daemon) const PHASE_COMPILING: &str = "compiling";

/// Sentinel for "this request has not reached the compile gate yet".
const NO_TICKET: u64 = u64::MAX;

/// Daemon-global compile-queue counters.
///
/// Cheap relaxed atomics — these are diagnostics on the heartbeat path,
/// never a correctness gate, so no ordering stronger than `Relaxed` is
/// warranted.
#[derive(Debug, Default)]
pub(in crate::daemon) struct CompileQueueGauge {
    waiting: AtomicU32,
    in_flight: AtomicU32,
    next_ticket: AtomicU64,
    admitted: AtomicU64,
}

impl CompileQueueGauge {
    /// Register a new waiter at the compile gate.
    ///
    /// Call immediately *before* awaiting the permit; call
    /// [`CompileQueueGuard::admit`] immediately after it is granted. The
    /// returned guard restores the counters on drop, including the
    /// cancellation path (client disconnect, handler dropped mid-await).
    pub(in crate::daemon) fn enqueue(self: &Arc<Self>) -> CompileQueueGuard {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        self.waiting.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = current_slot() {
            slot.ticket.store(ticket, Ordering::Relaxed);
            slot.admitted.store(false, Ordering::Relaxed);
        }
        CompileQueueGuard {
            gauge: Arc::clone(self),
            ticket,
            admitted: false,
        }
    }

    /// Requests currently blocked waiting for a permit.
    pub(in crate::daemon) fn waiting(&self) -> u32 {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Compiler children currently holding a permit.
    pub(in crate::daemon) fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Monotonic count of tickets that have left the queue.
    fn admitted(&self) -> u64 {
        self.admitted.load(Ordering::Relaxed)
    }
}

/// RAII tracker for one request's trip through the compile gate.
pub(in crate::daemon) struct CompileQueueGuard {
    gauge: Arc<CompileQueueGauge>,
    ticket: u64,
    admitted: bool,
}

impl CompileQueueGuard {
    /// Record that this request's permit was granted.
    pub(in crate::daemon) fn admit(&mut self) {
        if self.admitted {
            return;
        }
        self.admitted = true;
        self.gauge.admitted.fetch_add(1, Ordering::Relaxed);
        self.gauge.waiting.fetch_sub(1, Ordering::Relaxed);
        self.gauge.in_flight.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = current_slot() {
            slot.admitted.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for CompileQueueGuard {
    fn drop(&mut self) {
        if self.admitted {
            self.gauge.in_flight.fetch_sub(1, Ordering::Relaxed);
        } else {
            // Cancelled before admission. Advance the cursor anyway so the
            // waiters behind this one keep reporting a shrinking position.
            self.gauge.waiting.fetch_sub(1, Ordering::Relaxed);
            self.gauge.admitted.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(slot) = current_slot() {
            if slot.ticket.load(Ordering::Relaxed) == self.ticket {
                slot.ticket.store(NO_TICKET, Ordering::Relaxed);
                slot.admitted.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Per-request progress view, published by the compile gate and read by
/// the connection layer's heartbeat ticker.
#[derive(Debug)]
pub(in crate::daemon) struct CompileProgressSlot {
    ticket: AtomicU64,
    admitted: AtomicBool,
}

impl Default for CompileProgressSlot {
    fn default() -> Self {
        Self {
            ticket: AtomicU64::new(NO_TICKET),
            admitted: AtomicBool::new(false),
        }
    }
}

impl CompileProgressSlot {
    /// Number of compiles still ahead of this request, `0` once it holds a
    /// permit or has not reached the gate yet.
    fn queue_position(&self, gauge: &CompileQueueGauge) -> u32 {
        let ticket = self.ticket.load(Ordering::Relaxed);
        if ticket == NO_TICKET || self.admitted.load(Ordering::Relaxed) {
            return 0;
        }
        u32::try_from(ticket.saturating_sub(gauge.admitted())).unwrap_or(u32::MAX)
    }

    fn phase(&self) -> &'static str {
        if self.ticket.load(Ordering::Relaxed) != NO_TICKET
            && !self.admitted.load(Ordering::Relaxed)
        {
            PHASE_QUEUED
        } else {
            PHASE_COMPILING
        }
    }
}

/// Build the interim heartbeat frame for `slot` against the global `gauge`.
pub(in crate::daemon) fn progress_response(
    slot: &CompileProgressSlot,
    gauge: &CompileQueueGauge,
) -> Response {
    Response::CompileProgress {
        queue_position: slot.queue_position(gauge),
        queue_depth: gauge.waiting(),
        in_flight: gauge.in_flight(),
        phase: slot.phase().to_string(),
    }
}

tokio::task_local! {
    static PROGRESS_SLOT: Arc<CompileProgressSlot>;
}

/// Run `future` with `slot` installed as the current request's progress slot.
pub(in crate::daemon) fn scope<F>(
    slot: Arc<CompileProgressSlot>,
    future: F,
) -> impl std::future::Future<Output = F::Output>
where
    F: std::future::Future,
{
    PROGRESS_SLOT.scope(slot, future)
}

fn current_slot() -> Option<Arc<CompileProgressSlot>> {
    PROGRESS_SLOT.try_with(Arc::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positions(response: &Response) -> (u32, u32, u32, String) {
        match response {
            Response::CompileProgress {
                queue_position,
                queue_depth,
                in_flight,
                phase,
            } => (*queue_position, *queue_depth, *in_flight, phase.clone()),
            other => panic!("expected CompileProgress, got {other:?}"),
        }
    }

    #[test]
    fn gauge_tracks_waiting_and_in_flight() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let mut first = gauge.enqueue();
        let second = gauge.enqueue();
        assert_eq!(gauge.waiting(), 2);
        assert_eq!(gauge.in_flight(), 0);

        first.admit();
        assert_eq!(gauge.waiting(), 1);
        assert_eq!(gauge.in_flight(), 1);

        drop(first);
        assert_eq!(gauge.in_flight(), 0);
        drop(second);
        assert_eq!(gauge.waiting(), 0);
    }

    #[test]
    fn cancelled_waiter_still_advances_the_cursor() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let head = gauge.enqueue();
        let tail = gauge.enqueue();
        assert_eq!(tail.ticket, 1);
        // Head is dropped without ever being admitted (client disconnect).
        drop(head);
        assert_eq!(gauge.admitted(), 1, "cursor must advance past the cancel");
        let slot = CompileProgressSlot {
            ticket: AtomicU64::new(tail.ticket),
            admitted: AtomicBool::new(false),
        };
        assert_eq!(
            slot.queue_position(&gauge),
            0,
            "tail is now next in line, nobody ahead of it"
        );
    }

    #[test]
    fn position_counts_waiters_ahead() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let mut first = gauge.enqueue();
        let _second = gauge.enqueue();
        let third = gauge.enqueue();
        let slot = CompileProgressSlot {
            ticket: AtomicU64::new(third.ticket),
            admitted: AtomicBool::new(false),
        };
        let (position, depth, in_flight, phase) = positions(&progress_response(&slot, &gauge));
        assert_eq!(position, 2, "two compiles ahead");
        assert_eq!(depth, 3);
        assert_eq!(in_flight, 0);
        assert_eq!(phase, PHASE_QUEUED);

        first.admit();
        let (position, depth, in_flight, _) = positions(&progress_response(&slot, &gauge));
        assert_eq!(position, 1, "one admitted, position shrinks");
        assert_eq!(depth, 2);
        assert_eq!(in_flight, 1);
    }

    #[test]
    fn admitted_slot_reports_compiling_at_position_zero() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let slot = CompileProgressSlot {
            ticket: AtomicU64::new(7),
            admitted: AtomicBool::new(true),
        };
        let (position, _, _, phase) = positions(&progress_response(&slot, &gauge));
        assert_eq!(position, 0);
        assert_eq!(phase, PHASE_COMPILING);
    }

    #[test]
    fn unreached_gate_reports_compiling() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let slot = CompileProgressSlot::default();
        let (position, _, _, phase) = positions(&progress_response(&slot, &gauge));
        assert_eq!(position, 0);
        assert_eq!(
            phase, PHASE_COMPILING,
            "a request that never queues is simply running"
        );
    }

    #[tokio::test]
    async fn enqueue_publishes_the_ticket_into_the_task_local_slot() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let slot = Arc::new(CompileProgressSlot::default());
        let observed = scope(Arc::clone(&slot), {
            let gauge = Arc::clone(&gauge);
            let slot = Arc::clone(&slot);
            async move {
                let mut guard = gauge.enqueue();
                let queued = positions(&progress_response(&slot, &gauge));
                guard.admit();
                let running = positions(&progress_response(&slot, &gauge));
                drop(guard);
                let done = positions(&progress_response(&slot, &gauge));
                (queued, running, done)
            }
        })
        .await;
        assert_eq!(observed.0 .3, PHASE_QUEUED);
        assert_eq!(observed.0 .1, 1, "one waiter while queued");
        assert_eq!(observed.1 .3, PHASE_COMPILING);
        assert_eq!(observed.1 .2, 1, "one in flight once admitted");
        assert_eq!(observed.2 .2, 0, "guard drop releases the in-flight slot");
    }

    #[tokio::test]
    async fn concurrent_enqueues_hand_out_unique_shrinking_positions() {
        let gauge = Arc::new(CompileQueueGauge::default());
        let mut handles = Vec::new();
        for _ in 0..32 {
            let gauge = Arc::clone(&gauge);
            handles.push(tokio::spawn(async move {
                let slot = Arc::new(CompileProgressSlot::default());
                scope(Arc::clone(&slot), async move {
                    let mut guard = gauge.enqueue();
                    let ticket = guard.ticket;
                    guard.admit();
                    ticket
                })
                .await
            }));
        }
        let mut tickets = Vec::new();
        for handle in handles {
            tickets.push(handle.await.expect("task"));
        }
        tickets.sort_unstable();
        assert_eq!(
            tickets,
            (0..32).collect::<Vec<u64>>(),
            "every waiter gets a unique ticket under concurrency"
        );
        assert_eq!(gauge.waiting(), 0, "all waiters drained");
        assert_eq!(gauge.in_flight(), 0, "all guards dropped");
        assert_eq!(gauge.admitted(), 32);
    }
}
