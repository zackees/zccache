//! Global daemon statistics collector.
//!
//! All counters are atomic — safe to update from concurrent connection handlers.

use std::sync::atomic::{AtomicU64, Ordering};

/// Global statistics collected since daemon startup.
pub struct StatsCollector {
    /// Total compile requests received.
    compilations: AtomicU64,
    /// Cache hits.
    hits: AtomicU64,
    /// Cache misses (cold compiles).
    misses: AtomicU64,
    /// Non-cacheable invocations.
    non_cacheable: AtomicU64,
    /// Compilations that exited non-zero.
    compile_errors: AtomicU64,
    /// Non-zero compile results served from cache.
    compile_errors_cached: AtomicU64,
    /// Total sessions created.
    sessions_total: AtomicU64,
    /// Cumulative nanoseconds spent on cache hits.
    hit_time_ns: AtomicU64,
    /// Cumulative nanoseconds spent on cache misses.
    miss_time_ns: AtomicU64,
    /// Total artifact bytes served from cache.
    bytes_read: AtomicU64,
    /// Total artifact bytes stored into cache.
    bytes_written: AtomicU64,
    /// Total link/archive requests.
    link_total: AtomicU64,
    /// Link cache hits.
    link_hits: AtomicU64,
    /// Link cache misses.
    link_misses: AtomicU64,
    /// Non-cacheable link invocations.
    link_non_cacheable: AtomicU64,
}

/// Snapshot of current stats values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub compilations: u64,
    pub hits: u64,
    pub misses: u64,
    pub non_cacheable: u64,
    pub compile_errors: u64,
    pub compile_errors_cached: u64,
    pub sessions_total: u64,
    pub hit_time_ns: u64,
    pub miss_time_ns: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub link_total: u64,
    pub link_hits: u64,
    pub link_misses: u64,
    pub link_non_cacheable: u64,
}

impl StatsSnapshot {
    /// Estimated time saved: difference between average miss time and average hit time,
    /// multiplied by number of hits. Returns 0 if no data.
    #[must_use]
    pub fn time_saved_ms(&self) -> u64 {
        if self.hits == 0 || self.misses == 0 {
            return 0;
        }
        let avg_hit_ns = self.hit_time_ns / self.hits;
        let avg_miss_ns = self.miss_time_ns / self.misses;
        let saved_per_hit_ns = avg_miss_ns.saturating_sub(avg_hit_ns);
        (self.hits * saved_per_hit_ns) / 1_000_000
    }
}

