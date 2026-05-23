//! Sibling-workspace `ZCCACHE_PATH_REMAP=auto` benchmarks (C++ and Rust).

use super::common::{
    end_zccache_session, find_sccache, fmt_dur, fmt_ratio, median, print_trials_per, start_daemon,
    start_zccache_session, NUM_FILES, RUSTC_NUM_FILES, RUSTC_WARM_TRIALS, WARM_TRIALS,
};
use super::rust_project::{
    generate_rust_project, run_rustc_batch, run_sccache_rustc_batch,
    run_zccache_rustc_batch_with_env, rust_source_names, rustc_args_for, warmup_rustc,
};
use super::sibling_remap::{
    make_git_workspace, measure_cpp_sibling_remap_mode, path_remap_auto_env,
};

/// C++ sibling-workspace remap benchmark. Warm-only. Compares zccache (with
/// ZCCACHE_PATH_REMAP=auto, primed from sibling workspace A) against bare clang
/// and sccache (also primed from workspace A, then measured in workspace B).
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_cpp_sibling_remap_warm --nocapture --ignored
async fn perf_cpp_sibling_remap_warm() {
    let compiler_path = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C++ SIBLING-WORKSPACE REMAP BENCHMARK (warm-only)");
    eprintln!("  {NUM_FILES} .cpp files | {WARM_TRIALS} warm trials | ZCCACHE_PATH_REMAP=auto");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let no_file = measure_cpp_sibling_remap_mode(
        "Sibling-workspace no __FILE__, Warm",
        &compiler,
        false,
        false,
    )
    .await;
    let with_file = measure_cpp_sibling_remap_mode(
        "Sibling-workspace with __FILE__, Warm",
        &compiler,
        true,
        true,
    )
    .await;
    let results = [no_file, with_file];

    let dash = "\u{2014}";
    eprintln!();
    eprintln!(
        "## C++ Sibling-Workspace Remap Benchmark: {NUM_FILES} .cpp files, {WARM_TRIALS} warm trials"
    );
    eprintln!();
    eprintln!("| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    for result in &results {
        let zc_warm_med = median(&result.zccache_warm);
        let sccache_warm_str = result.sccache_warm.as_ref().map(|t| fmt_dur(median(t)));
        let vs_sccache = result
            .sccache_warm
            .as_ref()
            .map(|t| fmt_ratio(median(t), zc_warm_med, true));
        let vs_bare = fmt_ratio(result.bare_warm, zc_warm_med, true);
        eprintln!(
            "| {} | {} | {} | **{}** | {} | {} |",
            result.scenario,
            fmt_dur(result.bare_warm),
            sccache_warm_str.as_deref().unwrap_or(dash),
            fmt_dur(zc_warm_med),
            vs_sccache.as_deref().unwrap_or(dash),
            vs_bare,
        );
    }
    eprintln!();
    eprintln!(
        "> Sibling-workspace = two adjacent git roots; sccache and zccache are primed from workspace A, then warm trials are measured in workspace B. The `with __FILE__` row compiles absolute source paths so each sibling root is embedded in preprocessed output."
    );
    eprintln!();
}

