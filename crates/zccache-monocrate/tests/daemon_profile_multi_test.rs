//! Profile the multi-file warm cache path to identify bottlenecks.
//!
//! Run: soldr cargo test -p zccache-daemon --test profile_multi_test -- --nocapture --ignored

use std::sync::Arc;
use std::time::Instant;
use zccache_monocrate::daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};

const NUM_FILES: usize = 50;
const WARM_ITERS: usize = 10;

fn generate_project(dir: &std::path::Path) {
    let incdir = dir.join("include");
    std::fs::create_dir_all(&incdir).unwrap();
    std::fs::write(
        incdir.join("common.h"),
        "#pragma once\n#include <vector>\n#include <cstdint>\n\
         namespace bench { template<typename T> inline T clamp(T v, T lo, T hi) \
         { return v < lo ? lo : v > hi ? hi : v; } }\n",
    )
    .unwrap();

    for i in 0..NUM_FILES {
        std::fs::write(
            dir.join(format!("unit_{i:03}.cpp")),
            format!(
                "#include \"common.h\"\n#include <cmath>\n\
                 namespace unit_{i:03} {{ double f(int n) {{ return std::sin(n * 0.{i:03}1); }} }}\n"
            ),
        )
        .unwrap();
    }
}

fn source_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.cpp")).collect()
}