impl StatsCollector {
    /// Create a new collector with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compilations: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            non_cacheable: AtomicU64::new(0),
            compile_errors: AtomicU64::new(0),
            compile_errors_cached: AtomicU64::new(0),
            sessions_total: AtomicU64::new(0),
            hit_time_ns: AtomicU64::new(0),
            miss_time_ns: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            link_total: AtomicU64::new(0),
            link_hits: AtomicU64::new(0),
            link_misses: AtomicU64::new(0),
            link_non_cacheable: AtomicU64::new(0),
        }
    }

    /// Record a compilation request (always called, regardless of outcome).
    pub fn record_compilation(&self) {
        self.compilations.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a cache hit with its latency.
    pub fn record_hit(&self, latency_ns: u64, artifact_bytes: u64) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.hit_time_ns.fetch_add(latency_ns, Ordering::Relaxed);
        self.bytes_read.fetch_add(artifact_bytes, Ordering::Relaxed);
    }

    /// Record a cache miss with its latency.
    pub fn record_miss(&self, latency_ns: u64, artifact_bytes: u64) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.miss_time_ns.fetch_add(latency_ns, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(artifact_bytes, Ordering::Relaxed);
    }

    /// Record a non-cacheable invocation.
    pub fn record_non_cacheable(&self) {
        self.non_cacheable.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a compile error (non-zero exit).
    pub fn record_error(&self) {
        self.compile_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a cached compile error replay.
    pub fn record_cached_error(&self) {
        self.compile_errors_cached.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a new session creation.
    pub fn record_session(&self) {
        self.sessions_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a link/archive request.
    pub fn record_link(&self) {
        self.link_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a link cache hit.
    pub fn record_link_hit(&self) {
        self.link_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a link cache miss.
    pub fn record_link_miss(&self) {
        self.link_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a non-cacheable link invocation.
    pub fn record_link_non_cacheable(&self) {
        self.link_non_cacheable.fetch_add(1, Ordering::Relaxed);
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.compilations.store(0, Ordering::Relaxed);
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.non_cacheable.store(0, Ordering::Relaxed);
        self.compile_errors.store(0, Ordering::Relaxed);
        self.compile_errors_cached.store(0, Ordering::Relaxed);
        self.sessions_total.store(0, Ordering::Relaxed);
        self.hit_time_ns.store(0, Ordering::Relaxed);
        self.miss_time_ns.store(0, Ordering::Relaxed);
        self.bytes_read.store(0, Ordering::Relaxed);
        self.bytes_written.store(0, Ordering::Relaxed);
        self.link_total.store(0, Ordering::Relaxed);
        self.link_hits.store(0, Ordering::Relaxed);
        self.link_misses.store(0, Ordering::Relaxed);
        self.link_non_cacheable.store(0, Ordering::Relaxed);
    }

    /// Take a consistent snapshot of all counters.
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            compilations: self.compilations.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            non_cacheable: self.non_cacheable.load(Ordering::Relaxed),
            compile_errors: self.compile_errors.load(Ordering::Relaxed),
            compile_errors_cached: self.compile_errors_cached.load(Ordering::Relaxed),
            sessions_total: self.sessions_total.load(Ordering::Relaxed),
            hit_time_ns: self.hit_time_ns.load(Ordering::Relaxed),
            miss_time_ns: self.miss_time_ns.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            link_total: self.link_total.load(Ordering::Relaxed),
            link_hits: self.link_hits.load(Ordering::Relaxed),
            link_misses: self.link_misses.load(Ordering::Relaxed),
            link_non_cacheable: self.link_non_cacheable.load(Ordering::Relaxed),
        }
    }
}

impl Default for StatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Phase profiling ─────────────────────────────────────────────────────────

/// Accumulated timing for each phase of the compile hot path.
///
/// All values are in nanoseconds. Thread-safe via atomics.
pub struct PhaseProfiler {
    /// Bounded staged-output pipeline telemetry.
    pub(crate) staged: super::staged_stats::StagedProfiler,
    /// Number of profiled requests.
    pub count: AtomicU64,
    /// Parse compiler invocation args.
    pub parse_args_ns: AtomicU64,
    /// Build compile context + register with depgraph.
    pub build_context_ns: AtomicU64,
    /// Hash source file via metadata cache.
    pub hash_source_ns: AtomicU64,
    /// Hash all known headers via metadata cache.
    pub hash_headers_ns: AtomicU64,
    /// Check depgraph for cache verdict.
    pub depgraph_check_ns: AtomicU64,
    /// Request-level cache lookup.
    pub request_cache_lookup_ns: AtomicU64,
    /// Cross-root request-cache validation.
    pub cross_root_validate_ns: AtomicU64,
    /// Artifact lookup (in-memory HashMap).
    pub artifact_lookup_ns: AtomicU64,
    /// Write cached outputs to disk.
    pub write_output_ns: AtomicU64,
    /// Record stats + session bookkeeping.
    pub bookkeeping_ns: AtomicU64,
    /// Total wall clock for the full hit path.
    pub total_hit_ns: AtomicU64,
    // Miss path
    /// Run the actual compiler (miss path).
    pub compiler_exec_ns: AtomicU64,
    /// Scan includes (miss path).
    pub include_scan_ns: AtomicU64,
    /// Hash all files for artifact key (miss path).
    pub hash_all_ns: AtomicU64,
    /// Store artifact (miss path).
    pub artifact_store_ns: AtomicU64,
    /// Total wall clock for the full miss path.
    pub total_miss_ns: AtomicU64,
    /// Number of profiled misses.
    pub miss_count: AtomicU64,
}

impl PhaseProfiler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            staged: super::staged_stats::StagedProfiler::new(),
            count: AtomicU64::new(0),
            parse_args_ns: AtomicU64::new(0),
            build_context_ns: AtomicU64::new(0),
            hash_source_ns: AtomicU64::new(0),
            hash_headers_ns: AtomicU64::new(0),
            depgraph_check_ns: AtomicU64::new(0),
            request_cache_lookup_ns: AtomicU64::new(0),
            cross_root_validate_ns: AtomicU64::new(0),
            artifact_lookup_ns: AtomicU64::new(0),
            write_output_ns: AtomicU64::new(0),
            bookkeeping_ns: AtomicU64::new(0),
            total_hit_ns: AtomicU64::new(0),
            compiler_exec_ns: AtomicU64::new(0),
            include_scan_ns: AtomicU64::new(0),
            hash_all_ns: AtomicU64::new(0),
            artifact_store_ns: AtomicU64::new(0),
            total_miss_ns: AtomicU64::new(0),
            miss_count: AtomicU64::new(0),
        }
    }

    /// Record timing for one cache-hit compile.
    pub fn record_hit(&self, phases: &HitPhases) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.parse_args_ns
            .fetch_add(phases.parse_args_ns, Ordering::Relaxed);
        self.build_context_ns
            .fetch_add(phases.build_context_ns, Ordering::Relaxed);
        self.hash_source_ns
            .fetch_add(phases.hash_source_ns, Ordering::Relaxed);
        self.hash_headers_ns
            .fetch_add(phases.hash_headers_ns, Ordering::Relaxed);
        self.depgraph_check_ns
            .fetch_add(phases.depgraph_check_ns, Ordering::Relaxed);
        self.request_cache_lookup_ns
            .fetch_add(phases.request_cache_lookup_ns, Ordering::Relaxed);
        self.cross_root_validate_ns
            .fetch_add(phases.cross_root_validate_ns, Ordering::Relaxed);
        self.artifact_lookup_ns
            .fetch_add(phases.artifact_lookup_ns, Ordering::Relaxed);
        self.write_output_ns
            .fetch_add(phases.write_output_ns, Ordering::Relaxed);
        self.bookkeeping_ns
            .fetch_add(phases.bookkeeping_ns, Ordering::Relaxed);
        self.total_hit_ns
            .fetch_add(phases.total_ns, Ordering::Relaxed);
    }

    /// Record timing for one cache-miss compile.
    pub fn record_miss(&self, phases: &MissPhases) {
        self.miss_count.fetch_add(1, Ordering::Relaxed);
        self.compiler_exec_ns
            .fetch_add(phases.compiler_exec_ns, Ordering::Relaxed);
        self.include_scan_ns
            .fetch_add(phases.include_scan_ns, Ordering::Relaxed);
        self.hash_all_ns
            .fetch_add(phases.hash_all_ns, Ordering::Relaxed);
        self.artifact_store_ns
            .fetch_add(phases.artifact_store_ns, Ordering::Relaxed);
        self.total_miss_ns
            .fetch_add(phases.total_ns, Ordering::Relaxed);
    }

    /// Reset all phase counters to zero.
    pub fn reset(&self) {
        self.staged.reset();
        self.count.store(0, Ordering::Relaxed);
        self.parse_args_ns.store(0, Ordering::Relaxed);
        self.build_context_ns.store(0, Ordering::Relaxed);
        self.hash_source_ns.store(0, Ordering::Relaxed);
        self.hash_headers_ns.store(0, Ordering::Relaxed);
        self.depgraph_check_ns.store(0, Ordering::Relaxed);
        self.request_cache_lookup_ns.store(0, Ordering::Relaxed);
        self.cross_root_validate_ns.store(0, Ordering::Relaxed);
        self.artifact_lookup_ns.store(0, Ordering::Relaxed);
        self.write_output_ns.store(0, Ordering::Relaxed);
        self.bookkeeping_ns.store(0, Ordering::Relaxed);
        self.total_hit_ns.store(0, Ordering::Relaxed);
        self.compiler_exec_ns.store(0, Ordering::Relaxed);
        self.include_scan_ns.store(0, Ordering::Relaxed);
        self.hash_all_ns.store(0, Ordering::Relaxed);
        self.artifact_store_ns.store(0, Ordering::Relaxed);
        self.total_miss_ns.store(0, Ordering::Relaxed);
        self.miss_count.store(0, Ordering::Relaxed);
    }

    /// Return a snapshot of raw phase-timing totals (not averaged).
    ///
    /// Used by [`super::server`]'s `SessionEnd` / `SessionStats` handlers
    /// to populate [`crate::protocol::SessionStats::phase_profile`]. The
    /// caller divides by `hit_count` / `miss_count` if it wants averages —
    /// the existing [`snapshot`] does that, but pre-averaged values can't
    /// be summed if a downstream tool ever wants to combine results from
    /// multiple sessions. Keep the wire format raw and let consumers
    /// average.
    ///
    /// Returns counts as they currently sit in the atomics; no clamping
    /// to `max(1)` as in [`snapshot`].
    ///
    /// [`snapshot`]: PhaseProfiler::snapshot
    #[must_use]
    pub fn totals_snapshot(&self) -> PhaseTotals {
        PhaseTotals {
            hit_count: self.count.load(Ordering::Relaxed),
            miss_count: self.miss_count.load(Ordering::Relaxed),
            parse_args_ns: self.parse_args_ns.load(Ordering::Relaxed),
            build_context_ns: self.build_context_ns.load(Ordering::Relaxed),
            hash_source_ns: self.hash_source_ns.load(Ordering::Relaxed),
            hash_headers_ns: self.hash_headers_ns.load(Ordering::Relaxed),
            depgraph_check_ns: self.depgraph_check_ns.load(Ordering::Relaxed),
            request_cache_lookup_ns: self.request_cache_lookup_ns.load(Ordering::Relaxed),
            cross_root_validate_ns: self.cross_root_validate_ns.load(Ordering::Relaxed),
            artifact_lookup_ns: self.artifact_lookup_ns.load(Ordering::Relaxed),
            write_output_ns: self.write_output_ns.load(Ordering::Relaxed),
            bookkeeping_ns: self.bookkeeping_ns.load(Ordering::Relaxed),
            total_hit_ns: self.total_hit_ns.load(Ordering::Relaxed),
            compiler_exec_ns: self.compiler_exec_ns.load(Ordering::Relaxed),
            include_scan_ns: self.include_scan_ns.load(Ordering::Relaxed),
            hash_all_ns: self.hash_all_ns.load(Ordering::Relaxed),
            artifact_store_ns: self.artifact_store_ns.load(Ordering::Relaxed),
            total_miss_ns: self.total_miss_ns.load(Ordering::Relaxed),
            staged: self.staged.snapshot(),
        }
    }

    /// Return a snapshot of average phase durations.
    #[must_use]
    pub fn snapshot(&self) -> ProfileSnapshot {
        let n = self.count.load(Ordering::Relaxed).max(1);
        let mn = self.miss_count.load(Ordering::Relaxed).max(1);
        ProfileSnapshot {
            hit_count: self.count.load(Ordering::Relaxed),
            miss_count: self.miss_count.load(Ordering::Relaxed),
            avg_parse_args_ns: self.parse_args_ns.load(Ordering::Relaxed) / n,
            avg_build_context_ns: self.build_context_ns.load(Ordering::Relaxed) / n,
            avg_hash_source_ns: self.hash_source_ns.load(Ordering::Relaxed) / n,
            avg_hash_headers_ns: self.hash_headers_ns.load(Ordering::Relaxed) / n,
            avg_depgraph_check_ns: self.depgraph_check_ns.load(Ordering::Relaxed) / n,
            avg_request_cache_lookup_ns: self.request_cache_lookup_ns.load(Ordering::Relaxed) / n,
            avg_cross_root_validate_ns: self.cross_root_validate_ns.load(Ordering::Relaxed) / n,
            avg_artifact_lookup_ns: self.artifact_lookup_ns.load(Ordering::Relaxed) / n,
            avg_write_output_ns: self.write_output_ns.load(Ordering::Relaxed) / n,
            avg_bookkeeping_ns: self.bookkeeping_ns.load(Ordering::Relaxed) / n,
            avg_total_hit_ns: self.total_hit_ns.load(Ordering::Relaxed) / n,
            avg_compiler_exec_ns: self.compiler_exec_ns.load(Ordering::Relaxed) / mn,
            avg_include_scan_ns: self.include_scan_ns.load(Ordering::Relaxed) / mn,
            avg_hash_all_ns: self.hash_all_ns.load(Ordering::Relaxed) / mn,
            avg_artifact_store_ns: self.artifact_store_ns.load(Ordering::Relaxed) / mn,
            avg_total_miss_ns: self.total_miss_ns.load(Ordering::Relaxed) / mn,
        }
    }
}

