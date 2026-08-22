//! Wrapper-side phase timing (issue #1460).
//!
//! Every `ZCCACHE_PROFILE_*` surface in the tree is daemon-side, so the
//! per-invocation path — argv parse, tool resolution, endpoint resolve, IPC
//! roundtrip, output materialization — had no timing at all. This measures it.
//!
//! Correction to this module's original rationale: it justified itself by the
//! unattributed remainder of #1437's emscripten cold gap. That was wrong. The
//! perf benchmark reaches the daemon through an in-process `ClientConn`
//! (`perf_bench/cpp_project.rs`) and never runs the wrapper, so nothing here
//! can explain that gap. What holds is the coverage claim: the path every
//! real user takes, once per compile, was unmeasured.
//!
//! The row is emitted with `eprintln!` in the same `key=value` shape the
//! daemon uses, so it lands in the same log and one parse gets both halves.
//! It is gated on the same env the daemon reads.
//!
//! State is process-global rather than a handle threaded through the call
//! graph. The wrapper process serves exactly one invocation, and the marks
//! that matter sit several frames deep inside the IPC path — behind an
//! ephemeral-session fallback that recurses — so threading a handle there
//! would reshape signatures across the whole route for instrumentation that
//! is off by default. Atomics also survive `run_async` moving the work onto a
//! tokio worker thread, which a `thread_local` would not.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Shared with the daemon's cc-miss profile so one switch turns on both
/// halves of a single invocation's timing.
const PROFILE_ENV: &str = "ZCCACHE_PROFILE_CC_MISS";

/// The one route that must stay silent on stderr; see [`emit`].
pub(crate) const ROUTE_PROBE_BYPASS: &str = "probe_bypass";

/// Sentinel for a phase that was never reached — a compile that fails to
/// connect never records a response, and `0` would read as "instant".
const UNSET: u64 = u64::MAX;

static START: OnceLock<Option<Instant>> = OnceLock::new();
static ROUTE: OnceLock<&'static str> = OnceLock::new();
static SETUP_NS: AtomicU64 = AtomicU64::new(UNSET);
static CONNECTED_NS: AtomicU64 = AtomicU64::new(UNSET);
static SENT_NS: AtomicU64 = AtomicU64::new(UNSET);
static RESPONSE_NS: AtomicU64 = AtomicU64::new(UNSET);

/// Cached env lookup, matching the `OnceLock` pattern `zccache-depgraph`
/// already uses for this variable (`graph/mod.rs`).
fn enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os(PROFILE_ENV).is_some())
}

fn started_at() -> Option<Instant> {
    *START.get_or_init(|| enabled().then(Instant::now))
}

fn elapsed_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(UNSET - 1)
}