fn clean_objects(dir: &std::path::Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("o") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[tokio::test]
#[ignore]
async fn profile_multi_file_warm_path() {
    let compiler_path = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let sources = source_names();

    generate_project(tmp.path());

    eprintln!("\n=== Multi-File Warm Path Profile ===");
    eprintln!("  {NUM_FILES} files, {WARM_ITERS} warm iterations");
    eprintln!("  Compiler: {compiler}\n");

    // Start daemon — keep server accessible for profile_snapshot()
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();

    // We need to share the server so we can call profile_snapshot() later.
    // Wrap in Arc<Mutex> since run() takes &mut self.
    let server = Arc::new(tokio::sync::Mutex::new(server));
    let server_clone = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        server_clone.lock().await.run(0).await.unwrap();
    });

    // Give server time to start accepting
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();
    let session_id = match client.recv::<Response>().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // Cold pass: single-file to populate cache
    eprintln!("  Populating cache (cold, single-file)...");
    let t0 = Instant::now();
    for src in &sources {
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".into(),
                    src.clone(),
                    "-o".into(),
                    src.replace(".cpp", ".o"),
                    "-Iinclude".into(),
                    "-O2".into(),
                    "-std=c++17".into(),
                ],
                cwd: cwd.clone().into(),
                compiler: compiler.clone().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv::<Response>().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0);
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    eprintln!("  Cold done: {:.3}s\n", t0.elapsed().as_secs_f64());

    // ── Profile: multi-file warm ────────────────────────────────────
    eprintln!("  --- Multi-file warm iterations ---");
    let mut multi_times = Vec::with_capacity(WARM_ITERS);
    let mut multi_args: Vec<String> = vec!["-c".into()];
    multi_args.extend(sources.iter().cloned());
    multi_args.extend(["-Iinclude".into(), "-O2".into(), "-std=c++17".into()]);

    for i in 0..WARM_ITERS {
        clean_objects(tmp.path());
        let t = Instant::now();
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args.clone(),
                cwd: cwd.clone().into(),
                compiler: compiler.clone().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv::<Response>().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(cached, "iter {i} should be cached");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
        let elapsed = t.elapsed();
        multi_times.push(elapsed);
        eprintln!(
            "    iter {}: {:.3}ms",
            i + 1,
            elapsed.as_secs_f64() * 1000.0
        );
    }

    // ── Profile: single-file warm ───────────────────────────────────
    eprintln!("\n  --- Single-file warm iterations ---");
    let mut single_times = Vec::with_capacity(WARM_ITERS);

    for i in 0..WARM_ITERS {
        clean_objects(tmp.path());
        let t = Instant::now();
        for src in &sources {
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: vec![
                        "-c".into(),
                        src.clone(),
                        "-o".into(),
                        src.replace(".cpp", ".o"),
                        "-Iinclude".into(),
                        "-O2".into(),
                        "-std=c++17".into(),
                    ],
                    cwd: cwd.clone().into(),
                    compiler: compiler.clone().into(),
                    env: None,
                    stdin: Vec::new(),
                })
                .await
                .unwrap();
            match client.recv::<Response>().await.unwrap() {
                Some(Response::CompileResult { exit_code, .. }) => {
                    assert_eq!(exit_code, 0);
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }
        }
        let elapsed = t.elapsed();
        single_times.push(elapsed);
        eprintln!(
            "    iter {}: {:.3}ms ({:.3}ms/file)",
            i + 1,
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1000.0 / NUM_FILES as f64
        );
    }

    // End session
    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    // ── Get profiler snapshot ───────────────────────────────────────
    shutdown.notify_one();
    handle.await.unwrap();

    let profile = server.lock().await.profile_snapshot();

    // ── Report ──────────────────────────────────────────────────────
    let multi_med = {
        let mut s: Vec<_> = multi_times
            .iter()
            .map(|t| t.as_secs_f64() * 1000.0)
            .collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    };
    let single_med = {
        let mut s: Vec<_> = single_times
            .iter()
            .map(|t| t.as_secs_f64() * 1000.0)
            .collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    };

    eprintln!("\n======================================================");
    eprintln!("PROFILE RESULTS ({NUM_FILES} files, {WARM_ITERS} warm iters)");
    eprintln!("======================================================");
    eprintln!("\n  Wall clock (median):");
    eprintln!(
        "    Multi-file:  {multi_med:.3}ms total ({:.3}ms/file)",
        multi_med / NUM_FILES as f64
    );
    eprintln!(
        "    Single-file: {single_med:.3}ms total ({:.3}ms/file)",
        single_med / NUM_FILES as f64
    );
    eprintln!("    Multi speedup: {:.1}x", single_med / multi_med);

    eprintln!(
        "\n  Phase profiler (avg per hit, {} total hits):",
        profile.hit_count
    );
    eprintln!(
        "    build_context:    {:>6}us",
        profile.avg_build_context_ns
    );
    eprintln!("    hash_source:      {:>6}us", profile.avg_hash_source_ns);
    eprintln!("    hash_headers:     {:>6}us", profile.avg_hash_headers_ns);
    eprintln!(
        "    depgraph_check:   {:>6}us",
        profile.avg_depgraph_check_ns
    );
    eprintln!(
        "    artifact_lookup:  {:>6}us",
        profile.avg_artifact_lookup_ns
    );
    eprintln!("    write_output:     {:>6}us", profile.avg_write_output_ns);
    eprintln!("    bookkeeping:      {:>6}us", profile.avg_bookkeeping_ns);
    eprintln!("    ─────────────────────────");
    eprintln!("    total_hit:        {:>6}us", profile.avg_total_hit_ns);

    // Breakdown as percentages
    let total = profile.avg_total_hit_ns.max(1) as f64;
    eprintln!("\n  Breakdown (% of total hit time):");
    eprintln!(
        "    build_context:    {:>5.1}%",
        profile.avg_build_context_ns as f64 / total * 100.0
    );
    eprintln!(
        "    hash_source:      {:>5.1}%",
        profile.avg_hash_source_ns as f64 / total * 100.0
    );
    eprintln!(
        "    hash_headers:     {:>5.1}%",
        profile.avg_hash_headers_ns as f64 / total * 100.0
    );
    eprintln!(
        "    depgraph_check:   {:>5.1}%",
        profile.avg_depgraph_check_ns as f64 / total * 100.0
    );
    eprintln!(
        "    artifact_lookup:  {:>5.1}%",
        profile.avg_artifact_lookup_ns as f64 / total * 100.0
    );
    eprintln!(
        "    write_output:     {:>5.1}%",
        profile.avg_write_output_ns as f64 / total * 100.0
    );
    eprintln!(
        "    bookkeeping:      {:>5.1}%",
        profile.avg_bookkeeping_ns as f64 / total * 100.0
    );

    // IPC overhead estimate
    let daemon_per_file_us = profile.avg_total_hit_ns as f64;
    let wall_per_file_single_us = single_med * 1000.0 / NUM_FILES as f64;
    let ipc_overhead_us = wall_per_file_single_us - daemon_per_file_us;
    eprintln!("\n  IPC overhead estimate (single-file mode):");
    eprintln!("    Wall per file:    {wall_per_file_single_us:.0}us");
    eprintln!("    Daemon per file:  {daemon_per_file_us:.0}us");
    eprintln!(
        "    IPC round-trip:   {ipc_overhead_us:.0}us ({:.1}%)",
        ipc_overhead_us / wall_per_file_single_us * 100.0
    );

    // Multi-file overhead estimate
    let daemon_total_us = profile.avg_total_hit_ns as f64 * NUM_FILES as f64;
    let wall_multi_us = multi_med * 1000.0;
    let multi_overhead_us = wall_multi_us - daemon_total_us;
    eprintln!("\n  Multi-file overhead:");
    eprintln!("    Wall total:       {wall_multi_us:.0}us");
    eprintln!("    Sum daemon work:  {daemon_total_us:.0}us (parallel, so less than wall)");
    eprintln!("    Overhead (IPC + spawn + collect): {multi_overhead_us:.0}us");

    // ── Miss (cold) profile ───────────────────────────────────────
    if profile.miss_count > 0 {
        eprintln!(
            "\n  Miss profiler (avg per miss, {} total misses):",
            profile.miss_count
        );
        eprintln!(
            "    compiler_exec:    {:>8}us ({:.1}ms)",
            profile.avg_compiler_exec_ns,
            profile.avg_compiler_exec_ns as f64 / 1000.0
        );
        eprintln!(
            "    include_scan:     {:>8}us ({:.1}ms)",
            profile.avg_include_scan_ns,
            profile.avg_include_scan_ns as f64 / 1000.0
        );
        eprintln!(
            "    hash_all:         {:>8}us ({:.1}ms)",
            profile.avg_hash_all_ns,
            profile.avg_hash_all_ns as f64 / 1000.0
        );
        eprintln!(
            "    artifact_store:   {:>8}us ({:.1}ms)",
            profile.avg_artifact_store_ns,
            profile.avg_artifact_store_ns as f64 / 1000.0
        );
        eprintln!("    ─────────────────────────");
        eprintln!(
            "    total_miss:       {:>8}us ({:.1}ms)",
            profile.avg_total_miss_ns,
            profile.avg_total_miss_ns as f64 / 1000.0
        );

        let miss_total = profile.avg_total_miss_ns.max(1) as f64;
        let accounted = (profile.avg_compiler_exec_ns
            + profile.avg_include_scan_ns
            + profile.avg_hash_all_ns
            + profile.avg_artifact_store_ns) as f64;
        let unaccounted = miss_total - accounted;
        eprintln!("\n  Miss breakdown (% of total miss time):");
        eprintln!(
            "    compiler_exec:    {:>5.1}%",
            profile.avg_compiler_exec_ns as f64 / miss_total * 100.0
        );
        eprintln!(
            "    include_scan:     {:>5.1}%",
            profile.avg_include_scan_ns as f64 / miss_total * 100.0
        );
        eprintln!(
            "    hash_all:         {:>5.1}%",
            profile.avg_hash_all_ns as f64 / miss_total * 100.0
        );
        eprintln!(
            "    artifact_store:   {:>5.1}%",
            profile.avg_artifact_store_ns as f64 / miss_total * 100.0
        );
        eprintln!(
            "    unaccounted:      {:>5.1}%  ({:.0}us — parse/context/read_output/watch/bookkeeping)",
            unaccounted / miss_total * 100.0,
            unaccounted
        );
    }
    eprintln!();
}
