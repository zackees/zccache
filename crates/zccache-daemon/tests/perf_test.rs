//! Performance comparison: bare clang vs sccache vs zccache.
//!
//! Three-way benchmark measuring compile latency across cache-miss and cache-hit scenarios.
//! Run with: soldr cargo test -p zccache-daemon --test perf_test -- --nocapture --ignored

use std::time::Instant;
use zccache_monocrate::core::NormalizedPath;
use zccache_daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache_monocrate::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_monocrate::ipc::IpcClientConnection;

// ─── Config ──────────────────────────────────────────────────────────────────

const FILE_COUNT: usize = 50;
const WARM_ITERATIONS: usize = 100;
const BARE_ITERATIONS: usize = 20; // bare clang is slow, fewer iterations needed

// ─── Tool discovery ──────────────────────────────────────────────────────────

fn find_sccache() -> Option<NormalizedPath> {
    for path in &[
        "sccache",
        "sccache.exe",
        "/c/tools/python13/Scripts/sccache",
    ] {
        if let Ok(output) = std::process::Command::new(path).arg("--version").output() {
            if output.status.success() {
                return Some(NormalizedPath::new(path));
            }
        }
    }
    None
}

fn sccache_version(sccache: &std::path::Path) -> String {
    std::process::Command::new(sccache)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn clang_version(clang: &std::path::Path) -> String {
    std::process::Command::new(clang)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

// ─── Daemon helpers ──────────────────────────────────────────────────────────

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn start_session(
    client: &mut ClientConn,
    _clang: &std::path::Path,
    cwd: &str,
    log_file: &str,
) -> (String, String) {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: Some(log_file.to_string().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    let compiler = _clang.to_string_lossy().into_owned();
    (session_id, compiler)
}

async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

// ─── Test file generation ────────────────────────────────────────────────────

/// Generate realistic test source files with headers and cross-references.
fn generate_test_files(dir: &std::path::Path, count: usize) {
    // Shared headers
    std::fs::write(
        dir.join("common.h"),
        r#"#pragma once
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

inline int common_add(int a, int b) { return a + b; }
inline int common_mul(int a, int b) { return a * b; }
"#,
    )
    .unwrap();

    std::fs::write(
        dir.join("math_utils.h"),
        r#"#pragma once

template<typename T>
T clamp(T val, T lo, T hi) {
    return val < lo ? lo : (val > hi ? hi : val);
}

template<typename T>
T lerp(T a, T b, float t) {
    return static_cast<T>(a + (b - a) * t);
}

inline unsigned int hash_combine(unsigned int a, unsigned int b) {
    return a ^ (b + 0x9e3779b9 + (a << 6) + (a >> 2));
}
"#,
    )
    .unwrap();

    for i in 0..count {
        let src = dir.join(format!("file_{i}.cpp"));
        std::fs::write(
            &src,
            format!(
                r#"#include "common.h"
#include "math_utils.h"

namespace ns_{i} {{

struct Data_{i} {{
    int values[16];
    int count;

    int sum() const {{
        int s = 0;
        for (int j = 0; j < count; j++) {{
            s = common_add(s, values[j]);
        }}
        return s;
    }}

    int product() const {{
        int p = 1;
        for (int j = 0; j < count; j++) {{
            p = common_mul(p, values[j]);
        }}
        return p;
    }}
}};

static int compute_{i}(int x) {{
    Data_{i} d;
    d.count = clamp(x, 0, 16);
    for (int j = 0; j < d.count; j++) {{
        d.values[j] = common_add(j, {i});
    }}
    unsigned int h = 0;
    for (int j = 0; j < d.count; j++) {{
        h = hash_combine(h, static_cast<unsigned int>(d.values[j]));
    }}
    return static_cast<int>(h) + d.sum() + d.product();
}}

}} // namespace ns_{i}

int func_{i}() {{
    return ns_{i}::compute_{i}(10);
}}
"#
            ),
        )
        .unwrap();
    }
}

// ─── Benchmark runners ──────────────────────────────────────────────────────

struct BenchResult {
    label: String,
    cold_ms: Vec<f64>,
    warm_ms: Vec<f64>,
}

/// Benchmark bare clang: direct `clang++ -c file.cpp -o file.o` with no cache.
fn bench_bare_clang(
    clang: &std::path::Path,
    src_dir: &std::path::Path,
    file_count: usize,
    iterations: usize,
) -> BenchResult {
    let cwd = src_dir.to_string_lossy().into_owned();
    let mut all_ms = Vec::new();

    for iter in 0..iterations {
        for i in 0..file_count {
            let src = format!("file_{i}.cpp");
            let obj = format!("file_{i}.o");
            let _ = std::fs::remove_file(src_dir.join(&obj));

            let start = Instant::now();
            let output = std::process::Command::new(clang)
                .args(["-c", &src, "-o", &obj])
                .current_dir(&cwd)
                .output()
                .unwrap();
            let elapsed = start.elapsed();

            assert!(
                output.status.success(),
                "bare clang failed for {src}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            all_ms.push(elapsed.as_secs_f64() * 1000.0);
        }
        if (iter + 1) % 5 == 0 {
            eprint!("    bare clang: {}/{iterations} iterations\r", iter + 1);
        }
    }
    eprintln!();

    BenchResult {
        label: format!("bare clang ({})", clang_version(clang)),
        cold_ms: all_ms.clone(),
        warm_ms: all_ms,
    }
}

/// Benchmark sccache: shell out to `sccache clang++ -c file.cpp -o file.o`.
fn bench_sccache(
    sccache: &std::path::Path,
    clang: &std::path::Path,
    src_dir: &std::path::Path,
    file_count: usize,
    warm_iterations: usize,
) -> BenchResult {
    let _ = std::process::Command::new(sccache)
        .arg("--zero-stats")
        .output();
    let _ = std::process::Command::new(sccache)
        .arg("--start-server")
        .output();

    let mut cold_ms = Vec::new();
    let mut warm_ms = Vec::new();
    let cwd = src_dir.to_string_lossy().into_owned();

    // Cold pass (cache miss)
    for i in 0..file_count {
        let src = format!("file_{i}.cpp");
        let obj = format!("file_{i}.o");

        let start = Instant::now();
        let output = std::process::Command::new(sccache)
            .arg(clang.to_string_lossy().as_ref())
            .args(["-c", &src, "-o", &obj])
            .current_dir(&cwd)
            .output()
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            output.status.success(),
            "sccache cold failed for {src}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        cold_ms.push(elapsed.as_secs_f64() * 1000.0);

        if (i + 1) % 10 == 0 {
            eprint!("    sccache cold: {}/{file_count} files\r", i + 1);
        }
    }
    eprintln!();

    // Warm passes (cache hit)
    for iter in 0..warm_iterations {
        for i in 0..file_count {
            let src = format!("file_{i}.cpp");
            let obj = format!("file_{i}.o");
            let _ = std::fs::remove_file(src_dir.join(&obj));

            let start = Instant::now();
            let output = std::process::Command::new(sccache)
                .arg(clang.to_string_lossy().as_ref())
                .args(["-c", &src, "-o", &obj])
                .current_dir(&cwd)
                .output()
                .unwrap();
            let elapsed = start.elapsed();

            assert!(
                output.status.success(),
                "sccache warm failed for {src}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            warm_ms.push(elapsed.as_secs_f64() * 1000.0);
        }
        if (iter + 1) % 10 == 0 {
            eprint!(
                "    sccache warm: {}/{warm_iterations} iterations\r",
                iter + 1
            );
        }
    }
    eprintln!();

    BenchResult {
        label: format!("sccache ({})", sccache_version(sccache)),
        cold_ms,
        warm_ms,
    }
}

/// Benchmark zccache: use IPC to daemon.
async fn bench_zccache(
    clang: &std::path::Path,
    src_dir: &std::path::Path,
    file_count: usize,
    warm_iterations: usize,
) -> BenchResult {
    let cwd = src_dir.to_string_lossy().into_owned();
    let log = src_dir.join("zccache_bench.log");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let (sid, compiler) = start_session(&mut client, clang, &cwd, &log.to_string_lossy()).await;

    let mut cold_ms = Vec::new();
    let mut warm_ms = Vec::new();

    // Cold pass (cache miss)
    for i in 0..file_count {
        let src = format!("file_{i}.cpp");
        let obj = format!("file_{i}.o");

        let start = Instant::now();
        let (exit_code, cached) = compile(
            &mut client,
            &sid,
            &compiler,
            &["-c", &src, "-o", &obj],
            &cwd,
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(exit_code, 0, "zccache cold failed for {src}");
        assert!(!cached, "first compile should be a miss");
        cold_ms.push(elapsed.as_secs_f64() * 1000.0);

        if (i + 1) % 10 == 0 {
            eprint!("    zccache cold: {}/{file_count} files\r", i + 1);
        }
    }
    eprintln!();

    // Warm passes (cache hit)
    for iter in 0..warm_iterations {
        for i in 0..file_count {
            let src = format!("file_{i}.cpp");
            let obj = format!("file_{i}.o");
            let _ = std::fs::remove_file(src_dir.join(&obj));

            let start = Instant::now();
            let (exit_code, cached) = compile(
                &mut client,
                &sid,
                &compiler,
                &["-c", &src, "-o", &obj],
                &cwd,
            )
            .await;
            let elapsed = start.elapsed();

            assert_eq!(exit_code, 0, "zccache warm failed for {src}");
            assert!(cached, "recompile should be a hit");
            warm_ms.push(elapsed.as_secs_f64() * 1000.0);
        }
        if (iter + 1) % 10 == 0 {
            eprint!(
                "    zccache warm: {}/{warm_iterations} iterations\r",
                iter + 1
            );
        }
    }
    eprintln!();

    shutdown.notify_one();
    server_handle.await.unwrap();

    BenchResult {
        label: "zccache (in-memory)".to_string(),
        cold_ms,
        warm_ms,
    }
}

// ─── Statistics & reporting ──────────────────────────────────────────────────

fn stats(values: &[f64]) -> (f64, f64, f64, f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() as f64 * 0.95) as usize];
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    (mean, p50, p95, min, max)
}

fn print_stat_line(label: &str, values: &[f64]) {
    let (mean, p50, p95, min, max) = stats(values);
    println!(
        "  {label:<22} mean={mean:>8.2}ms  p50={p50:>8.2}ms  p95={p95:>8.2}ms  min={min:>8.2}ms  max={max:>8.2}ms  (n={})",
        values.len()
    );
}

fn print_three_way(bare: &BenchResult, sccache: &BenchResult, zccache: &BenchResult) {
    let wide = "=".repeat(110);
    let dash = "-".repeat(100);

    println!("\n{wide}");
    println!("  BENCHMARK: bare clang vs sccache vs zccache");
    println!("  {FILE_COUNT} source files, {BARE_ITERATIONS} bare iterations, {WARM_ITERATIONS} cached iterations");
    println!("{wide}\n");

    // Individual results
    println!("  {}", bare.label);
    println!("  {dash}");
    print_stat_line("Compile:", &bare.cold_ms);
    println!();

    println!("  {}", sccache.label);
    println!("  {dash}");
    print_stat_line("Cold (cache miss):", &sccache.cold_ms);
    print_stat_line("Warm (cache hit):", &sccache.warm_ms);
    println!();

    println!("  {}", zccache.label);
    println!("  {dash}");
    print_stat_line("Cold (cache miss):", &zccache.cold_ms);
    print_stat_line("Warm (cache hit):", &zccache.warm_ms);
    println!();

    // Comparison table
    let (_, bare_p50, _, _, _) = stats(&bare.cold_ms);
    let (_, scc_cold_p50, _, _, _) = stats(&sccache.cold_ms);
    let (_, scc_warm_p50, _, _, _) = stats(&sccache.warm_ms);
    let (_, zcc_cold_p50, _, _, _) = stats(&zccache.cold_ms);
    let (_, zcc_warm_p50, _, _, _) = stats(&zccache.warm_ms);

    println!("  COMPARISON (median / p50)");
    println!("  {dash}");
    println!(
        "  {:.<50} {:>8.2}ms (baseline)",
        "bare clang compile", bare_p50
    );
    println!(
        "  {:.<50} {:>8.2}ms ({:.1}x vs bare)",
        "sccache cache miss",
        scc_cold_p50,
        scc_cold_p50 / bare_p50
    );
    println!(
        "  {:.<50} {:>8.2}ms ({:.1}x vs bare)",
        "sccache cache hit",
        scc_warm_p50,
        scc_warm_p50 / bare_p50
    );
    println!(
        "  {:.<50} {:>8.2}ms ({:.1}x vs bare)",
        "zccache cache miss",
        zcc_cold_p50,
        zcc_cold_p50 / bare_p50
    );
    println!(
        "  {:.<50} {:>8.2}ms ({:.1}x vs bare)",
        "zccache cache hit",
        zcc_warm_p50,
        zcc_warm_p50 / bare_p50
    );
    println!();

    // Head-to-head
    let scc_vs_zcc_hit = scc_warm_p50 / zcc_warm_p50;
    let bare_vs_zcc_hit = bare_p50 / zcc_warm_p50;
    println!("  HEAD-TO-HEAD");
    println!("  {dash}");
    println!(
        "  zccache cache hit vs sccache cache hit:  {scc_vs_zcc_hit:>6.1}x faster  ({zcc_warm_p50:.2}ms vs {scc_warm_p50:.2}ms)"
    );
    println!(
        "  zccache cache hit vs bare clang:          {bare_vs_zcc_hit:>6.1}x faster  ({zcc_warm_p50:.2}ms vs {bare_p50:.2}ms)"
    );
    println!();

    // ASCII bar chart
    let max_bar = 60.0;
    let scale = max_bar / bare_p50; // bare clang = full bar

    println!(
        "  LATENCY BAR CHART (p50, each = = {:.1}ms)",
        bare_p50 / max_bar
    );
    println!("  {dash}");

    let bars = [
        ("bare clang", bare_p50),
        ("sccache miss", scc_cold_p50),
        ("sccache hit", scc_warm_p50),
        ("zccache miss", zcc_cold_p50),
        ("zccache hit", zcc_warm_p50),
    ];

    for (name, val) in &bars {
        let bar_len = (val * scale).round().max(1.0) as usize;
        let bar: String = "=".repeat(bar_len);
        println!("  {name:<14} |{bar} {val:.2}ms");
    }

    println!();
    println!("{wide}");
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Full three-way benchmark: bare clang vs sccache vs zccache.
///
/// Run with: soldr cargo test -p zccache-daemon --test perf_test -- perf_full_benchmark --nocapture --ignored
#[tokio::test]
#[ignore]
async fn perf_full_benchmark() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            println!("SKIP: clang not found at ~/.clang-tool-chain");
            return;
        }
    };
    let sccache = match find_sccache() {
        Some(p) => p,
        None => {
            println!("SKIP: sccache not found");
            return;
        }
    };

    let bare_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();

    generate_test_files(bare_dir.path(), FILE_COUNT);
    generate_test_files(sccache_dir.path(), FILE_COUNT);
    generate_test_files(zccache_dir.path(), FILE_COUNT);

    println!();
    println!("  Config: {FILE_COUNT} files, {BARE_ITERATIONS} bare iters, {WARM_ITERATIONS} cached iters");
    println!("  clang:   {}", clang_version(&clang));
    println!("  sccache: {}", sccache_version(&sccache));
    println!();

    println!("  [1/3] Running bare clang benchmark...");
    let bare_result = bench_bare_clang(&clang, bare_dir.path(), FILE_COUNT, BARE_ITERATIONS);
    println!("  [1/3] bare clang done.");

    println!("  [2/3] Running sccache benchmark...");
    let sccache_result = bench_sccache(
        &sccache,
        &clang,
        sccache_dir.path(),
        FILE_COUNT,
        WARM_ITERATIONS,
    );
    println!("  [2/3] sccache done.");

    println!("  [3/3] Running zccache benchmark...");
    let zccache_result =
        bench_zccache(&clang, zccache_dir.path(), FILE_COUNT, WARM_ITERATIONS).await;
    println!("  [3/3] zccache done.");

    print_three_way(&bare_result, &sccache_result, &zccache_result);
}

/// Quick sanity check that both cached tools produce cache hits.
#[tokio::test]
#[ignore]
async fn perf_sanity_check() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            println!("SKIP: clang not found");
            return;
        }
    };
    let sccache = match find_sccache() {
        Some(p) => p,
        None => {
            println!("SKIP: sccache not found");
            return;
        }
    };

    let tmp = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    generate_test_files(tmp.path(), 1);
    let cwd = tmp.path().to_string_lossy().into_owned();

    // sccache: cold then warm
    let _ = std::process::Command::new(&sccache)
        .arg("--start-server")
        .output();
    let _ = std::process::Command::new(&sccache)
        .arg("--zero-stats")
        .output();

    let out = std::process::Command::new(&sccache)
        .arg(clang.to_string_lossy().as_ref())
        .args(["-c", "file_0.cpp", "-o", "file_0.o"])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "sccache cold failed");

    std::fs::remove_file(tmp.path().join("file_0.o")).unwrap();
    let out = std::process::Command::new(&sccache)
        .arg(clang.to_string_lossy().as_ref())
        .args(["-c", "file_0.cpp", "-o", "file_0.o"])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "sccache warm failed");

    let stats_out = std::process::Command::new(&sccache)
        .arg("--show-stats")
        .output()
        .unwrap();
    let stats_text = String::from_utf8_lossy(&stats_out.stdout);
    println!("sccache stats:\n{stats_text}");

    // zccache: cold then warm
    let zcc_tmp = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    generate_test_files(zcc_tmp.path(), 1);
    let zcc_cwd = zcc_tmp.path().to_string_lossy().into_owned();
    let log = zcc_tmp.path().join("log.txt");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let (sid, compiler) =
        start_session(&mut client, &clang, &zcc_cwd, &log.to_string_lossy()).await;

    let (exit_code, cached) = compile(
        &mut client,
        &sid,
        &compiler,
        &["-c", "file_0.cpp", "-o", "file_0.o"],
        &zcc_cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "first compile should miss");

    std::fs::remove_file(zcc_tmp.path().join("file_0.o")).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        &sid,
        &compiler,
        &["-c", "file_0.cpp", "-o", "file_0.o"],
        &zcc_cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "second compile should hit");

    let log_text = std::fs::read_to_string(&log).unwrap();
    println!("zccache log:\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
    println!("\nSanity check passed: both tools produce cache hits.");
}