impl Default for PhaseProfiler {
    fn default() -> Self {
        Self::new()
    }
}

/// Timing data for one cache-hit compile request.
pub struct HitPhases {
    pub parse_args_ns: u64,
    pub build_context_ns: u64,
    pub hash_source_ns: u64,
    pub hash_headers_ns: u64,
    pub depgraph_check_ns: u64,
    pub request_cache_lookup_ns: u64,
    pub cross_root_validate_ns: u64,
    pub artifact_lookup_ns: u64,
    pub write_output_ns: u64,
    pub bookkeeping_ns: u64,
    pub total_ns: u64,
}

/// Timing data for one cache-miss compile request.
pub struct MissPhases {
    pub compiler_exec_ns: u64,
    pub include_scan_ns: u64,
    pub hash_all_ns: u64,
    pub artifact_store_ns: u64,
    pub total_ns: u64,
}

/// Raw phase-timing totals — symmetric to [`ProfileSnapshot`] but without
/// the per-compile averaging. The struct layout intentionally mirrors
/// [`crate::protocol::PhaseProfileSummary`] so the conversion is
/// field-for-field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTotals {
    pub hit_count: u64,
    pub miss_count: u64,
    pub parse_args_ns: u64,
    pub build_context_ns: u64,
    pub hash_source_ns: u64,
    pub hash_headers_ns: u64,
    pub depgraph_check_ns: u64,
    pub request_cache_lookup_ns: u64,
    pub cross_root_validate_ns: u64,
    pub artifact_lookup_ns: u64,
    pub write_output_ns: u64,
    pub bookkeeping_ns: u64,
    pub total_hit_ns: u64,
    pub compiler_exec_ns: u64,
    pub include_scan_ns: u64,
    pub hash_all_ns: u64,
    pub artifact_store_ns: u64,
    pub total_miss_ns: u64,
    pub staged: crate::protocol::StagedProfileSummary,
}

