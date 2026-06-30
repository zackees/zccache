//! Wrapper-based probe-storm benchmark — validates the #625 fast-path
//! end-to-end (issue #625, follow-up to #633).
//!
//! Why this exists separately from `tests_probe_storm.rs`:
//! that bench talks to the daemon directly via `Request::Compile`,
//! which never goes through `cli::commands::wrap::routing::
//! classify_invocation`. So setting `ZCCACHE_PROBE_BYPASS=1` has zero
//! effect on it. This bench invokes `zccache <compiler> -c probe.c
//! -o probe.o` as a subprocess — exercising the actual production
//! wrapper path that downstream consumers (fastled, soldr) use.
//!
//! The bench measures three modes per probe storm:
//!
//! 1. **Bare compiler** — no zccache. The floor we want to match.
//! 2. **zccache wrapper, cache mode** (no env var) — today's behaviour.
//!    Pays IPC + key compute + write per probe.
//! 3. **zccache wrapper, bypass mode** (`ZCCACHE_PROBE_BYPASS=1`) —
//!    the fix landed in #633. Routes `ProbeBypass`, exec's the
//!    compiler directly with zero IPC.
//!
//! The bench asserts mode 3 ≤ mode 2 (the fix must not be a regression
//! over the cached path). The bench also prints per-probe wall-time for
//! the perf-cluster operator to inspect.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::common::{fmt_dur, fmt_ratio, median, WARM_TRIALS};

/// Number of unique probe TUs. Smaller than `PROBE_COUNT` in
/// `tests_probe_storm.rs` because each compile here is a real
/// subprocess invocation (~10–30 ms fork/exec overhead on Windows),
/// not an in-process IPC roundtrip. Twenty probes still demonstrates
/// the per-probe delta clearly while keeping bench wall-time low.
const PROBE_COUNT: usize = 20;

/// Locate the `zccache` binary built alongside this test binary.
/// Mirrors the pattern used by `tests/cli_kv.rs`: walk from
/// `current_exe()` (the perf_bench_test binary) up to the per-target
/// `deps/` directory, then up one more level to the build profile
/// directory containing `zccache.exe`.
fn zccache_bin() -> PathBuf {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("target dir")
        .to_path_buf();
    if cfg!(windows) {
        path.push("zccache.exe");
    } else {
        path.push("zccache");
    }
    assert!(
        path.exists(),
        "zccache binary not found at {path:?}. Run `soldr cargo build -p zccache --bin zccache` first."
    );
    path
}

/// Generate PROBE_COUNT unique probes using `trial_id` as a salt. Different
/// `trial_id`s produce probes with different content AND different paths,
/// so the cache cannot hit across trials. This matches what meson actually
/// does at configure time: every probe is a freshly-named temp dir with
/// freshly-generated content. Measuring "warm trials" against the SAME
/// probes is a no-op because each subsequent trial is a 100% cache hit
/// (hardlink ≈ 13 ms / probe) and tells us nothing about the configure-
/// time workload we care about.
fn generate_probes(root: &Path, trial_id: u64) -> Vec<PathBuf> {
    std::fs::create_dir_all(root).unwrap();
    let mut sources = Vec::with_capacity(PROBE_COUNT);
    for i in 0..PROBE_COUNT {
        let probe_dir = root.join(format!("trial_{trial_id}_probe_{i:03}"));
        std::fs::create_dir_all(&probe_dir).unwrap();
        let probe_src = probe_dir.join("probe.c");
        std::fs::write(
            &probe_src,
            format!(
                "/* probe trial={trial_id} idx={i} */\n\
                 int probe_{trial_id}_{i:03}(int x) {{ return x + {i} + {trial_id} * 31; }}\n",
            ),
        )
        .unwrap();
        sources.push(probe_src);
    }
    sources
}

fn clean_outputs(sources: &[PathBuf]) {
    for src in sources {
        let _ = std::fs::remove_file(src.with_extension("o"));
    }
}

