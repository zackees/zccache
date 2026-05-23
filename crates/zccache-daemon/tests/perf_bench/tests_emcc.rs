//! Emscripten (em++) warm-cache and sibling-workspace remap benchmarks.
//!
//! emcc/em++ are detected by zccache as Clang-family, so the C++ compile flow
//! applies as-is. These benchmarks exercise the Emscripten suite end-to-end
//! (single-file + multi-file warm-cache compile) and verify path-remap auto
//! works across sibling git worktrees.

use std::path::Path;

use super::common::{
    end_zccache_session, find_empp, find_sccache, fmt_dur, fmt_ratio, median, print_trials,
    print_trials_per, start_daemon, start_zccache_session, NUM_FILES, WARM_TRIALS,
};
use super::cpp_project::{
    baseline_multi, baseline_single, generate_project, nuke_and_regenerate, sccache_compile_multi,
    sccache_compile_single, source_names, warmup_compiler, zccache_compile_cpp_single_with_env,
    zccache_compile_multi, zccache_compile_single,
};
use super::sibling_remap::{make_git_workspace, path_remap_auto_env};

/// Emscripten warm-cache benchmark: bare em++ vs sccache vs zccache.
/// Mirrors `perf_warm_cache_zccache_vs_sccache` (C++) but uses em++.
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_emcc_warm_cache_zccache_vs_sccache --nocapture --ignored
async fn perf_emcc_warm_cache_zccache_vs_sccache() {
    let compiler_path = match find_empp() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: em++ not found (install emsdk and source emsdk_env)");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  EMSCRIPTEN COMPILATION BENCHMARK");
    eprintln!("  {NUM_FILES} .cpp files | {WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    // ── Bare em++ ────────────────────────────────────────────────────
    let bl_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(bl_dir.path());

    eprintln!("  [1/3] Bare em++ (baseline)");
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

    // ── sccache em++ ──────────────────────────────────────────────────
    let sccache_cold_single;
    let sccache_warm_single;
    let sccache_cold_multi;
    let sccache_warm_multi;
    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache_test_support::temp_cache_dir().unwrap();
        generate_project(sc_dir.path());

        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache em++ ({})", sccache_bin.display());

        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if Path::new(cache_dir).exists() {
                let _ = std::fs::remove_dir_all(cache_dir);
                let _ = std::fs::create_dir_all(cache_dir);
            }
            let _ = std::process::Command::new(sccache)
                .arg("--start-server")
                .env("SCCACHE_DIR", cache_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };

        stop_purge_start(&sccache_bin, &sc_cache_str);
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        let cold = sccache_compile_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("        single cold:  {}", fmt_dur(cold));
        sccache_cold_single = Some(cold);
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            warm.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("single warm:", &warm);
        sccache_warm_single = Some(warm);

        stop_purge_start(&sccache_bin, &sc_cache_str);
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        let cold = sccache_compile_multi(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("        multi cold:   {}", fmt_dur(cold));
        sccache_cold_multi = Some(cold);
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            warm.push(sccache_compile_multi(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("multi warm:", &warm);
        sccache_warm_multi = Some(warm);

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        sccache_cold_single = None;
        sccache_warm_single = None;
        sccache_cold_multi = None;
        sccache_warm_multi = None;
    }

    // ── zccache em++ ──────────────────────────────────────────────────
    let zc_dir = zccache_test_support::temp_cache_dir().unwrap();
    generate_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache em++");
    let (_zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let session_id = start_zccache_session(&mut client, &zc_cwd).await;

    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    let zc_cold_single =
        zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("        single cold:  {}", fmt_dur(zc_cold_single));
    let mut zc_warm_single = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm_single.push(
            zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("single warm:", &zc_warm_single);

    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    let zc_cold_multi =
        zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("        multi cold:   {}", fmt_dur(zc_cold_multi));
    let mut zc_warm_multi = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm_multi.push(
            zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("multi warm:", &zc_warm_multi);

    end_zccache_session(&mut client, session_id).await;
    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Report ────────────────────────────────────────────────────────
    let dash = "\u{2014}";
    let zc_single_med = median(&zc_warm_single);
    let zc_multi_med = median(&zc_warm_multi);
    let sc_warm_single_str = sccache_warm_single.as_ref().map(|t| fmt_dur(median(t)));
    let sc_warm_multi_str = sccache_warm_multi.as_ref().map(|t| fmt_dur(median(t)));
    let sc_cold_single_str = sccache_cold_single.map(fmt_dur);
    let sc_cold_multi_str = sccache_cold_multi.map(fmt_dur);
    let vs_sccache_cold_single = sccache_cold_single.map(|d| fmt_ratio(d, zc_cold_single, false));
    let vs_sccache_cold_multi = sccache_cold_multi.map(|d| fmt_ratio(d, zc_cold_multi, false));
    let vs_sccache_warm_single = sccache_warm_single
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_single_med, true));
    let vs_sccache_warm_multi = sccache_warm_multi
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_multi_med, true));

    eprintln!();
    eprintln!("## Emscripten Benchmark: {NUM_FILES} .cpp files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare em++ | sccache | zccache | vs sccache | vs bare em++ |");
    eprintln!("|:---------|---------:|--------:|--------:|-----------:|-------------:|");
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        sc_cold_single_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_cold_single),
        vs_sccache_cold_single.as_deref().unwrap_or(dash),
        fmt_ratio(bl_cold_single, zc_cold_single, false),
    );
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_single),
        sc_warm_single_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_single_med),
        vs_sccache_warm_single.as_deref().unwrap_or(dash),
        fmt_ratio(bl_warm_single, zc_single_med, true),
    );
    eprintln!(
        "| Multi-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        sc_cold_multi_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_cold_multi),
        vs_sccache_cold_multi.as_deref().unwrap_or(dash),
        fmt_ratio(bl_cold_multi, zc_cold_multi, false),
    );
    eprintln!(
        "| Multi-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_multi),
        sc_warm_multi_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_multi_med),
        vs_sccache_warm_multi.as_deref().unwrap_or(dash),
        fmt_ratio(bl_warm_multi, zc_multi_med, true),
    );
    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!();
}