impl From<PhaseTotals> for crate::protocol::PhaseProfileSummary {
    fn from(t: PhaseTotals) -> Self {
        Self {
            hit_count: t.hit_count,
            miss_count: t.miss_count,
            parse_args_ns: t.parse_args_ns,
            build_context_ns: t.build_context_ns,
            hash_source_ns: t.hash_source_ns,
            hash_headers_ns: t.hash_headers_ns,
            depgraph_check_ns: t.depgraph_check_ns,
            request_cache_lookup_ns: t.request_cache_lookup_ns,
            cross_root_validate_ns: t.cross_root_validate_ns,
            artifact_lookup_ns: t.artifact_lookup_ns,
            write_output_ns: t.write_output_ns,
            bookkeeping_ns: t.bookkeeping_ns,
            total_hit_ns: t.total_hit_ns,
            compiler_exec_ns: t.compiler_exec_ns,
            include_scan_ns: t.include_scan_ns,
            hash_all_ns: t.hash_all_ns,
            artifact_store_ns: t.artifact_store_ns,
            total_miss_ns: t.total_miss_ns,
            staged: t.staged,
        }
    }
}

/// Averaged profile snapshot.
#[derive(Debug, Clone)]
pub struct ProfileSnapshot {
    pub hit_count: u64,
    pub miss_count: u64,
    pub avg_parse_args_ns: u64,
    pub avg_build_context_ns: u64,
    pub avg_hash_source_ns: u64,
    pub avg_hash_headers_ns: u64,
    pub avg_depgraph_check_ns: u64,
    pub avg_request_cache_lookup_ns: u64,
    pub avg_cross_root_validate_ns: u64,
    pub avg_artifact_lookup_ns: u64,
    pub avg_write_output_ns: u64,
    pub avg_bookkeeping_ns: u64,
    pub avg_total_hit_ns: u64,
    pub avg_compiler_exec_ns: u64,
    pub avg_include_scan_ns: u64,
    pub avg_hash_all_ns: u64,
    pub avg_artifact_store_ns: u64,
    pub avg_total_miss_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_collector_is_zero() {
        let c = StatsCollector::new();
        let s = c.snapshot();
        assert_eq!(s.compilations, 0);
        assert_eq!(s.hits, 0);
        assert_eq!(s.misses, 0);
        assert_eq!(s.non_cacheable, 0);
        assert_eq!(s.compile_errors, 0);
        assert_eq!(s.compile_errors_cached, 0);
        assert_eq!(s.sessions_total, 0);
        assert_eq!(s.bytes_read, 0);
        assert_eq!(s.bytes_written, 0);
    }

