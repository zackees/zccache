//! Perf budget tests for the compile journal hot path.
//!
//! Issue #459: `JournalEntry::new` + `CompileJournal::log` together used to
//! cost ~2–4 µs on Windows (SystemTime::now + format_timestamp +
//! serde_json::to_string) and the client used to wait on that work before
//! `conn.send` returned. The fix reorders the daemon dispatch loop so the
//! response is sent first; these tests pin the per-call cost so a future
//! regression that adds more synchronous work to the hot path is caught.
//!
//! Budget is set ~10× looser than typical so transient CI noise doesn't
//! false-positive — the gate is "did someone accidentally add a syscall
//! or unbounded allocation per call", not micro-benchmarking.

use std::time::{Duration, Instant};

use super::super::JournalEntry;
use super::make_ctx;

/// Building a `JournalEntry` should stay allocation-bounded. The old
/// implementation paid `SystemTime::now()` + a 24-byte `format!` heap alloc
/// per call. Any future change that adds a fresh syscall or string
/// allocation per call would push this past budget.
#[test]
fn journal_entry_new_stays_under_budget() {
    let iterations = 10_000;
    let start = Instant::now();
    for _ in 0..iterations {
        let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
        let _e = JournalEntry::new(ctx, "hit", 0, 1_000_000, None);
    }
    let elapsed = start.elapsed();
    // Budget: 10,000 calls < 100 ms (avg ≤ 10 µs / call). The pre-fix path
    // measured ~3–4 µs / call on Windows; ~0.3 µs / call on Linux. Setting
    // the budget here at ~3× the worst-case observed pre-fix cost catches
    // accidental regressions that add a syscall per call without
    // false-positiving on transient CI jitter.
    assert!(
        elapsed < Duration::from_millis(100),
        "JournalEntry::new regressed beyond budget: {elapsed:?} for {iterations} iterations \
         (avg {:?}/call)",
        elapsed / iterations as u32
    );
}
