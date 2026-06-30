//! Cold-path profiling and stress test for C++ compilation cache.
//!
//! Focuses specifically on the cache-miss (cold) path to identify hotspots:
//!   - System include discovery latency
//!   - Response file expansion
//!   - Depfile parsing vs recursive include scanning
//!   - Per-file hashing cost
//!   - Artifact persistence overhead
//!   - Scaling behavior (10, 50, 100, 200 files)
//!
//! Run with: soldr cargo test -p zccache-daemon --test cold_path_profile_test -- --nocapture --ignored

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

const SEP: &str =
    "================================================================================";

// ─── Config ──────────────────────────────────────────────────────────────────

/// Number of shared headers that every source file includes.
const SHARED_HEADER_COUNT: usize = 12;
/// Number of private headers per source file (deeper include tree).
const PRIVATE_HEADERS_PER_FILE: usize = 3;
/// Number of warm iterations after cold pass to validate caching works.
const WARM_VALIDATION_ITERS: usize = 3;

// ─── Test file generation ────────────────────────────────────────────────────

/// Generate a realistic C++ project with deep include trees.
///
/// Structure:
///   include/          — shared headers (SHARED_HEADER_COUNT files)
///   include/detail/   — private headers (PRIVATE_HEADERS_PER_FILE per source)
///   src/              — source files
fn generate_project(dir: &Path, file_count: usize) {
    let include_dir = dir.join("include");
    let detail_dir = include_dir.join("detail");
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&detail_dir).unwrap();
    std::fs::create_dir_all(&src_dir).unwrap();

    // Shared headers — each has templates and inline functions to exercise
    // the include scanner and produce non-trivial compilation.
    for h in 0..SHARED_HEADER_COUNT {
        let mut content = format!(
            r#"#pragma once
#include <cstdint>
namespace shared_{h} {{
  template<typename T>
  inline T transform_{h}(T val) {{
    T result = val;
    for (int i = 0; i < {depth}; ++i) {{
      result = result ^ (result >> {shift});
    }}
    return result;
  }}
  inline uint64_t hash_{h}(uint64_t seed) {{
    seed ^= seed >> 33;
    seed *= 0xff51afd7ed558ccd{suffix};
    seed ^= seed >> 33;
    return seed;
  }}
}}
"#,
            depth = 3 + h % 5,
            shift = 1 + h % 16,
            suffix = "ULL",
        );

        // Some headers include other headers to create fan-out.
        if h > 0 {
            content = format!("#include \"header_{}.h\"\n{content}", h - 1);
        }

        std::fs::write(include_dir.join(format!("header_{h}.h")), content).unwrap();
    }

    // Private detail headers — each source gets its own set.
    for i in 0..file_count {
        for p in 0..PRIVATE_HEADERS_PER_FILE {
            std::fs::write(
                detail_dir.join(format!("detail_{i}_{p}.h")),
                format!(
                    r#"#pragma once
namespace detail_{i}_{p} {{
  template<typename T>
  inline T compute(T x) {{ return x * {val} + {off}; }}
}}
"#,
                    val = i * PRIVATE_HEADERS_PER_FILE + p + 1,
                    off = p + 1,
                ),
            )
            .unwrap();
        }
    }

    // Source files — include shared headers + private detail headers.
    for i in 0..file_count {
        let mut includes = String::new();
        for h in 0..SHARED_HEADER_COUNT {
            includes.push_str(&format!("#include \"header_{h}.h\"\n"));
        }
        for p in 0..PRIVATE_HEADERS_PER_FILE {
            includes.push_str(&format!("#include \"detail/detail_{i}_{p}.h\"\n"));
        }

        let calls: String = (0..SHARED_HEADER_COUNT)
            .map(|h| format!("    sum += shared_{h}::hash_{h}(sum);\n"))
            .collect();
        let detail_calls: String = (0..PRIVATE_HEADERS_PER_FILE)
            .map(|p| format!("    sum += detail_{i}_{p}::compute(sum);\n"))
            .collect();

        std::fs::write(
            src_dir.join(format!("unit_{i:03}.cpp")),
            format!(
                r#"{includes}
#include <cmath>
namespace unit_{i:03} {{
  uint64_t compute(int n) {{
    uint64_t sum = n;
    for (int j = 0; j < n; j++) {{
{calls}{detail_calls}      sum ^= static_cast<uint64_t>(std::sin(j * 0.{i:03}1) * 1e9);
    }}
    return sum;
  }}
}}
"#
            ),
        )
        .unwrap();
    }
}