    #[test]
    fn record_hit_increments() {
        let c = StatsCollector::new();
        c.record_compilation();
        c.record_hit(500, 1024);
        let s = c.snapshot();
        assert_eq!(s.compilations, 1);
        assert_eq!(s.hits, 1);
        assert_eq!(s.hit_time_ns, 500);
        assert_eq!(s.bytes_read, 1024);
    }

    #[test]
    fn record_miss_increments() {
        let c = StatsCollector::new();
        c.record_compilation();
        c.record_miss(50_000, 2048);
        let s = c.snapshot();
        assert_eq!(s.compilations, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.miss_time_ns, 50_000);
        assert_eq!(s.bytes_written, 2048);
    }

    #[test]
    fn record_non_cacheable_and_error() {
        let c = StatsCollector::new();
        c.record_non_cacheable();
        c.record_non_cacheable();
        c.record_error();
        c.record_cached_error();
        let s = c.snapshot();
        assert_eq!(s.non_cacheable, 2);
        assert_eq!(s.compile_errors, 1);
        assert_eq!(s.compile_errors_cached, 1);
    }

    #[test]
    fn record_session_increments() {
        let c = StatsCollector::new();
        c.record_session();
        c.record_session();
        c.record_session();
        assert_eq!(c.snapshot().sessions_total, 3);
    }

