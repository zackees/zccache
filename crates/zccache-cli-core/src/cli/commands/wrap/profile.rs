//! Wrapper-side phase timing (issue #1460).
//!
//! Every `ZCCACHE_PROFILE_*` surface in the tree is daemon-side, so the
//! per-invocation path — argv parse, tool resolution, endpoint resolve, IPC
//! roundtrip, output materialization — has never been measured. Attributing
//! #1437's emscripten multi-file cold regression with the daemon's own
//! `zccache_cc_miss_profile` accounted for 0.401s of a 1.071s gap; the
//! remaining 0.670s is in this path, which nothing could see.
//!
//! The line is emitted with `eprintln!` in the same `key=value` shape the
//! daemon uses, so it lands in the same benchmark log and `benchmark_stats`
//! can parse both. It is gated on the same env the daemon reads, which
//! `benchmark_stats.py` already sets unconditionally — so existing campaign
//! runs start carrying wrapper rows without a harness change.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

/// Shared with the daemon's cc-miss profile so one switch turns on both
/// halves of a single invocation's timing.
const PROFILE_ENV: &str = "ZCCACHE_PROFILE_CC_MISS";

/// The one route that must stay silent on stderr; see [`WrapperProfile::emit`].
pub(crate) const ROUTE_PROBE_BYPASS: &str = "probe_bypass";

/// Cached env lookup, matching the `OnceLock` pattern `zccache-depgraph`
/// already uses for this variable (`graph/mod.rs`). The wrapper runs once per
/// compile, so this is read on the hot path.
fn enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os(PROFILE_ENV).is_some())
}

/// Phase timing for one wrapper invocation.
///
/// Inert unless [`PROFILE_ENV`] is set: `start` is `None`, every mark is a
/// branch, and nothing is printed. Interior mutability keeps the handle
/// shareable by `&` through the routing code without threading `&mut`.
pub(crate) struct WrapperProfile {
    start: Option<Instant>,
    setup_ns: Cell<u64>,
    route: Cell<&'static str>,
}

impl WrapperProfile {
    pub(crate) fn start() -> Self {
        Self {
            start: enabled().then(Instant::now),
            setup_ns: Cell::new(0),
            route: Cell::new("none"),
        }
    }

    /// Everything before the invocation is routed: argv checks, strict-paths
    /// resolution, tool-path resolution, client env, endpoint resolve.
    pub(crate) fn mark_setup_done(&self) {
        if let Some(start) = self.start {
            self.setup_ns.set(elapsed_ns(start));
        }
    }

    /// Routes seen in the field: `compile`, `link_or_archive`, `formatter`,
    /// `probe_bypass`, `disabled`, and `none` for an argv error that returns
    /// before routing. Labelling the early returns keeps a row from looking
    /// like a routing bug when it is really an opt-out.
    pub(crate) fn set_route(&self, route: &'static str) {
        if self.start.is_some() {
            self.route.set(route);
        }
    }

    /// Whether [`emit`](Self::emit) would print. Exists so the silence
    /// contract is assertable without capturing stderr.
    pub(crate) fn would_emit(&self) -> bool {
        self.start.is_some() && self.route.get() != ROUTE_PROBE_BYPASS
    }

    /// Emit the row. `dispatch_ns` is the whole routed call — IPC connect,
    /// request, the daemon's own work, response, and output materialization.
    /// Subtracting the daemon's `total_ns` for the same compile leaves the
    /// out-of-daemon cost that #1460 exists to expose.
    ///
    /// Never emits on the probe-bypass route. Probe callers parse the tool's
    /// stderr, which is why `run_passthrough` is silent there — and
    /// `benchmark_stats.py` sets the profiling env *unconditionally*, so
    /// without this guard a probe during a benchmark run would be corrupted
    /// by the very instrumentation added to measure that run.
    pub(crate) fn emit(&self) {
        let Some(start) = self.start else {
            return;
        };
        if !self.would_emit() {
            return;
        }
        let total_ns = elapsed_ns(start);
        let setup_ns = self.setup_ns.get();
        eprintln!(
            "zccache_wrapper_profile route={} total_ns={} setup_ns={} dispatch_ns={}",
            self.route.get(),
            total_ns,
            setup_ns,
            total_ns.saturating_sub(setup_ns),
        );
    }
}

fn elapsed_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_profile_records_nothing() {
        // Constructed directly rather than via `start()`, which reads a
        // process-global env through a OnceLock and would make this test
        // order-dependent.
        let profile = WrapperProfile {
            start: None,
            setup_ns: Cell::new(0),
            route: Cell::new("none"),
        };

        profile.mark_setup_done();
        profile.set_route("compile");

        assert_eq!(profile.setup_ns.get(), 0);
        assert_eq!(
            profile.route.get(),
            "none",
            "a disabled profile must not even record the route"
        );
        profile.emit();
    }

    #[test]
    fn an_enabled_profile_records_setup_and_route() {
        let profile = WrapperProfile {
            start: Some(Instant::now()),
            setup_ns: Cell::new(0),
            route: Cell::new("none"),
        };

        profile.set_route("compile");
        profile.mark_setup_done();

        assert_eq!(profile.route.get(), "compile");
        // Any real elapsed time is fine; the contract is that it was taken,
        // not how long it was. Asserting a duration here would be exactly the
        // wall-clock mistake PERF.md warns about.
        assert!(profile.setup_ns.get() > 0);
    }

    #[test]
    fn dispatch_is_the_remainder_after_setup() {
        let start = Instant::now();
        let profile = WrapperProfile {
            start: Some(start),
            setup_ns: Cell::new(0),
            route: Cell::new("compile"),
        };
        profile.mark_setup_done();

        let total = elapsed_ns(start);
        assert!(
            total >= profile.setup_ns.get(),
            "setup is a prefix of total, so dispatch can never be negative"
        );
    }

    #[test]
    fn the_probe_bypass_route_is_never_emitted() {
        // `run_passthrough` is silent on this route because probe callers
        // parse the tool's stderr, and `benchmark_stats.py` sets the profiling
        // env unconditionally -- so an unguarded emit would corrupt probes
        // during exactly the benchmark runs this instrumentation serves.
        let profile = WrapperProfile {
            start: Some(Instant::now()),
            setup_ns: Cell::new(0),
            route: Cell::new(ROUTE_PROBE_BYPASS),
        };

        assert!(
            !profile.would_emit(),
            "probe-bypass must stay silent even with profiling enabled"
        );
    }

    #[test]
    fn other_routes_do_emit_when_enabled() {
        for route in ["compile", "link_or_archive", "formatter"] {
            let profile = WrapperProfile {
                start: Some(Instant::now()),
                setup_ns: Cell::new(0),
                route: Cell::new(route),
            };
            assert!(profile.would_emit(), "{route} should emit");
        }
    }
}