/// Rust sibling-workspace remap benchmark. Warm-only. Compares zccache (with
/// ZCCACHE_PATH_REMAP=auto, primed from sibling workspace A) against bare rustc
/// and sccache (each warm in workspace B).
#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_rustc_sibling_remap_warm --nocapture --ignored
async fn perf_rustc_sibling_remap_warm() {
    let rustc_path = match zccache_monocrate::test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: rustc not found");
            return;
        }
    };
    let rc = rustc_path.to_string_lossy().to_string();
    let srcs = rust_source_names();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  RUST SIBLING-WORKSPACE REMAP BENCHMARK (warm-only)");
    eprintln!(
        "  {RUSTC_NUM_FILES} .rs files | {RUSTC_WARM_TRIALS} warm trials | ZCCACHE_PATH_REMAP=auto"
    );
    eprintln!("  Compiler: {rc}");
    eprintln!("================================================================");
    eprintln!();

    let parent = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let workspace_a = parent.path().join("workspace-a");
    let workspace_b = parent.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    make_git_workspace(&workspace_a);
    make_git_workspace(&workspace_b);
    generate_rust_project(&workspace_a);
    generate_rust_project(&workspace_b);

    // ── Bare rustc warm in workspace B ─────────────────────────────────
    eprintln!("  [1/3] Bare rustc (workspace B, warm)");
    warmup_rustc(&rc, &workspace_b);
    let _ = run_rustc_batch(&rc, &workspace_b, &srcs, rustc_args_for); // discard cold
    let mut bl_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        bl_warm.push(run_rustc_batch(&rc, &workspace_b, &srcs, rustc_args_for));
    }
    print_trials_per("warm:", &bl_warm, Some(RUSTC_NUM_FILES));
    eprintln!();

    // ── sccache warm in workspace B ────────────────────────────────────
    let sccache_warm = if let Some(scc_bin) = find_sccache() {
        let scd = zccache_monocrate::test_support::temp_cache_dir().unwrap();
        let scd_s = scd.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &scd_s);
        eprintln!("  [2/3] sccache (workspace B, warm)");
        let _ = std::process::Command::new(&scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new(&scc_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &scd_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_rustc(&rc, &workspace_b);
        let _ = run_sccache_rustc_batch(&scc_bin, &rc, &workspace_b, &srcs, rustc_args_for);
        let mut warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            warm.push(run_sccache_rustc_batch(
                &scc_bin,
                &rc,
                &workspace_b,
                &srcs,
                rustc_args_for,
            ));
        }
        print_trials_per("warm:", &warm, Some(RUSTC_NUM_FILES));
        let _ = std::process::Command::new(&scc_bin)
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

    // ── zccache primed from workspace A, warm in workspace B ───────────
    eprintln!("  [3/3] zccache (prime: workspace A, warm: workspace B, remap=auto)");
    let (_zccache_cache_dir, ep, sh, sd) = start_daemon().await;
    let mut cl = zccache_monocrate::ipc::connect(&ep).await.unwrap();
    let workspace_a_str = workspace_a.to_string_lossy().into_owned();
    let workspace_b_str = workspace_b.to_string_lossy().into_owned();

    let session_a = start_zccache_session(&mut cl, &workspace_a_str).await;
    warmup_rustc(&rc, &workspace_a);
    let _ = run_zccache_rustc_batch_with_env(
        &mut cl,
        &session_a,
        &rc,
        &workspace_a_str,
        &srcs,
        rustc_args_for,
        path_remap_auto_env(),
    )
    .await;
    end_zccache_session(&mut cl, session_a).await;

    let session_b = start_zccache_session(&mut cl, &workspace_b_str).await;
    // First compile in B should hit sibling cache entries from A.
    let _ = run_zccache_rustc_batch_with_env(
        &mut cl,
        &session_b,
        &rc,
        &workspace_b_str,
        &srcs,
        rustc_args_for,
        path_remap_auto_env(),
    )
    .await;
    let mut zc_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        zc_warm.push(
            run_zccache_rustc_batch_with_env(
                &mut cl,
                &session_b,
                &rc,
                &workspace_b_str,
                &srcs,
                rustc_args_for,
                path_remap_auto_env(),
            )
            .await,
        );
    }
    print_trials_per("warm:", &zc_warm, Some(RUSTC_NUM_FILES));

    end_zccache_session(&mut cl, session_b).await;
    sd.notify_one();
    sh.await.unwrap();

    // ── Report ─────────────────────────────────────────────────────────
    let dash = "\u{2014}";
    let bl_med = median(&bl_warm);
    let zc_med = median(&zc_warm);
    let sccache_warm_str = sccache_warm.as_ref().map(|t| fmt_dur(median(t)));
    let vs_sccache = sccache_warm
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_med, true));
    let vs_bare = fmt_ratio(bl_med, zc_med, true);

    eprintln!();
    eprintln!(
        "## Rust Sibling-Workspace Remap Benchmark: {RUSTC_NUM_FILES} .rs files, {RUSTC_WARM_TRIALS} warm trials"
    );
    eprintln!();
    eprintln!("| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    eprintln!(
        "| Sibling-workspace, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_med),
        sccache_warm_str.as_deref().unwrap_or(dash),
        fmt_dur(zc_med),
        vs_sccache.as_deref().unwrap_or(dash),
        vs_bare,
    );
    eprintln!();
    eprintln!(
        "> Sibling-workspace = two adjacent git roots; zccache primed from workspace A, warm trials measured in workspace B with `ZCCACHE_PATH_REMAP=auto`. Bare/sccache run their normal same-workspace warm trials in workspace B."
    );
    eprintln!();
}
