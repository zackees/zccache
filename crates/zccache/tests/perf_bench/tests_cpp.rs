//! C++ warm-cache benchmark: zccache vs sccache vs bare clang++.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::Path;
use zccache::protocol::{Request, Response};

use super::common::{
    dir_size_bytes, find_sccache, fmt_bytes, fmt_dur, fmt_ratio, median, print_trials,
    start_daemon, NUM_FILES, WARM_TRIALS,
};
use super::cpp_project::{
    baseline_multi, baseline_single, generate_project, nuke_and_regenerate, sccache_compile_multi,
    sccache_compile_single, source_names, warmup_compiler, zccache_compile_multi,
    zccache_compile_single,
};

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored
async fn perf_warm_cache_zccache_vs_sccache() {
    let compiler_path = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!("  WARM-CACHE BENCHMARK");
    eprintln!("  {NUM_FILES} C++ files \u{00b7} {WARM_TRIALS} warm trials \u{00b7} each tool in its own tempdir");
    eprintln!("  Compiler: {compiler}");
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!();

    // ── Baseline (fresh dir) ──────────────────────────────────────────
    let bl_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_project(bl_dir.path());

    eprintln!("  [1/3] Bare clang (baseline)");

    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        single cold:  {}", fmt_dur(bl_cold_single));

    let bl_warm_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        single warm:  {}", fmt_dur(bl_warm_single));

    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!("        multi cold:   {}", fmt_dur(bl_cold_multi));

    let bl_warm_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!("        multi warm:   {}", fmt_dur(bl_warm_multi));
    eprintln!();

    drop(bl_dir);

    // ── sccache (fresh dir) ───────────────────────────────────────────
    let sccache_cold_single;
    let sccache_cold_multi;
    let sccache_single_times;
    let sccache_multi_times;
    let mut sccache_cache_bytes = None;

    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache::test_support::temp_cache_dir().unwrap();
        generate_project(sc_dir.path());

        // Use a fresh cache dir so previous sccache usage doesn't pollute results.
        let sc_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();

        // Set SCCACHE_DIR for this process so both server and client see it.
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache ({})", sccache_bin.display());

        // Helper: stop server, purge disk cache, restart with fresh SCCACHE_DIR.
        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            // Purge disk cache
            let cache_path = std::path::Path::new(cache_dir);
            if cache_path.exists() {
                let _ = std::fs::remove_dir_all(cache_path);
                let _ = std::fs::create_dir_all(cache_path);
            }
            let _ = std::process::Command::new(sccache)
                .arg("--start-server")
                .env("SCCACHE_DIR", cache_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };

        // Start fresh server with isolated cache dir
        stop_purge_start(&sccache_bin, &sc_cache_str);

        // Cold single-file: nuke dir, regenerate, warmup clang, compile (cache empty)
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        single cold:  ");
        let cold_s = sccache_compile_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("{}", fmt_dur(cold_s));
        sccache_cold_single = Some(cold_s);

        // Warm trials: single-file (cache populated from cold pass)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("single warm:", &times);
        sccache_single_times = Some(times);

        stop_purge_start(&sccache_bin, &sc_cache_str);

        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        multi cold:   ");
        let cold_m = sccache_compile_multi(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("{}", fmt_dur(cold_m));
        sccache_cold_multi = Some(cold_m);

        // Warm trials: multi-file (sccache can't cache this — passes through)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_multi(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("multi warm:", &times);
        sccache_multi_times = Some(times);
        sccache_cache_bytes = Some(dir_size_bytes(sc_cache_dir.path()));

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        std::env::remove_var("SCCACHE_DIR");
        drop(sc_dir);
        drop(sc_cache_dir);
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        sccache_cold_single = None;
        sccache_cold_multi = None;
        sccache_single_times = None;
        sccache_multi_times = None;
    }

    // ── zccache (fresh dir, in-process daemon) ────────────────────────
    let zc_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache (in-process daemon)");

    let (zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Start session
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

    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    eprint!("        single cold:  ");
    let zc_cold_single =
        zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("{}", fmt_dur(zc_cold_single));

    let mut zc_single_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_single_times.push(
            zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("single warm:", &zc_single_times);

    client.send(&Request::Clear).await.unwrap();
    let _ = client.recv::<Response>().await;
    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());

    eprint!("        multi cold:   ");
    let zc_cold_multi =
        zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("{}", fmt_dur(zc_cold_multi));

    let mut zc_multi_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_multi_times.push(
            zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("multi warm:", &zc_multi_times);
    let zccache_cache_bytes = dir_size_bytes(zccache_cache_dir.path());

    // End session
    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Summary ─────────────────────────────────────────────────────
    let zc_single_med = median(&zc_single_times);
    let zc_multi_med = median(&zc_multi_times);

    let scc_single_str = sccache_single_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_multi_str = sccache_multi_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_cold_s_str = sccache_cold_single.map(fmt_dur);
    let scc_cold_m_str = sccache_cold_multi.map(fmt_dur);
    let sccache_cache_str = sccache_cache_bytes.map(fmt_bytes);
    let zccache_cache_str = fmt_bytes(zccache_cache_bytes);
    let dash = "\u{2014}";

    eprintln!();
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!("  RESULTS");
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!();
    eprintln!("## Benchmark: {NUM_FILES} C++ files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare Clang | sccache | zccache | bare cache | sccache cache | zccache cache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|--------------:|-----------:|--------------:|");

    // Single-file, Cold
    let scc_cs = scc_cold_s_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_s = sccache_cold_single.map(|t| fmt_ratio(t, zc_cold_single, false));
    let vs_bare_cold_s = fmt_ratio(bl_cold_single, zc_cold_single, false);
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        scc_cs,
        fmt_dur(zc_cold_single),
        fmt_bytes(0),
        sccache_cache_str.as_deref().unwrap_or(dash),
        zccache_cache_str,
        vs_scc_cold_s.as_deref().unwrap_or(dash),
        vs_bare_cold_s,
    );

    // Single-file, Warm
    let scc_ws = scc_single_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_s = sccache_single_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_single_med, true));
    let vs_bare_warm_s = fmt_ratio(bl_warm_single, zc_single_med, true);
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} | {} | {} | {} |",
        fmt_dur(bl_warm_single),
        scc_ws,
        fmt_dur(zc_single_med),
        fmt_bytes(0),
        sccache_cache_str.as_deref().unwrap_or(dash),
        zccache_cache_str,
        vs_scc_warm_s.as_deref().unwrap_or(dash),
        vs_bare_warm_s,
    );

    // Multi-file, Cold
    let scc_cm = scc_cold_m_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_m = sccache_cold_multi.map(|t| fmt_ratio(t, zc_cold_multi, false));
    let vs_bare_cold_m = fmt_ratio(bl_cold_multi, zc_cold_multi, false);
    eprintln!(
        "| Multi-file, Cold | {} | {} | {} | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        scc_cm,
        fmt_dur(zc_cold_multi),
        fmt_bytes(0),
        sccache_cache_str.as_deref().unwrap_or(dash),
        zccache_cache_str,
        vs_scc_cold_m.as_deref().unwrap_or(dash),
        vs_bare_cold_m,
    );

    // Multi-file, Warm
    let scc_wm = scc_multi_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_m = sccache_multi_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_multi_med, true));
    let vs_bare_warm_m = fmt_ratio(bl_warm_multi, zc_multi_med, true);
    eprintln!(
        "| Multi-file, Warm | {} | {} | **{}** | {} | {} | {} | {} | {} |",
        fmt_dur(bl_warm_multi),
        scc_wm,
        fmt_dur(zc_multi_med),
        fmt_bytes(0),
        sccache_cache_str.as_deref().unwrap_or(dash),
        zccache_cache_str,
        vs_scc_warm_m.as_deref().unwrap_or(dash),
        vs_bare_warm_m,
    );

    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!("> Single-file = {NUM_FILES} sequential `clang++ -c unit.cpp` invocations. Multi-file = one `clang++ -c *.cpp` invocation.");
    if sccache_multi_times.is_some() {
        eprintln!("> sccache cannot cache multi-file compilations \u{2014} its \"warm\" multi-file time is a full recompile.");
    }

    // ── Bottom Line ─────────────────────────────────────────────────
    eprintln!();
    eprintln!("### Bottom Line");
    eprintln!();
    let single_vs_clang = bl_warm_single.as_secs_f64() / zc_single_med.as_secs_f64();
    let multi_vs_clang = bl_warm_multi.as_secs_f64() / zc_multi_med.as_secs_f64();
    if let Some(ref t) = sccache_single_times {
        let single_vs_scc = median(t).as_secs_f64() / zc_single_med.as_secs_f64();
        eprintln!(
            "  Warm single-file:  {single_vs_clang:.0}x faster than clang, {single_vs_scc:.0}x faster than sccache"
        );
    } else {
        eprintln!("  Warm single-file:  {single_vs_clang:.0}x faster than clang");
    }
    if let Some(ref t) = sccache_multi_times {
        let multi_vs_scc = median(t).as_secs_f64() / zc_multi_med.as_secs_f64();
        eprintln!(
            "  Warm multi-file:   {multi_vs_clang:.0}x faster than clang, {multi_vs_scc:.0}x faster than sccache"
        );
    } else {
        eprintln!("  Warm multi-file:   {multi_vs_clang:.0}x faster than clang");
    }
    eprintln!();
}
