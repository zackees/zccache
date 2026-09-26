//! Perf unit test (#1649): a compiler-wrapper process carries no steady CPU
//! cost of its own.
//!
//! kernal-api's native crash capture runs an all-thread pre-crash sampler that
//! takes a full resolved snapshot every 50 ms (about 300 ms of CPU each) and
//! joins an in-flight capture on exit. With it armed in the wrapper, every
//! concurrent compile carried a sampler burning most of a core: a passthrough
//! `zccache <tool>` cost 358 ms instead of 41 ms (release build), cargo's
//! `rustc -vV` probes made the benchmark's warm-target build 240 ms against a
//! 173 ms bare baseline, and a cold `cargo build -p zccache -j4` took
//! 131-146 s instead of 41 s.
//!
//! The wrapped tool is the `zccache` binary's own `--version`, so the test
//! needs no compiler, and `ZCCACHE_DISABLE=1` keeps the daemon out of it: the
//! measurement is the wrapper process alone.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// About 3x the unoptimized post-fix wrapper cost on a quiet host, and well
/// under the ~350 ms a single joined sampler capture adds.
const WRAPPER_PASSTHROUGH_BUDGET: Duration = Duration::from_millis(250);
const RUNS: usize = 7;

/// Extra time a wrapper may spend deriving its development daemon namespace
/// instead of inheriting one. Deriving it from a memoized hash is a stat and
/// one small read; re-hashing the unoptimized test binary (150+ MB) costs
/// 80 ms or more, and the 76 MB release binary 41 ms.
const NAMESPACE_DERIVATION_MARGIN: Duration = Duration::from_millis(30);

/// Namespace handling for one passthrough run.
#[derive(Clone, Copy)]
enum Namespace {
    /// A pinned `ZCCACHE_DAEMON_NAMESPACE`, as soldr and the daemon's own
    /// children inherit it: the wrapper derives nothing.
    Inherited,
    /// No namespace in the environment, as under a plain `RUSTC_WRAPPER`:
    /// a development build derives it from its executable (#1394).
    Derived,
}

fn run_passthrough(zccache: &str, cache_dir: &std::path::Path, namespace: Namespace) -> Duration {
    let mut cmd = Command::new(zccache);
    cmd.arg(zccache)
        .arg("--version")
        .env("ZCCACHE_DISABLE", "1")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env_remove("ZCCACHE_SESSION_ID")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match namespace {
        Namespace::Inherited => cmd.env("ZCCACHE_DAEMON_NAMESPACE", "startup-budget"),
        Namespace::Derived => cmd.env_remove("ZCCACHE_DAEMON_NAMESPACE"),
    };
    let started = Instant::now();
    let status = cmd.status().expect("spawn zccache wrapper");
    let elapsed = started.elapsed();
    assert!(status.success(), "wrapper passthrough failed: {status:?}");
    elapsed
}

/// Median of [`RUNS`] passthroughs after one warm-up run (page cache, and
/// for [`Namespace::Derived`] the first derivation).
fn median_passthrough(
    zccache: &str,
    cache_dir: &std::path::Path,
    namespace: Namespace,
) -> (Duration, Vec<Duration>) {
    run_passthrough(zccache, cache_dir, namespace);
    let mut samples: Vec<Duration> = (0..RUNS)
        .map(|_| run_passthrough(zccache, cache_dir, namespace))
        .collect();
    samples.sort();
    (samples[RUNS / 2], samples)
}

/// Development builds name their daemon after a hash of the executable so two
/// builds never share one (#1394). Cargo starts the wrapper with its own
/// environment, so no namespace is inherited and every `rustc -vV` probe and
/// every compile re-hashed the whole binary: 41 ms per invocation for the
/// 76 MB release binary, which alone made the benchmark's warm-target build
/// miss its 1.3x-of-bare limit (280 ms against 193 ms). The hash is now
/// memoized per executable identity, so only the first invocation pays it.
#[test]
fn development_namespace_is_not_rehashed_per_invocation() {
    let zccache = env!("CARGO_BIN_EXE_zccache");
    let cache_dir = tempfile::tempdir().unwrap();
    let (inherited, inherited_samples) =
        median_passthrough(zccache, cache_dir.path(), Namespace::Inherited);
    let (derived, derived_samples) =
        median_passthrough(zccache, cache_dir.path(), Namespace::Derived);
    eprintln!("inherited namespace: {inherited_samples:?}");
    eprintln!("derived namespace:   {derived_samples:?}");
    assert!(
        derived <= inherited + NAMESPACE_DERIVATION_MARGIN,
        "deriving the development namespace costs {:?} per wrapper invocation \
         (median {derived:?} against {inherited:?} with it inherited); the \
         executable is being re-hashed on the compile hot path (#1649)",
        derived.saturating_sub(inherited)
    );
}

#[test]
fn compiler_wrapper_passthrough_stays_within_budget() {
    let zccache = env!("CARGO_BIN_EXE_zccache");
    let cache_dir = tempfile::tempdir().unwrap();
    let (median, samples) = median_passthrough(zccache, cache_dir.path(), Namespace::Inherited);
    eprintln!("wrapper passthrough samples: {samples:?}");
    assert!(
        median <= WRAPPER_PASSTHROUGH_BUDGET,
        "median wrapper passthrough {median:?} exceeds {WRAPPER_PASSTHROUGH_BUDGET:?}; \
         a steady per-process cost is back on the compile hot path (#1649): {samples:?}"
    );
}