    #[test]
    fn time_saved_ms_calculation() {
        let c = StatsCollector::new();
        // 10 hits at 1ms each (1_000_000ns), 5 misses at 100ms each (100_000_000ns)
        for _ in 0..10 {
            c.record_hit(1_000_000, 0);
        }
        for _ in 0..5 {
            c.record_miss(100_000_000, 0);
        }
        let s = c.snapshot();
        // avg_hit = 1000000ns, avg_miss = 100000000ns, saved_per_hit = 99000000ns
        // total_saved = 10 * 99000000 / 1_000_000 = 990ms
        assert_eq!(s.time_saved_ms(), 990);
    }

    #[test]
    fn time_saved_ms_no_data() {
        let c = StatsCollector::new();
        assert_eq!(c.snapshot().time_saved_ms(), 0);

        // Only hits, no misses — can't estimate
        c.record_hit(500, 0);
        assert_eq!(c.snapshot().time_saved_ms(), 0);
    }

    #[test]
    fn reset_clears_all_counters() {
        let c = StatsCollector::new();
        c.record_compilation();
        c.record_hit(500, 1024);
        c.record_miss(50_000, 2048);
        c.record_non_cacheable();
        c.record_error();
        c.record_session();

        c.reset();

        let s = c.snapshot();
        assert_eq!(
            s,
            StatsSnapshot {
                compilations: 0,
                hits: 0,
                misses: 0,
                non_cacheable: 0,
                compile_errors: 0,
                compile_errors_cached: 0,
                sessions_total: 0,
                hit_time_ns: 0,
                miss_time_ns: 0,
                bytes_read: 0,
                bytes_written: 0,
                link_total: 0,
                link_hits: 0,
                link_misses: 0,
                link_non_cacheable: 0,
            }
        );
    }

    #[test]
    fn profiler_reset_clears_all_counters() {
        let p = PhaseProfiler::new();
        p.record_hit(&HitPhases {
            parse_args_ns: 100,
            build_context_ns: 200,
            hash_source_ns: 300,
            hash_headers_ns: 400,
            depgraph_check_ns: 500,
            request_cache_lookup_ns: 0,
            cross_root_validate_ns: 0,
            artifact_lookup_ns: 600,
            write_output_ns: 700,
            bookkeeping_ns: 800,
            total_ns: 3600,
        });
        p.record_miss(&MissPhases {
            compiler_exec_ns: 1000,
            include_scan_ns: 2000,
            hash_all_ns: 3000,
            artifact_store_ns: 4000,
            total_ns: 10000,
        });

        p.reset();

        let s = p.snapshot();
        assert_eq!(s.hit_count, 0);
        assert_eq!(s.miss_count, 0);
    }