/// Run one storm of probes via bare compiler invocations.
fn bare_storm(compiler: &str, sources: &[PathBuf]) -> Duration {
    clean_outputs(sources);
    let start = Instant::now();
    for src in sources {
        let obj = src.with_extension("o");
        let output = Command::new(compiler)
            .args(["-c", "-O0"])
            .arg(src)
            .arg("-o")
            .arg(&obj)
            .output()
            .expect("bare clang spawn");
        assert!(
            output.status.success(),
            "bare compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    start.elapsed()
}

/// Run one storm of probes via `zccache <compiler> ...`, with the
/// supplied env vars (e.g. cache dir, BYPASS toggle).
fn zccache_storm(
    zccache: &Path,
    compiler: &str,
    sources: &[PathBuf],
    env: &[(&str, &str)],
) -> Duration {
    clean_outputs(sources);
    let start = Instant::now();
    for src in sources {
        let obj = src.with_extension("o");
        let mut cmd = Command::new(zccache);
        cmd.arg(compiler)
            .args(["-c", "-O0"])
            .arg(src)
            .arg("-o")
            .arg(&obj);
        // Detach from any soldr-injected `ZCCACHE_SESSION_ID` in this test
        // process's env. With a session id set, the wrapper takes the
        // `cmd_compile` path which assumes the daemon at that endpoint is
        // already running. Our isolated `ZCCACHE_CACHE_DIR` points at a
        // different endpoint, so the session would be stale; routing to
        // `cmd_compile_ephemeral` (which auto-starts a daemon) is what we
        // actually want for the bench.
        cmd.env_remove("ZCCACHE_SESSION_ID");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let output = cmd.output().expect("zccache wrapper spawn");
        assert!(
            output.status.success(),
            "zccache wrapper compile failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    start.elapsed()
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache --test perf_bench_test -- perf_meson_probe_storm_wrapper --nocapture --ignored
async fn perf_meson_probe_storm_wrapper() {
    zccache::test_support::ensure_clang_tool_chain_on_path();
    let compiler_path = match zccache::test_support::find_on_path("clang") {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let zccache = zccache_bin();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  MESON PROBE-STORM WRAPPER BENCHMARK (issue #625)");
    eprintln!("  {PROBE_COUNT} tiny probes | wrapper-spawn-per-probe");
    eprintln!("  Compiler: {compiler}");
    eprintln!("  zccache:  {}", zccache.display());
    eprintln!("================================================================");
    eprintln!();

    // Each mode runs (1 + WARM_TRIALS) storms. Every storm uses
    // freshly-generated probe sources so the cache cannot hit across
    // storms (matches what meson does at configure time). Daemon-start
    // and cargo-cold costs land on the "cold" measurement; subsequent
    // storms see steady-state daemon overhead.
    let trial_counter = std::sync::atomic::AtomicU64::new(0);
    let next_trial = || trial_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let run_mode = |label: &str, env: &[(&str, &str)]| -> (Duration, Vec<Duration>) {
        eprintln!("  {label}");
        let dir = zccache::test_support::temp_cache_dir().unwrap();
        let probes = generate_probes(dir.path(), next_trial());
        let cold = if env.is_empty() {
            bare_storm(&compiler, &probes)
        } else {
            zccache_storm(&zccache, &compiler, &probes, env)
        };
        eprintln!(
            "        cold:  {} ({} per probe)",
            fmt_dur(cold),
            fmt_dur(cold / PROBE_COUNT as u32),
        );
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            // Fresh probes so each trial is a cache miss (meson-realistic).
            let trial_dir = zccache::test_support::temp_cache_dir().unwrap();
            let trial_probes = generate_probes(trial_dir.path(), next_trial());
            let t = if env.is_empty() {
                bare_storm(&compiler, &trial_probes)
            } else {
                zccache_storm(&zccache, &compiler, &trial_probes, env)
            };
            warm.push(t);
            // Hold the dir until the trial ends, then drop.
            drop(trial_dir);
        }
        let warm_med = median(&warm);
        eprintln!(
            "        warm/probe (median):  {}",
            fmt_dur(warm_med / PROBE_COUNT as u32),
        );
        eprintln!();
        drop(dir);
        (cold, warm)
    };

    let (bl_cold, bl_warm) = run_mode("[1/3] Bare compiler", &[]);

    let zc_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
    let zc_cache_str = zc_cache_dir.path().to_string_lossy().into_owned();
    let cache_env: Vec<(&str, &str)> = vec![("ZCCACHE_CACHE_DIR", zc_cache_str.as_str())];
    let (cache_cold, cache_warm) = run_mode(
        "[2/3] zccache wrapper, cache mode (ZCCACHE_PROBE_BYPASS unset)",
        &cache_env,
    );

    let bp_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
    let bp_cache_str = bp_cache_dir.path().to_string_lossy().into_owned();
    let bypass_env: Vec<(&str, &str)> = vec![
        ("ZCCACHE_CACHE_DIR", bp_cache_str.as_str()),
        ("ZCCACHE_PROBE_BYPASS", "1"),
    ];
    let (bypass_cold, bypass_warm) = run_mode(
        "[3/3] zccache wrapper, bypass mode (ZCCACHE_PROBE_BYPASS=1)",
        &bypass_env,
    );

    let bl_warm_med = median(&bl_warm);
    let cache_warm_med = median(&cache_warm);
    let bypass_warm_med = median(&bypass_warm);

    // --- Report -----------------------------------------------------
    eprintln!("## Wrapper Probe-Storm Benchmark: {PROBE_COUNT} unique probes per trial, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Mode | Bare | zccache (cache) | zccache (bypass) |");
    eprintln!("|:-----|----:|----------------:|-----------------:|");
    eprintln!(
        "| Storm, Cold | {} | {} | **{}** |",
        fmt_dur(bl_cold),
        fmt_dur(cache_cold),
        fmt_dur(bypass_cold),
    );
    eprintln!(
        "| Storm, Warm | {} | {} | **{}** |",
        fmt_dur(bl_warm_med),
        fmt_dur(cache_warm_med),
        fmt_dur(bypass_warm_med),
    );
    eprintln!();
    eprintln!(
        "> Bypass vs cache (warm): {}  |  Bypass vs bare (warm): {}",
        fmt_ratio(cache_warm_med, bypass_warm_med, true),
        fmt_ratio(bl_warm_med, bypass_warm_med, true),
    );
    eprintln!();

    // --- Behavioural assertion -------------------------------------
    // With every probe genuinely uncached (the meson configure shape),
    // bypass must be no slower than the cached path — otherwise the
    // #625 fast-path is a regression instead of a fix. Generous noise
    // band (1.3×) so a busy CI runner doesn't false-fail; this is
    // about catching big regressions, not microbenchmark fights.
    assert!(
        bypass_warm_med <= cache_warm_med + cache_warm_med / 3,
        "regression guard: bypass mode ({}) is more than 30% slower than \
         cache mode ({}) on the unique-probe (cache-miss) workload — the \
         #625 fast-path is supposed to beat the cache path here, not lose to it",
        fmt_dur(bypass_warm_med),
        fmt_dur(cache_warm_med),
    );
}