/// Emscripten sibling-workspace remap benchmark. Warm-only. Verifies
/// `ZCCACHE_PATH_REMAP=auto` injects `-ffile-prefix-map` for em++ (Clang-family)
/// so equivalent compiles share cache across sibling git roots.
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_emcc_sibling_remap_warm --nocapture --ignored
async fn perf_emcc_sibling_remap_warm() {
    let compiler_path = match find_empp() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: em++ not found (install emsdk and source emsdk_env)");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  EMSCRIPTEN SIBLING-WORKSPACE REMAP BENCHMARK (warm-only)");
    eprintln!("  {NUM_FILES} .cpp files | {WARM_TRIALS} warm trials | ZCCACHE_PATH_REMAP=auto");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let parent = zccache_test_support::temp_cache_dir().unwrap();
    let workspace_a = parent.path().join("workspace-a");
    let workspace_b = parent.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    make_git_workspace(&workspace_a);
    make_git_workspace(&workspace_b);
    generate_project(&workspace_a);
    generate_project(&workspace_b);

    // ── Bare em++ warm in workspace B ─────────────────────────────────
    eprintln!("  [1/3] Bare em++ (workspace B, warm)");
    warmup_compiler(&compiler, &workspace_b);
    let _ = baseline_single(&compiler, &workspace_b, &sources);
    let mut bl_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        bl_warm.push(baseline_single(&compiler, &workspace_b, &sources));
    }
    print_trials_per("warm:", &bl_warm, Some(NUM_FILES));
    eprintln!();

    // ── sccache em++ warm in workspace B ──────────────────────────────
    let sccache_warm = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache_test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);
        eprintln!("  [2/3] sccache em++ (workspace B, warm)");
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &sc_cache_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_compiler(&compiler, &workspace_b);
        let _ = sccache_compile_single(&sccache_bin, &compiler, &workspace_b, &sources);
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            warm.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                &workspace_b,
                &sources,
            ));
        }
        print_trials_per("warm:", &warm, Some(NUM_FILES));
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
        Some(warm)
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        None
    };

    // ── zccache primed from workspace A, warm in workspace B ──────────
    eprintln!("  [3/3] zccache em++ (prime: workspace A, warm: workspace B, remap=auto)");
    let (_zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    let workspace_a_str = workspace_a.to_string_lossy().into_owned();
    let workspace_b_str = workspace_b.to_string_lossy().into_owned();
    let session_a = start_zccache_session(&mut client, &workspace_a_str).await;
    warmup_compiler(&compiler, &workspace_a);
    let _ = zccache_compile_cpp_single_with_env(
        &mut client,
        &session_a,
        &compiler,
        &workspace_a_str,
        &sources,
        path_remap_auto_env(),
    )
    .await;
    end_zccache_session(&mut client, session_a).await;

    let session_b = start_zccache_session(&mut client, &workspace_b_str).await;
    let _ = zccache_compile_cpp_single_with_env(
        &mut client,
        &session_b,
        &compiler,
        &workspace_b_str,
        &sources,
        path_remap_auto_env(),
    )
    .await;
    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(
            zccache_compile_cpp_single_with_env(
                &mut client,
                &session_b,
                &compiler,
                &workspace_b_str,
                &sources,
                path_remap_auto_env(),
            )
            .await,
        );
    }
    print_trials_per("warm:", &zc_warm, Some(NUM_FILES));

    end_zccache_session(&mut client, session_b).await;
    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Report ────────────────────────────────────────────────────────
    let dash = "\u{2014}";
    let bl_warm_med = median(&bl_warm);
    let zc_warm_med = median(&zc_warm);
    let sccache_warm_str = sccache_warm.as_ref().map(|t| fmt_dur(median(t)));
    let vs_sccache = sccache_warm
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_warm_med, true));
    let vs_bare = fmt_ratio(bl_warm_med, zc_warm_med, true);

    eprintln!();
    eprintln!(
        "## Emscripten Sibling-Workspace Remap Benchmark: {NUM_FILES} .cpp files, {WARM_TRIALS} warm trials"
    );
    eprintln!();
    eprintln!("| Scenario | Bare em++ | sccache | zccache | vs sccache | vs bare em++ |");
    eprintln!("|:---------|---------:|--------:|--------:|-----------:|-------------:|");
    eprintln!(
        "| Sibling-workspace, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_med),
        sccache_warm_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_warm_med),
        vs_sccache.as_deref().unwrap_or(dash),
        vs_bare,
    );
    eprintln!();
    eprintln!(
        "> Sibling-workspace = two adjacent git roots; zccache primed from workspace A, warm trials measured in workspace B with `ZCCACHE_PATH_REMAP=auto`. Bare/sccache run their normal same-workspace warm trials in workspace B."
    );
    eprintln!();
}
