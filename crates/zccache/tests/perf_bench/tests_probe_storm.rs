//! Meson configure-phase probe-storm benchmark (issue #625).
//!
//! Simulates the workload meson's configure phase imposes on a compiler
//! cache: hundreds of tiny single-TU compiles where each TU lives in its
//! own freshly-named temp directory (meson generates probes like
//! `/tmp/meson-XXXXXX/probe.c`). Per-probe direct-compile is ~50–100 ms
//! cold and ~20–50 ms warm. Per-probe zccache *hit* overhead today is
//! ~15–30 ms (IPC roundtrip + key compute + decompress + write). With
//! hundreds of probes in a configure run, cumulative plumbing cost is
//! 5–15 s — and that is paid every time meson reruns configure (which
//! it does on every build cycle in downstream fastled).
//!
//! This benchmark establishes the baseline so the fast-path work in
//! #625 can be measured against it. Per CLAUDE.md → "Every perf fix
//! lands with a perf unit test", no perf-cluster row is added yet —
//! that comes with the fix.

use std::time::{Duration, Instant};

use zccache::protocol::{Request, Response};

use super::common::{fmt_dur, fmt_ratio, median, print_trials, start_daemon, WARM_TRIALS};

/// Number of unique probe TUs. Matches the rough order-of-magnitude of
/// what meson emits during a configure for a moderately-sized project
/// (fastled's configure runs in the low hundreds). Kept small enough
/// that the cold pass completes in under a few seconds on CI.
const PROBE_COUNT: usize = 50;

/// Generate the probe sources. Each is a unique tiny TU (so cache keys
/// differ across probes) that compiles in ~5–20 ms direct. Mirrors the
/// shape of meson's `compiler.check_header`, `compiler.has_function`,
/// and `compiler.compiles` probes.
fn generate_probes(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::create_dir_all(root).unwrap();
    let mut sources = Vec::with_capacity(PROBE_COUNT);
    for i in 0..PROBE_COUNT {
        // Each probe lives in its own subdir to match meson's
        // `/tmp/meson-XXXXXX/probe.c` layout — caches that key on the
        // full absolute path will see distinct keys per probe.
        let probe_dir = root.join(format!("probe_{i:03}"));
        std::fs::create_dir_all(&probe_dir).unwrap();
        let probe_src = probe_dir.join("probe.c");
        // Unique trivial source — each probe checks a different "feature".
        std::fs::write(
            &probe_src,
            format!(
                "/* probe {i} */\nint probe_{i:03}(int x) {{ return x + {i}; }}\nint main(void) {{ return probe_{i:03}(0); }}\n",
            ),
        )
        .unwrap();
        sources.push(probe_src);
    }
    sources
}

fn baseline_probe_storm(compiler: &str, sources: &[std::path::PathBuf]) -> Duration {
    let start = Instant::now();
    for src in sources {
        let obj = src.with_extension("o");
        let _ = std::fs::remove_file(&obj);
        let output = std::process::Command::new(compiler)
            .args(["-c", "-O0"])
            .arg(src)
            .arg("-o")
            .arg(&obj)
            .output()
            .expect("probe compile failed to spawn");
        assert!(
            output.status.success(),
            "probe compile failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
    start.elapsed()
}

async fn zccache_probe_storm(
    client: &mut super::common::ClientConn,
    session_id: &str,
    compiler: &str,
    sources: &[std::path::PathBuf],
) -> Duration {
    // Clean previous objects so the storm is reproducible — at cold
    // pass time the cache is empty; at warm pass time only hits matter.
    for src in sources {
        let _ = std::fs::remove_file(src.with_extension("o"));
    }

    let start = Instant::now();
    for src in sources {
        let obj = src.with_extension("o");
        let cwd = src.parent().unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id.to_string(),
                compiler: compiler.to_string().into(),
                args: vec![
                    "-c".into(),
                    "-O0".into(),
                    src.to_string_lossy().into_owned(),
                    "-o".into(),
                    obj.to_string_lossy().into_owned(),
                ],
                cwd: cwd.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv::<Response>().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "probe compile via zccache failed");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache --test perf_bench_test -- perf_meson_probe_storm --nocapture --ignored
async fn perf_meson_probe_storm() {
    zccache::test_support::ensure_clang_tool_chain_on_path();
    let compiler_path = match zccache::test_support::find_on_path("clang") {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  MESON PROBE-STORM BENCHMARK (issue #625)");
    eprintln!("  {PROBE_COUNT} tiny probes | each in its own tempdir");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    // --- Bare clang baseline ----------------------------------------
    let bl_dir = zccache::test_support::temp_cache_dir().unwrap();
    let bl_sources = generate_probes(bl_dir.path());

    eprintln!("  [1/2] Bare clang");
    let bl_cold = baseline_probe_storm(&compiler, &bl_sources);
    eprintln!(
        "        cold:  {}  ({} per probe)",
        fmt_dur(bl_cold),
        fmt_dur(bl_cold / PROBE_COUNT as u32),
    );
    let bl_warm = baseline_probe_storm(&compiler, &bl_sources);
    eprintln!(
        "        warm:  {}  ({} per probe)",
        fmt_dur(bl_warm),
        fmt_dur(bl_warm / PROBE_COUNT as u32),
    );
    drop(bl_dir);
    eprintln!();

    // --- zccache ----------------------------------------------------
    let zc_dir = zccache::test_support::temp_cache_dir().unwrap();
    let zc_sources = generate_probes(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [2/2] zccache");
    let (_zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: zc_cwd.clone().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    let zc_cold = zccache_probe_storm(&mut client, &session_id, &compiler, &zc_sources).await;
    eprintln!(
        "        cold:  {}  ({} per probe)",
        fmt_dur(zc_cold),
        fmt_dur(zc_cold / PROBE_COUNT as u32),
    );

    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(zccache_probe_storm(&mut client, &session_id, &compiler, &zc_sources).await);
    }
    print_trials("warm:", &zc_warm);
    let zc_warm_med = median(&zc_warm);
    eprintln!(
        "        warm/probe (median):  {}",
        fmt_dur(zc_warm_med / PROBE_COUNT as u32),
    );

    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    let vs_bare_cold = fmt_ratio(bl_cold, zc_cold, false);
    let vs_bare_warm = fmt_ratio(bl_warm, zc_warm_med, true);

    eprintln!();
    eprintln!("## Probe-storm Benchmark: {PROBE_COUNT} tiny C probes, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare clang | zccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------------:|");
    eprintln!(
        "| Probe-storm, Cold | {} | {} | {} |",
        fmt_dur(bl_cold),
        fmt_dur(zc_cold),
        vs_bare_cold,
    );
    eprintln!(
        "| Probe-storm, Warm | {} | **{}** | {} |",
        fmt_dur(bl_warm),
        fmt_dur(zc_warm_med),
        vs_bare_warm,
    );
    eprintln!();
    eprintln!(
        "> Per-probe warm overhead: bare {} → zccache {} (Δ {}). \n\
         > Goal of #625's fast-path: drive Δ toward zero (or negative) for sub-100 ms probes.",
        fmt_dur(bl_warm / PROBE_COUNT as u32),
        fmt_dur(zc_warm_med / PROBE_COUNT as u32),
        if zc_warm_med > bl_warm {
            fmt_dur((zc_warm_med - bl_warm) / PROBE_COUNT as u32)
        } else {
            format!("-{}", fmt_dur((bl_warm - zc_warm_med) / PROBE_COUNT as u32))
        },
    );
    eprintln!();
}
