//! Profiling stress test: drives many compilations and reports phase-level timing breakdown.
//!
//! Run with: soldr cargo test -p zccache-daemon --test profile_test -- --nocapture --ignored

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

// ─── Config ──────────────────────────────────────────────────────────────────

const FILE_COUNT: usize = 20;
const WARM_ITERATIONS: usize = 50;
const HEADER_COUNT: usize = 10;

// ─── Daemon helpers ──────────────────────────────────────────────────────────

async fn start_session(client: &mut ClientConn, _clang: &std::path::Path, cwd: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
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

fn generate_test_files(dir: &std::path::Path, file_count: usize, header_count: usize) {
    // Shared headers
    for h in 0..header_count {
        std::fs::write(
            dir.join(format!("header_{h}.h")),
            format!(
                r#"#pragma once
inline int header_{h}_func(int x) {{ return x + {h}; }}
template<typename T> T header_{h}_generic(T a, T b) {{ return a * b + {h}; }}
"#
            ),
        )
        .unwrap();
    }

    for i in 0..file_count {
        let includes: String = (0..header_count)
            .map(|h| format!("#include \"header_{h}.h\"\n"))
            .collect();
        let calls: String = (0..header_count)
            .map(|h| {
                format!("    sum += header_{h}_func(i);\n    sum += header_{h}_generic(sum, i);\n")
            })
            .collect();

        std::fs::write(
            dir.join(format!("src_{i}.cpp")),
            format!(
                r#"{includes}
int func_{i}(int n) {{
    int sum = 0;
    for (int i = 0; i < n; i++) {{
{calls}    }}
    return sum;
}}
"#
            ),
        )
        .unwrap();
    }
}

// ─── Profile reporting ───────────────────────────────────────────────────────

fn print_profile(profile: &zccache::daemon::ProfileSnapshot) {
    let wide = "=".repeat(80);
    let dash = "-".repeat(70);

    println!("\n{wide}");
    println!("  PHASE PROFILING RESULTS");
    println!("{wide}\n");

    if profile.hit_count > 0 {
        println!("  CACHE HIT PATH ({} samples)", profile.hit_count);
        println!("  {dash}");

        let phases = [
            ("parse_args", profile.avg_parse_args_ns),
            ("build_context + register", profile.avg_build_context_ns),
            ("hash_source (metadata cache)", profile.avg_hash_source_ns),
            ("hash_headers (metadata cache)", profile.avg_hash_headers_ns),
            ("depgraph_check", profile.avg_depgraph_check_ns),
            (
                "artifact_lookup (Mutex<HashMap>)",
                profile.avg_artifact_lookup_ns,
            ),
            ("write_output (fs::write)", profile.avg_write_output_ns),
            ("bookkeeping (stats + log)", profile.avg_bookkeeping_ns),
        ];

        let total = profile.avg_total_hit_ns.max(1);
        let mut accounted = 0u64;
        for (name, us) in &phases {
            let pct = (*us as f64 / total as f64) * 100.0;
            let bar_len = (pct / 2.0).round().max(0.0) as usize;
            let bar: String = "#".repeat(bar_len);
            println!("  {name:<35} {us:>6}us  ({pct:>5.1}%)  {bar}");
            accounted += us;
        }

        let overhead = total.saturating_sub(accounted);
        let overhead_pct = (overhead as f64 / total as f64) * 100.0;
        println!(
            "  {:<35} {:>6}us  ({:>5.1}%)",
            "overhead/unaccounted", overhead, overhead_pct
        );
        println!("  {dash}");
        println!("  {:<35} {:>6}us", "TOTAL (avg per hit)", total);
        println!();
    }

    if profile.miss_count > 0 {
        println!("  CACHE MISS PATH ({} samples)", profile.miss_count);
        println!("  {dash}");

        let miss_phases = [
            ("compiler_exec (clang)", profile.avg_compiler_exec_ns),
            ("include_scan (recursive)", profile.avg_include_scan_ns),
            ("hash_all_files", profile.avg_hash_all_ns),
            (
                "artifact_store (depgraph + HashMap)",
                profile.avg_artifact_store_ns,
            ),
        ];

        let total = profile.avg_total_miss_ns.max(1);
        let mut accounted = 0u64;
        for (name, us) in &miss_phases {
            let pct = (*us as f64 / total as f64) * 100.0;
            let bar_len = (pct / 2.0).round().max(0.0) as usize;
            let bar: String = "#".repeat(bar_len);
            println!("  {name:<35} {us:>6}us  ({pct:>5.1}%)  {bar}");
            accounted += us;
        }

        let overhead = total.saturating_sub(accounted);
        let overhead_pct = (overhead as f64 / total as f64) * 100.0;
        println!(
            "  {:<35} {:>6}us  ({:>5.1}%)",
            "overhead/unaccounted", overhead, overhead_pct
        );
        println!("  {dash}");
        println!("  {:<35} {:>6}us", "TOTAL (avg per miss)", total);
        println!();
    }

    println!("{wide}");
}

// ─── The test ────────────────────────────────────────────────────────────────

/// Profiling stress test: cold + warm compilations with phase-level timing breakdown.
///
/// Run with: soldr cargo test -p zccache-daemon --test profile_test -- --nocapture --ignored
#[tokio::test]
#[ignore]
async fn profile_compile_phases() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            println!("SKIP: clang not found at ~/.clang-tool-chain");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    generate_test_files(tmp.path(), FILE_COUNT, HEADER_COUNT);
    let cwd = tmp.path().to_string_lossy().into_owned();

    println!();
    println!("  Config: {FILE_COUNT} files, {HEADER_COUNT} headers each, {WARM_ITERATIONS} warm iterations");
    println!("  clang:  {}", {
        let out = std::process::Command::new(&clang)
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    });

    // Start daemon — wrap in Arc<Mutex<Option>> so we can read the profiler after shutdown
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let endpoint = zccache::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let server = Arc::new(Mutex::new(Some(server)));
    let server_clone = Arc::clone(&server);
    let server_handle = tokio::spawn(async move {
        let mut srv = server_clone.lock().await.take().unwrap();
        srv.run(0).await.unwrap();
        // Put it back so we can read the profiler
        *server_clone.lock().await = Some(srv);
    });

    // Give server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd).await;
    let compiler = clang.to_string_lossy().into_owned();

    // ── Cold pass (cache miss) ───────────────────────────────────────
    println!("\n  [1/2] Cold pass ({FILE_COUNT} files)...");
    let cold_start = std::time::Instant::now();
    for i in 0..FILE_COUNT {
        let src = format!("src_{i}.cpp");
        let obj = format!("src_{i}.o");
        let (exit_code, cached) = compile(
            &mut client,
            &sid,
            &compiler,
            &["-c", &src, "-o", &obj],
            &cwd,
        )
        .await;
        assert_eq!(exit_code, 0, "cold compile failed for {src}");
        assert!(!cached, "cold compile should be a miss");
    }
    let cold_elapsed = cold_start.elapsed();
    println!(
        "  Cold pass done: {:.1}ms total ({:.1}ms/file)",
        cold_elapsed.as_secs_f64() * 1000.0,
        cold_elapsed.as_secs_f64() * 1000.0 / FILE_COUNT as f64,
    );

    // ── Warm pass (cache hit) ────────────────────────────────────────
    println!("  [2/2] Warm pass ({FILE_COUNT} files x {WARM_ITERATIONS} iterations)...");
    let warm_start = std::time::Instant::now();
    let total_warm = FILE_COUNT * WARM_ITERATIONS;
    for iter in 0..WARM_ITERATIONS {
        for i in 0..FILE_COUNT {
            let src = format!("src_{i}.cpp");
            let obj = format!("src_{i}.o");
            let _ = std::fs::remove_file(tmp.path().join(&obj));
            let (exit_code, cached) = compile(
                &mut client,
                &sid,
                &compiler,
                &["-c", &src, "-o", &obj],
                &cwd,
            )
            .await;
            assert_eq!(exit_code, 0, "warm compile failed for {src}");
            assert!(cached, "warm compile should be a hit");
        }
        if (iter + 1) % 10 == 0 {
            eprint!("    warm: {}/{WARM_ITERATIONS} iterations\r", iter + 1);
        }
    }
    eprintln!();
    let warm_elapsed = warm_start.elapsed();
    println!(
        "  Warm pass done: {:.1}ms total ({:.3}ms/file, {} files)",
        warm_elapsed.as_secs_f64() * 1000.0,
        warm_elapsed.as_secs_f64() * 1000.0 / total_warm as f64,
        total_warm,
    );

    // ── Get profile snapshot before shutdown ──────────────────────────
    // We need to get profile data via a Status request since the server
    // object was moved into the spawned task. Use the session stats instead
    // and read from the stats endpoint.

    // End session to get stats
    client
        .send(&Request::SessionEnd {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    if let Some(Response::SessionEnded { stats: Some(s), .. }) = client.recv().await.unwrap() {
        println!(
            "\n  Session stats: {} compilations, {} hits, {} misses",
            s.compilations, s.hits, s.misses
        );
    }

    // Get daemon status
    client.send(&Request::Status).await.unwrap();
    if let Some(Response::Status(status)) = client.recv().await.unwrap() {
        println!(
            "  Daemon: {} hits, {} misses, {} time saved (ms)",
            status.cache_hits, status.cache_misses, status.time_saved_ms
        );
    }

    // Shutdown and read profiler
    shutdown.notify_one();
    server_handle.await.unwrap();

    // IPC-level timing analysis
    println!("\n  ── IPC-Level Timing ──");
    let avg_warm_us = warm_elapsed.as_micros() as f64 / total_warm as f64;
    let avg_cold_us = cold_elapsed.as_micros() as f64 / FILE_COUNT as f64;
    println!(
        "  Average cold compile (IPC round-trip): {avg_cold_us:.0}us ({:.2}ms)",
        avg_cold_us / 1000.0
    );
    println!(
        "  Average warm compile (IPC round-trip): {avg_warm_us:.0}us ({:.3}ms)",
        avg_warm_us / 1000.0
    );

    // Phase profiling from the same server instance
    println!("\n  ── Phase Profiling ──");
    let guard = server.lock().await;
    if let Some(ref srv) = *guard {
        let profile = srv.profile_snapshot();
        print_profile(&profile);
    }
}