fn mark(slot: &AtomicU64) {
    if let Some(start) = started_at() {
        // First writer wins. A failed connect retries through the ephemeral
        // fallback and connects twice; the first is the one the setup phase
        // precedes, so later marks must not overwrite it.
        let _ = slot.compare_exchange(
            UNSET,
            elapsed_ns(start),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Begin timing. Inert unless [`PROFILE_ENV`] is set.
pub(crate) fn start() {
    let _ = started_at();
}

/// Everything before the invocation is routed: argv checks, strict-paths
/// resolution, tool-path resolution, client env, endpoint resolve.
pub(crate) fn mark_setup_done() {
    mark(&SETUP_NS);
}

/// The daemon connection is open — end of client-side connect cost.
pub(crate) fn mark_connected() {
    mark(&CONNECTED_NS);
}

/// The request is on the wire — end of encode/send, start of the daemon wait.
///
/// Splitting this out is what makes `wait_ns` comparable to the daemon's own
/// `total_ns`: without it, request encoding and transmission were folded into
/// the same number as the daemon's work.
pub(crate) fn mark_request_sent() {
    mark(&SENT_NS);
}

/// The daemon's response has arrived — end of the wait, start of output work.
pub(crate) fn mark_response() {
    mark(&RESPONSE_NS);
}

pub(crate) fn set_route(route: &'static str) {
    if started_at().is_some() {
        let _ = ROUTE.set(route);
    }
}

fn route() -> &'static str {
    ROUTE.get().copied().unwrap_or("none")
}

/// Whether [`emit`] would print. Exists so the silence contract is assertable
/// without capturing stderr.
pub(crate) fn would_emit() -> bool {
    started_at().is_some() && route() != ROUTE_PROBE_BYPASS
}

/// Difference between two marks, or `None` if either was never reached.
fn span(from: u64, to: u64) -> Option<u64> {
    (from != UNSET && to != UNSET).then(|| to.saturating_sub(from))
}

fn render(total_ns: u64, setup: u64, connected: u64, sent: u64, response: u64) -> String {
    // A phase that was not reached prints -1 rather than 0, so "never
    // happened" cannot be misread as "took no time".
    let field = |value: Option<u64>| value.map_or_else(|| "-1".to_string(), |ns| ns.to_string());
    format!(
        "zccache_wrapper_profile route={} total_ns={} setup_ns={} connect_ns={} \
         send_ns={} wait_ns={} post_ns={}",
        route(),
        total_ns,
        field(span(0, setup)),
        field(span(setup, connected)),
        field(span(connected, sent)),
        field(span(sent, response)),
        field(span(response, total_ns)),
    )
}

/// Emit the row.
///
/// Never emits on the probe-bypass route. Probe callers parse the tool's
/// stderr, which is why `run_passthrough` is silent there — and
/// `benchmark_stats.py` sets the profiling env *unconditionally*, so without
/// this guard a probe during a benchmark run would be corrupted by the very
/// instrumentation added to measure that run.
pub(crate) fn emit() {
    let Some(start) = started_at() else {
        return;
    };
    if !would_emit() {
        return;
    }
    eprintln!(
        "{}",
        render(
            elapsed_ns(start),
            SETUP_NS.load(Ordering::Acquire),
            CONNECTED_NS.load(Ordering::Acquire),
            SENT_NS.load(Ordering::Acquire),
            RESPONSE_NS.load(Ordering::Acquire),
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // `render` and `span` are pure, so the phase arithmetic is testable
    // without touching the process-global marks — which a single test binary
    // shares and which are deliberately write-once.

    #[test]
    fn spans_are_differences_between_marks() {
        assert_eq!(span(0, 100), Some(100));
        assert_eq!(span(100, 250), Some(150));
    }

    #[test]
    fn an_unreached_mark_has_no_span() {
        assert_eq!(span(100, UNSET), None);
        assert_eq!(span(UNSET, 100), None);
    }

    #[test]
    fn a_span_never_goes_negative() {
        // Marks are monotonic in practice; saturating means a reordering bug
        // surfaces as 0 rather than an absurd u64.
        assert_eq!(span(250, 100), Some(0));
    }

    #[test]
    fn unreached_phases_render_as_minus_one_not_zero() {
        // A compile that never connected must not report "connect took 0ns".
        let line = render(500, 100, UNSET, UNSET, UNSET);

        assert!(line.contains("setup_ns=100"), "{line}");
        assert!(line.contains("connect_ns=-1"), "{line}");
        assert!(line.contains("send_ns=-1"), "{line}");
        assert!(line.contains("wait_ns=-1"), "{line}");
        assert!(line.contains("post_ns=-1"), "{line}");
    }

    #[test]
    fn a_send_that_never_completed_leaves_wait_unreported() {
        // Connected, then the send failed: connect is real, everything after
        // it is not. Reporting wait_ns=0 here would invent a daemon that
        // answered instantly.
        let line = render(500, 100, 250, UNSET, UNSET);

        assert!(line.contains("connect_ns=150"), "{line}");
        assert!(line.contains("send_ns=-1"), "{line}");
        assert!(line.contains("wait_ns=-1"), "{line}");
    }

    #[test]
    fn a_complete_invocation_renders_every_phase() {
        let line = render(1000, 100, 250, 300, 900);

        assert!(line.contains("total_ns=1000"), "{line}");
        assert!(line.contains("setup_ns=100"), "{line}");
        assert!(line.contains("connect_ns=150"), "{line}");
        assert!(line.contains("send_ns=50"), "{line}");
        assert!(line.contains("wait_ns=600"), "{line}");
        assert!(line.contains("post_ns=100"), "{line}");
    }

    #[test]
    fn phases_sum_to_the_total() {
        let (total, setup, connected, sent, response) = (1000u64, 100u64, 250u64, 300u64, 900u64);
        let parts = span(0, setup).unwrap()
            + span(setup, connected).unwrap()
            + span(connected, sent).unwrap()
            + span(sent, response).unwrap()
            + span(response, total).unwrap();

        assert_eq!(parts, total, "phases must account for the whole invocation");
    }
}