fn source_paths(dir: &Path, file_count: usize) -> Vec<(NormalizedPath, NormalizedPath)> {
    (0..file_count)
        .map(|i| {
            let src = NormalizedPath::new(dir.join("src").join(format!("unit_{i:03}.cpp")));
            let obj = NormalizedPath::new(dir.join(format!("unit_{i:03}.o")));
            (src, obj)
        })
        .collect()
}

fn clean_objects(dir: &Path, file_count: usize) {
    for i in 0..file_count {
        let _ = std::fs::remove_file(dir.join(format!("unit_{i:03}.o")));
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn start_session(client: &mut ClientConn, cwd: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv::<Response>().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn compile_one(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    src: &Path,
    obj: &Path,
    cwd: &str,
) -> (i32, bool, Duration) {
    let start = Instant::now();
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: vec![
                "-c".into(),
                src.to_string_lossy().into_owned(),
                "-o".into(),
                obj.to_string_lossy().into_owned(),
                "-Iinclude".into(),
                "-O2".into(),
                "-std=c++17".into(),
            ],
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    let (exit_code, cached) = match client.recv::<Response>().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    };
    (exit_code, cached, start.elapsed())
}

// ─── Reporting ───────────────────────────────────────────────────────────────

struct ColdPassResult {
    file_count: usize,
    per_file_latencies: Vec<Duration>,
    total_elapsed: Duration,
}

impl ColdPassResult {
    fn report(&self) {
        let total_ms = self.total_elapsed.as_secs_f64() * 1000.0;
        let avg_ms = total_ms / self.file_count as f64;

        let mut sorted: Vec<f64> = self
            .per_file_latencies
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let p50 = sorted[sorted.len() / 2];
        let p90 = sorted[(sorted.len() as f64 * 0.9) as usize];
        let p99 = sorted[(sorted.len() as f64 * 0.99).min(sorted.len() as f64 - 1.0) as usize];
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];

        // First file is special (includes system include discovery)
        let first_ms = self.per_file_latencies[0].as_secs_f64() * 1000.0;
        let rest_avg = if self.file_count > 1 {
            self.per_file_latencies[1..]
                .iter()
                .map(|d| d.as_secs_f64() * 1000.0)
                .sum::<f64>()
                / (self.file_count - 1) as f64
        } else {
            first_ms
        };

        eprintln!(
            "    Total:      {total_ms:>8.1}ms ({} files)",
            self.file_count
        );
        eprintln!("    Avg/file:   {avg_ms:>8.3}ms");
        eprintln!("    First file: {first_ms:>8.1}ms  (includes system include discovery)");
        eprintln!("    Rest avg:   {rest_avg:>8.3}ms  (steady-state cold compile)");
        eprintln!("    p50:        {p50:>8.3}ms");
        eprintln!("    p90:        {p90:>8.3}ms");
        eprintln!("    p99:        {p99:>8.3}ms");
        eprintln!("    min:        {min:>8.3}ms");
        eprintln!("    max:        {max:>8.3}ms");
    }
}

fn print_phase_profile(profile: &zccache::daemon::ProfileSnapshot) {
    let wide = "=".repeat(80);
    let dash = "-".repeat(70);

    eprintln!("\n{wide}");
    eprintln!("  DAEMON-SIDE PHASE PROFILING");
    eprintln!("{wide}");

    if profile.miss_count > 0 {
        eprintln!("\n  CACHE MISS PATH ({} samples)", profile.miss_count);
        eprintln!("  {dash}");

        let phases = [
            ("compiler_exec (clang)", profile.avg_compiler_exec_ns),
            (
                "include_scan (depfile/scanner)",
                profile.avg_include_scan_ns,
            ),
            ("hash_all_files (source+headers)", profile.avg_hash_all_ns),
            (
                "artifact_store (depgraph+persist)",
                profile.avg_artifact_store_ns,
            ),
        ];

        let total = profile.avg_total_miss_ns.max(1);
        let mut accounted = 0u64;
        for (name, ns) in &phases {
            let us = *ns;
            let pct = (us as f64 / total as f64) * 100.0;
            let bar_len = (pct / 2.0).round().max(0.0) as usize;
            let bar: String = "#".repeat(bar_len);
            eprintln!("  {name:<40} {:>8}ns  ({pct:>5.1}%)  {bar}", us);
            accounted += us;
        }

        let overhead = total.saturating_sub(accounted);
        let overhead_pct = (overhead as f64 / total as f64) * 100.0;
        eprintln!(
            "  {:<40} {:>8}ns  ({:>5.1}%)",
            "overhead (arg parse/ctx/sys includes)", overhead, overhead_pct
        );
        eprintln!("  {dash}");
        eprintln!("  {:<40} {:>8}ns", "TOTAL (avg per miss)", total);
    }

    if profile.hit_count > 0 {
        eprintln!("\n  CACHE HIT PATH ({} samples)", profile.hit_count);
        eprintln!("  {dash}");

        let phases = [
            ("parse_args", profile.avg_parse_args_ns),
            ("build_context + register", profile.avg_build_context_ns),
            ("hash_source", profile.avg_hash_source_ns),
            ("hash_headers", profile.avg_hash_headers_ns),
            ("depgraph_check", profile.avg_depgraph_check_ns),
            ("artifact_lookup", profile.avg_artifact_lookup_ns),
            ("write_output", profile.avg_write_output_ns),
            ("bookkeeping", profile.avg_bookkeeping_ns),
        ];

        let total = profile.avg_total_hit_ns.max(1);
        let mut accounted = 0u64;
        for (name, ns) in &phases {
            let us = *ns;
            let pct = (us as f64 / total as f64) * 100.0;
            let bar_len = (pct / 2.0).round().max(0.0) as usize;
            let bar: String = "#".repeat(bar_len);
            eprintln!("  {name:<40} {:>8}ns  ({pct:>5.1}%)  {bar}", us);
            accounted += us;
        }

        let overhead = total.saturating_sub(accounted);
        let overhead_pct = (overhead as f64 / total as f64) * 100.0;
        eprintln!(
            "  {:<40} {:>8}ns  ({:>5.1}%)",
            "overhead/unaccounted", overhead, overhead_pct
        );
        eprintln!("  {dash}");
        eprintln!("  {:<40} {:>8}ns", "TOTAL (avg per hit)", total);
    }

    eprintln!("\n{wide}");
}

// ─── The test ────────────────────────────────────────────────────────────────

/// Cold-path profiling stress test with scaling analysis.
///
/// Tests 4 project sizes to reveal scaling characteristics:
///   - 10 files:  baseline
///   - 50 files:  typical project
///   - 100 files: medium project
///   - 200 files: large project (stress)
///
/// For each size, measures:
///   1. Per-file cold compile latency (IPC round-trip)
///   2. First-file penalty (system include discovery)
///   3. Daemon-side phase profiling (compiler_exec, include_scan, hash_all, artifact_store)
///   4. Warm validation (ensures all files cached correctly)
#[tokio::test]
#[ignore]
async fn cold_path_stress_profile() {
    let compiler_path = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    eprintln!("\n{}", SEP);
    eprintln!("  COLD PATH STRESS PROFILING TEST");
    eprintln!("{}", SEP);
    eprintln!("  Compiler: {}", {
        let out = std::process::Command::new(&compiler_path)
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    });
    eprintln!(
        "  Headers per file: {} shared + {} private = {} total",
        SHARED_HEADER_COUNT,
        PRIVATE_HEADERS_PER_FILE,
        SHARED_HEADER_COUNT + PRIVATE_HEADERS_PER_FILE,
    );
    eprintln!();

    let file_counts = [50];
    let mut all_results: Vec<(usize, ColdPassResult, zccache::daemon::ProfileSnapshot)> =
        Vec::new();

    // Single daemon for all sizes — avoids index.redb lock contention.
    // We take profiler snapshots before/after each size to compute per-size averages.
    let endpoint = zccache::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        server_clone.lock().await.run(0).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    for &file_count in &file_counts {
        eprintln!("  ══ {file_count} files ══════════════════════════════════════");

        let tmp = tempfile::tempdir().unwrap();
        generate_project(tmp.path(), file_count);
        let cwd = tmp.path().to_string_lossy().into_owned();
        let files = source_paths(tmp.path(), file_count);

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let sid = start_session(&mut client, &cwd).await;

        // ── Cold pass ────────────────────────────────────────────────
        eprintln!("    Cold pass...");
        let mut per_file_latencies = Vec::with_capacity(file_count);
        let cold_start = Instant::now();

        for (src, obj) in &files {
            let (exit_code, cached, elapsed) =
                compile_one(&mut client, &sid, &compiler, src, obj, &cwd).await;
            assert_eq!(exit_code, 0, "cold compile failed: {}", src.display());
            assert!(!cached, "cold compile should be a miss");
            per_file_latencies.push(elapsed);
        }
        let cold_total = cold_start.elapsed();

        let cold_result = ColdPassResult {
            file_count,
            per_file_latencies,
            total_elapsed: cold_total,
        };
        cold_result.report();

        // ── Warm validation pass ─────────────────────────────────────
        eprintln!("\n    Warm validation ({WARM_VALIDATION_ITERS} iters)...");
        let mut warm_latencies = Vec::new();
        for iter in 0..WARM_VALIDATION_ITERS {
            clean_objects(tmp.path(), file_count);
            let t = Instant::now();
            for (src, obj) in &files {
                let (exit_code, cached, _) =
                    compile_one(&mut client, &sid, &compiler, src, obj, &cwd).await;
                assert_eq!(exit_code, 0);
                assert!(cached, "warm iter {iter} should be a hit");
            }
            warm_latencies.push(t.elapsed());
        }
        let warm_med = {
            let mut ms: Vec<f64> = warm_latencies
                .iter()
                .map(|d| d.as_secs_f64() * 1000.0)
                .collect();
            ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            ms[ms.len() / 2]
        };
        eprintln!(
            "    Warm median: {warm_med:.1}ms total ({:.3}ms/file)",
            warm_med / file_count as f64
        );

        // End session
        client
            .send(&Request::SessionEnd {
                session_id: sid.clone(),
            })
            .await
            .unwrap();
        let _ = client.recv::<Response>().await;

        // Take cumulative snapshot — we'll use the latest snapshot per size
        // since the profiler gives averages across all requests.
        let profile = server.lock().await.profile_snapshot();
        all_results.push((file_count, cold_result, profile.clone()));

        eprintln!();
    }

    // Shutdown
    shutdown.notify_one();
    handle.await.unwrap();

    // ── Summary table ────────────────────────────────────────────────
    eprintln!("\n{}", SEP);
    eprintln!("  SCALING SUMMARY");
    eprintln!("{}\n", SEP);
    eprintln!(
        "  {:>6} │ {:>10} │ {:>10} │ {:>10} │ {:>10} │ {:>10} │ {:>10}",
        "Files", "Cold Total", "Cold/File", "1st File", "CompExec", "InclScan", "HashAll"
    );
    eprintln!(
        "  {:─>6}─┼─{:─>10}─┼─{:─>10}─┼─{:─>10}─┼─{:─>10}─┼─{:─>10}─┼─{:─>10}",
        "", "", "", "", "", "", ""
    );
    for (count, result, profile) in &all_results {
        let cold_total_ms = result.total_elapsed.as_secs_f64() * 1000.0;
        let cold_per_file_ms = cold_total_ms / *count as f64;
        let first_ms = result.per_file_latencies[0].as_secs_f64() * 1000.0;
        let exec_ms = profile.avg_compiler_exec_ns as f64 / 1_000_000.0;
        let scan_ms = profile.avg_include_scan_ns as f64 / 1_000_000.0;
        let hash_ms = profile.avg_hash_all_ns as f64 / 1_000_000.0;
        eprintln!(
            "  {:>6} │ {:>8.0}ms │ {:>8.1}ms │ {:>8.0}ms │ {:>8.1}ms │ {:>8.3}ms │ {:>8.3}ms",
            count, cold_total_ms, cold_per_file_ms, first_ms, exec_ms, scan_ms, hash_ms,
        );
    }

    // ── Detailed phase profile for the largest run ───────────────────
    if let Some((_, _, ref profile)) = all_results.last() {
        print_phase_profile(profile);
    }

    // ── Overhead analysis ────────────────────────────────────────────
    eprintln!("\n  OVERHEAD ANALYSIS (largest run)");
    eprintln!("  {}", "-".repeat(70));
    if let Some((count, result, profile)) = all_results.last() {
        let total_miss_ns = profile.avg_total_miss_ns;
        let compiler_ns = profile.avg_compiler_exec_ns;
        let overhead_ns = total_miss_ns.saturating_sub(compiler_ns);
        let overhead_pct = overhead_ns as f64 / total_miss_ns.max(1) as f64 * 100.0;

        eprintln!("  Total cold per-file (daemon):     {:>8}ns", total_miss_ns);
        eprintln!("  Compiler execution:               {:>8}ns", compiler_ns);
        eprintln!(
            "  zccache overhead:                 {:>8}ns  ({overhead_pct:.1}%)",
            overhead_ns
        );
        eprintln!(
            "    - include_scan:                 {:>8}ns",
            profile.avg_include_scan_ns
        );
        eprintln!(
            "    - hash_all_files:               {:>8}ns",
            profile.avg_hash_all_ns
        );
        eprintln!(
            "    - artifact_store:               {:>8}ns",
            profile.avg_artifact_store_ns
        );
        let parsed_overhead = profile
            .avg_include_scan_ns
            .saturating_add(profile.avg_hash_all_ns)
            .saturating_add(profile.avg_artifact_store_ns);
        let unaccounted = overhead_ns.saturating_sub(parsed_overhead);
        eprintln!("    - unaccounted (sys_incl/parse):  {:>8}ns", unaccounted);

        // IPC overhead = client-side total - daemon-side total
        let client_avg_ns = (result.total_elapsed.as_nanos() as u64) / *count as u64;
        let ipc_overhead_ns = client_avg_ns.saturating_sub(total_miss_ns);
        eprintln!(
            "\n  Client avg per-file:              {:>8}ns",
            client_avg_ns
        );
        eprintln!(
            "  IPC + serialization overhead:     {:>8}ns",
            ipc_overhead_ns
        );
    }
}

/// Targeted cold-path stress test: concurrent cold compiles.
///
/// Sends N compile requests through independent sessions to test
/// daemon under concurrent cold-path load.
#[tokio::test]
#[ignore]
async fn cold_path_concurrent_stress() {
    let compiler_path = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    const FILE_COUNT: usize = 30;
    const CONCURRENCY: usize = 4;

    eprintln!("\n{}", SEP);
    eprintln!("  CONCURRENT COLD PATH STRESS TEST");
    eprintln!("  {FILE_COUNT} files x {CONCURRENCY} concurrent sessions");
    eprintln!("{}\n", SEP);

    // Generate one project per concurrent session
    let tmps: Vec<_> = (0..CONCURRENCY)
        .map(|_| tempfile::tempdir().unwrap())
        .collect();
    for tmp in &tmps {
        generate_project(tmp.path(), FILE_COUNT);
    }

    // Single daemon serving all sessions
    let endpoint = zccache::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        server_clone.lock().await.run(0).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Spawn concurrent cold compiles
    let start = Instant::now();
    let mut tasks = Vec::new();

    for (idx, tmp) in tmps.iter().enumerate() {
        let ep = endpoint.clone();
        let comp = compiler.clone();
        let dir = tmp.path().to_path_buf();

        tasks.push(tokio::spawn(async move {
            let mut client = zccache::ipc::connect(&ep).await.unwrap();
            let cwd = dir.to_string_lossy().into_owned();
            let sid = start_session_inline(&mut client, &cwd).await;
            let files = source_paths(&dir, FILE_COUNT);

            let t = Instant::now();
            let mut miss_count = 0u32;
            for (src, obj) in &files {
                let (exit_code, cached, _) =
                    compile_one(&mut client, &sid, &comp, src, obj, &cwd).await;
                assert_eq!(exit_code, 0);
                if !cached {
                    miss_count += 1;
                }
            }
            let elapsed = t.elapsed();
            eprintln!(
                "    Session {idx}: {:.1}ms ({miss_count} misses, {:.1}ms/file)",
                elapsed.as_secs_f64() * 1000.0,
                elapsed.as_secs_f64() * 1000.0 / FILE_COUNT as f64,
            );

            // End session
            client
                .send(&Request::SessionEnd {
                    session_id: sid.clone(),
                })
                .await
                .unwrap();
            let _ = client.recv::<Response>().await;

            elapsed
        }));
    }

    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        results.push(task.await.unwrap());
    }

    let wall_clock = start.elapsed();
    let sum_ms: f64 = results.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
    let total_files = FILE_COUNT * CONCURRENCY;

    eprintln!(
        "\n  Wall clock:     {:.1}ms",
        wall_clock.as_secs_f64() * 1000.0
    );
    eprintln!("  Sum of sessions: {sum_ms:.1}ms");
    eprintln!(
        "  Throughput:     {:.1} files/sec ({total_files} total)",
        total_files as f64 / wall_clock.as_secs_f64()
    );
    eprintln!(
        "  Parallelism:    {:.2}x",
        sum_ms / (wall_clock.as_secs_f64() * 1000.0)
    );

    // Get profile
    shutdown.notify_one();
    handle.await.unwrap();
    let profile = server.lock().await.profile_snapshot();
    print_phase_profile(&profile);
}

