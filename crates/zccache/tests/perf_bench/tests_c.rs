//! C compilation perf benchmark + the always-on C11 regression guard.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::protocol::{Request, Response};

use super::c_project::{
    baseline_c_single, c_source_names, generate_c_project, nuke_and_regenerate_c,
    sccache_compile_c_single, warmup_c_compiler, zccache_compile_c_single,
};
use super::common::{
    dir_size_bytes, find_sccache, fmt_bytes, fmt_dur, fmt_ratio, median, print_trials,
    start_daemon, NUM_FILES, WARM_TRIALS,
};

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_c_zccache_vs_bare --nocapture --ignored
async fn perf_c_zccache_vs_bare() {
    zccache::test_support::ensure_clang_tool_chain_on_path();
    let compiler_path = match zccache::test_support::find_on_path("clang") {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = c_source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C COMPILATION BENCHMARK");
    eprintln!("  {NUM_FILES} .c files | {WARM_TRIALS} warm trials | each tool in its own tempdir");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let bl_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_c_project(bl_dir.path());

    eprintln!("  [1/2] Bare clang");
    nuke_and_regenerate_c(bl_dir.path());
    warmup_c_compiler(&compiler, bl_dir.path());
    let bl_cold = baseline_c_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        cold:  {}", fmt_dur(bl_cold));

    let bl_warm = baseline_c_single(&compiler, bl_dir.path(), &sources);
    eprintln!("        warm:  {}", fmt_dur(bl_warm));
    eprintln!();
    drop(bl_dir);

    let sccache_cold;
    let sccache_warm;
    let mut sccache_cold_cache_bytes = None;
    let mut sccache_warm_cache_bytes = None;
    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache::test_support::temp_cache_dir().unwrap();
        generate_c_project(sc_dir.path());

        let sc_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache ({})", sccache_bin.display());
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if sc_cache_dir.path().exists() {
            let _ = std::fs::remove_dir_all(sc_cache_dir.path());
            let _ = std::fs::create_dir_all(sc_cache_dir.path());
        }
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &sc_cache_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        nuke_and_regenerate_c(sc_dir.path());
        warmup_c_compiler(&compiler, sc_dir.path());
        let cold = sccache_compile_c_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("        cold:  {}", fmt_dur(cold));
        sccache_cold = Some(cold);
        sccache_cold_cache_bytes = Some(dir_size_bytes(sc_cache_dir.path()));

        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_c_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("warm:", &times);
        sccache_warm = Some(times);
        sccache_warm_cache_bytes = Some(dir_size_bytes(sc_cache_dir.path()));

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        sccache_cold = None;
        sccache_warm = None;
    }

    let zc_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_c_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache");
    let (zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
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

    nuke_and_regenerate_c(zc_dir.path());
    warmup_c_compiler(&compiler, zc_dir.path());
    let zc_cold =
        zccache_compile_c_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("        cold:  {}", fmt_dur(zc_cold));
    let zc_cold_cache_bytes = dir_size_bytes(zccache_cache_dir.path());

    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(
            zccache_compile_c_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("warm:", &zc_warm);
    let zc_warm_cache_bytes = dir_size_bytes(zccache_cache_dir.path());

    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    let zc_warm_med = median(&zc_warm);
    let vs_bare_cold = fmt_ratio(bl_cold, zc_cold, false);
    let vs_bare_warm = fmt_ratio(bl_warm, zc_warm_med, true);
    let dash = "\u{2014}";
    let sccache_cold_str = sccache_cold.map(fmt_dur);
    let sccache_warm_str = sccache_warm.as_ref().map(|times| fmt_dur(median(times)));
    let sccache_cold_cache_str = sccache_cold_cache_bytes.map(fmt_bytes);
    let sccache_warm_cache_str = sccache_warm_cache_bytes.map(fmt_bytes);
    let vs_sccache_cold = sccache_cold.map(|duration| fmt_ratio(duration, zc_cold, false));
    let vs_sccache_warm = sccache_warm
        .as_ref()
        .map(|times| fmt_ratio(median(times), zc_warm_med, true));

    eprintln!();
    eprintln!("## C Benchmark: {NUM_FILES} .c files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare clang | sccache | zccache | bare cache | sccache cache | zccache cache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|--------------:|-----------:|--------------:|");
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold),
        sccache_cold_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_cold),
        fmt_bytes(0),
        sccache_cold_cache_str.as_deref().unwrap_or(dash),
        fmt_bytes(zc_cold_cache_bytes),
        vs_sccache_cold.as_deref().unwrap_or(dash),
        vs_bare_cold,
    );
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} | {} | {} | {} |",
        fmt_dur(bl_warm),
        sccache_warm_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_warm_med),
        fmt_bytes(0),
        sccache_warm_cache_str.as_deref().unwrap_or(dash),
        fmt_bytes(zc_warm_cache_bytes),
        vs_sccache_warm.as_deref().unwrap_or(dash),
        vs_bare_warm,
    );
    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!();
}

/// Regression test for the C benchmark's generated source compiling cleanly
/// under `-std=c11`.
///
/// Background: commit 359811c added `<time.h>` plus a `bench_now()` helper to
/// the C benchmark's `common_c.h`. The helper called `clock_gettime`, which is
/// a POSIX (not C11) symbol — under strict `-std=c11`, glibc's `<time.h>` does
/// not declare it, so every C benchmark broke with "C warmup compile failed"
/// the first time perf-guard ran on main after the change.
///
/// This test rebuilds the same one-file warmup the benchmark uses and asserts
/// it compiles. It runs only when clang is on PATH (e.g., the perf-guard CI
/// jobs and any dev environment with clang installed); without clang it logs
/// a skip and returns. That guarantees CI lanes that don't have clang aren't
/// forced to install it just to run this guard.
#[test]
fn generated_c_project_compiles_under_std_c11() {
    zccache::test_support::ensure_clang_tool_chain_on_path();
    let Some(compiler_path) = zccache::test_support::find_on_path("clang") else {
        eprintln!("SKIP: no `clang` on PATH; skipping strict-C11 compile guard");
        return;
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    let dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_c_project(dir.path());

    // Exact compile flags used by `warmup_c_compiler`. If this fails, the
    // benchmark's warmup will also fail — and unlike the benchmark, this
    // test runs without the `#[ignore]` gate so it catches the regression
    // on every CI lane that has clang.
    warmup_c_compiler(&compiler, dir.path());
}