    #[test]
    fn totals_snapshot_round_trips_to_phase_profile_summary() {
        // Records one hit and one miss with distinct values in every field,
        // then verifies that `totals_snapshot()` returns the raw atomics
        // (not averaged) and that the protocol-crate `From` conversion
        // preserves every field. This is the wire-format proof for the
        // observability path: PhaseProfiler atomics → PhaseTotals →
        // protocol::PhaseProfileSummary → JSON in last-session-stats.json.
        let p = PhaseProfiler::new();
        p.record_hit(&HitPhases {
            parse_args_ns: 11,
            build_context_ns: 22,
            hash_source_ns: 33,
            hash_headers_ns: 44,
            depgraph_check_ns: 55,
            request_cache_lookup_ns: 66,
            cross_root_validate_ns: 77,
            artifact_lookup_ns: 88,
            write_output_ns: 99,
            bookkeeping_ns: 101,
            total_ns: 596,
        });
        p.record_miss(&MissPhases {
            compiler_exec_ns: 1_000,
            include_scan_ns: 2_000,
            hash_all_ns: 3_000,
            artifact_store_ns: 4_000,
            total_ns: 10_000,
        });

        let totals = p.totals_snapshot();
        assert_eq!(totals.hit_count, 1);
        assert_eq!(totals.miss_count, 1);
        // Raw totals — NOT averaged. snapshot() would divide by hit_count.
        assert_eq!(totals.parse_args_ns, 11);
        assert_eq!(totals.write_output_ns, 99);
        assert_eq!(totals.total_hit_ns, 596);
        assert_eq!(totals.compiler_exec_ns, 1_000);
        assert_eq!(totals.total_miss_ns, 10_000);

        // From-impl preserves every field for the protocol crate.
        let summary: crate::protocol::PhaseProfileSummary = totals.clone().into();
        assert_eq!(summary.hit_count, totals.hit_count);
        assert_eq!(summary.miss_count, totals.miss_count);
        assert_eq!(summary.parse_args_ns, totals.parse_args_ns);
        assert_eq!(summary.build_context_ns, totals.build_context_ns);
        assert_eq!(summary.hash_source_ns, totals.hash_source_ns);
        assert_eq!(summary.hash_headers_ns, totals.hash_headers_ns);
        assert_eq!(summary.depgraph_check_ns, totals.depgraph_check_ns);
        assert_eq!(
            summary.request_cache_lookup_ns,
            totals.request_cache_lookup_ns
        );
        assert_eq!(
            summary.cross_root_validate_ns,
            totals.cross_root_validate_ns
        );
        assert_eq!(summary.artifact_lookup_ns, totals.artifact_lookup_ns);
        assert_eq!(summary.write_output_ns, totals.write_output_ns);
        assert_eq!(summary.bookkeeping_ns, totals.bookkeeping_ns);
        assert_eq!(summary.total_hit_ns, totals.total_hit_ns);
        assert_eq!(summary.compiler_exec_ns, totals.compiler_exec_ns);
        assert_eq!(summary.include_scan_ns, totals.include_scan_ns);
        assert_eq!(summary.hash_all_ns, totals.hash_all_ns);
        assert_eq!(summary.artifact_store_ns, totals.artifact_store_ns);
        assert_eq!(summary.total_miss_ns, totals.total_miss_ns);
    }

    #[test]
    fn totals_snapshot_reflects_reset() {
        let p = PhaseProfiler::new();
        p.record_hit(&HitPhases {
            parse_args_ns: 100,
            build_context_ns: 200,
            hash_source_ns: 300,
            hash_headers_ns: 400,
            depgraph_check_ns: 500,
            request_cache_lookup_ns: 0,
            cross_root_validate_ns: 0,
            artifact_lookup_ns: 600,
            write_output_ns: 700,
            bookkeeping_ns: 800,
            total_ns: 3600,
        });
        assert_eq!(p.totals_snapshot().total_hit_ns, 3600);
        p.reset();
        let totals = p.totals_snapshot();
        assert_eq!(totals.hit_count, 0);
        assert_eq!(totals.total_hit_ns, 0);
    }

    #[test]
    fn concurrent_increments() {
        use std::sync::Arc;
        use std::thread;

        let c = Arc::new(StatsCollector::new());
        let mut handles = Vec::new();

        for _ in 0..4 {
            let c = Arc::clone(&c);
            handles.push(thread::spawn(move || {
                for _ in 0..250 {
                    c.record_compilation();
                    c.record_hit(100, 10);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let s = c.snapshot();
        assert_eq!(s.compilations, 1000);
        assert_eq!(s.hits, 1000);
        assert_eq!(s.hit_time_ns, 100_000);
        assert_eq!(s.bytes_read, 10_000);
    }
}