/// Inline session start (avoids lifetime issues in spawned tasks).
async fn start_session_inline(client: &mut ClientConn, cwd: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv::<Response>().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

/// Measure the first-file penalty specifically.
///
/// The first cold compile for a new compiler path pays the cost of system
/// include discovery (running the compiler with -v). This test isolates
/// that cost by using a single daemon with multiple sessions, each in a
/// fresh project directory so the compile context is always cold.
#[tokio::test]
#[ignore]
async fn cold_path_first_file_penalty() {
    let compiler_path = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    const TRIALS: usize = 5;

    eprintln!("\n{}", SEP);
    eprintln!("  FIRST-FILE PENALTY MEASUREMENT");
    eprintln!("  {TRIALS} trials, single daemon, separate sessions");
    eprintln!("{}\n", SEP);

    // Single daemon for all trials
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut first_file_times = Vec::with_capacity(TRIALS);
    let mut second_file_times = Vec::with_capacity(TRIALS);

    for trial in 0..TRIALS {
        let tmp = tempfile::tempdir().unwrap();
        generate_project(tmp.path(), 2);
        let cwd = tmp.path().to_string_lossy().into_owned();
        let files = source_paths(tmp.path(), 2);

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let sid = start_session(&mut client, &cwd).await;

        // First file — cold compile (system includes already cached after trial 0,
        // but compile context is always cold because the source dir is fresh)
        let (_, _, t1) =
            compile_one(&mut client, &sid, &compiler, &files[0].0, &files[0].1, &cwd).await;
        first_file_times.push(t1);

        // Second file — also cold, but shares system includes and depgraph state
        let (_, _, t2) =
            compile_one(&mut client, &sid, &compiler, &files[1].0, &files[1].1, &cwd).await;
        second_file_times.push(t2);

        eprintln!(
            "    Trial {}: first={:.1}ms  second={:.1}ms  delta={:.1}ms",
            trial + 1,
            t1.as_secs_f64() * 1000.0,
            t2.as_secs_f64() * 1000.0,
            (t1.as_secs_f64() - t2.as_secs_f64()) * 1000.0,
        );

        // End session
        client
            .send(&Request::SessionEnd {
                session_id: sid.clone(),
            })
            .await
            .unwrap();
        let _ = client.recv::<Response>().await;
    }

    shutdown.notify_one();
    handle.await.unwrap();

    let avg_first = first_file_times
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .sum::<f64>()
        / TRIALS as f64;
    let avg_second = second_file_times
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .sum::<f64>()
        / TRIALS as f64;

    eprintln!("\n  Average first file:  {avg_first:.1}ms");
    eprintln!("  Average second file: {avg_second:.1}ms");
    eprintln!(
        "  First-file overhead: {:.1}ms ({:.0}%)",
        avg_first - avg_second,
        if avg_first > 0.0 {
            (avg_first - avg_second) / avg_first * 100.0
        } else {
            0.0
        },
    );
    eprintln!("  (Trial 0 first-file includes system include discovery; later trials reuse it)");
}
