//! Rust (rustc) warm-cache benchmark: zccache vs sccache vs bare rustc.

use std::time::Duration;
use zccache::protocol::{Request, Response};

use super::common::{
    find_sccache, fmt_dur, fmt_ratio, median, print_trials, start_daemon, RUSTC_NUM_FILES,
    RUSTC_WARM_TRIALS,
};
use super::rust_project::{
    generate_rust_project, run_rustc_batch, run_sccache_rustc_batch, run_zccache_rustc_batch,
    rust_source_names, rustc_args_for, rustc_check_args_for, warmup_rustc,
};

/// Rust compilation: bare rustc vs sccache vs zccache, 50 independent .rs lib files.
/// Tests both `cargo build` (emit link+metadata+dep-info) and `cargo check` (emit metadata+dep-info) modes.
#[tokio::test]
#[ignore]
async fn perf_rustc_zccache_vs_sccache() {
    let rustc_path = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: rustc not found");
            return;
        }
    };
    let rc = rustc_path.to_string_lossy().to_string();
    let srcs = rust_source_names();

    eprintln!();
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!("  RUST COMPILATION BENCHMARK");
    eprintln!("  {RUSTC_NUM_FILES} .rs files \u{00b7} {RUSTC_WARM_TRIALS} warm trials \u{00b7} each tool in its own tempdir");
    eprintln!("  Compiler: {rc}");
    eprintln!("  Modes: build (--emit=dep-info,metadata,link) + check (--emit=dep-info,metadata)");
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!();

    // Helper: run a mode (build or check) through all 3 tools.
    // ─── Build mode (--emit=dep-info,metadata,link) ─────────────────────
    eprintln!("  ─── Build mode (cargo build) ───");
    eprintln!();

    let bl_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_rust_project(bl_dir.path());
    eprintln!("  [1/3] Bare rustc");
    warmup_rustc(&rc, bl_dir.path());
    let build_bl_cold = run_rustc_batch(&rc, bl_dir.path(), &srcs, rustc_args_for);
    eprintln!("        cold:  {}", fmt_dur(build_bl_cold));
    let build_bl_warm = run_rustc_batch(&rc, bl_dir.path(), &srcs, rustc_args_for);
    eprintln!("        warm:  {}", fmt_dur(build_bl_warm));
    eprintln!();
    drop(bl_dir);

    let build_sc_cold;
    let build_sc_warm;
    if let Some(ref scc_bin) = find_sccache() {
        let sd = zccache::test_support::temp_cache_dir().unwrap();
        generate_rust_project(sd.path());
        let scd = zccache::test_support::temp_cache_dir().unwrap();
        let scd_s = scd.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &scd_s);
        eprintln!("  [2/3] sccache");
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if scd.path().exists() {
            let _ = std::fs::remove_dir_all(scd.path());
            let _ = std::fs::create_dir_all(scd.path());
        }
        let _ = std::process::Command::new(scc_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &scd_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_rustc(&rc, sd.path());
        let c = run_sccache_rustc_batch(scc_bin, &rc, sd.path(), &srcs, rustc_args_for);
        eprintln!("        cold:  {}", fmt_dur(c));
        build_sc_cold = Some(c);
        let mut t = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            t.push(run_sccache_rustc_batch(
                scc_bin,
                &rc,
                sd.path(),
                &srcs,
                rustc_args_for,
            ));
        }
        print_trials("warm:", &t);
        build_sc_warm = Some(t);
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        build_sc_cold = None;
        build_sc_warm = None;
    }

    let zd = zccache::test_support::temp_cache_dir().unwrap();
    generate_rust_project(zd.path());
    let zc = zd.path().to_string_lossy().into_owned();
    eprintln!("  [3/3] zccache");
    let (_zccache_cache_dir, ep, sh, sd) = start_daemon().await;
    let mut cl = zccache::ipc::connect(&ep).await.unwrap();
    cl.send(&Request::SessionStart {
        client_pid: std::process::id(),
        working_dir: zc.clone().into(),
        log_file: None,
        track_stats: true,
        journal_path: None,
        profile: false,
    })
    .await
    .unwrap();
    let sid = match cl.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    warmup_rustc(&rc, zd.path());
    let build_zc_cold =
        run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_args_for).await;
    eprintln!("        cold:  {}", fmt_dur(build_zc_cold));
    let mut build_zc_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        build_zc_warm
            .push(run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_args_for).await);
    }
    print_trials("warm:", &build_zc_warm);
    eprintln!();

    // ─── Check mode (--emit=dep-info,metadata) ──────────────────────────
    eprintln!("  ─── Check mode (cargo check) ───");
    eprintln!();

    let bl_dir2 = zccache::test_support::temp_cache_dir().unwrap();
    generate_rust_project(bl_dir2.path());
    eprintln!("  [1/3] Bare rustc");
    warmup_rustc(&rc, bl_dir2.path());
    let check_bl_cold = run_rustc_batch(&rc, bl_dir2.path(), &srcs, rustc_check_args_for);
    eprintln!("        cold:  {}", fmt_dur(check_bl_cold));
    let check_bl_warm = run_rustc_batch(&rc, bl_dir2.path(), &srcs, rustc_check_args_for);
    eprintln!("        warm:  {}", fmt_dur(check_bl_warm));
    eprintln!();
    drop(bl_dir2);

    let check_sc_cold;
    let check_sc_warm;
    if let Some(ref scc_bin) = find_sccache() {
        let sd = zccache::test_support::temp_cache_dir().unwrap();
        generate_rust_project(sd.path());
        let scd = zccache::test_support::temp_cache_dir().unwrap();
        let scd_s = scd.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &scd_s);
        eprintln!("  [2/3] sccache");
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if scd.path().exists() {
            let _ = std::fs::remove_dir_all(scd.path());
            let _ = std::fs::create_dir_all(scd.path());
        }
        let _ = std::process::Command::new(scc_bin)
            .arg("--start-server")
            .env("SCCACHE_DIR", &scd_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        warmup_rustc(&rc, sd.path());
        let c = run_sccache_rustc_batch(scc_bin, &rc, sd.path(), &srcs, rustc_check_args_for);
        eprintln!("        cold:  {}", fmt_dur(c));
        check_sc_cold = Some(c);
        let mut t = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            t.push(run_sccache_rustc_batch(
                scc_bin,
                &rc,
                sd.path(),
                &srcs,
                rustc_check_args_for,
            ));
        }
        print_trials("warm:", &t);
        check_sc_warm = Some(t);
        let _ = std::process::Command::new(scc_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::env::remove_var("SCCACHE_DIR");
        eprintln!();
    } else {
        eprintln!("  [2/3] sccache: not found, skipping\n");
        check_sc_cold = None;
        check_sc_warm = None;
    }

    // Reuse zccache daemon — clear cache for fresh check-mode measurement
    cl.send(&Request::Clear).await.unwrap();
    let _ = cl.recv::<Response>().await;
    generate_rust_project(zd.path());
    eprintln!("  [3/3] zccache");
    warmup_rustc(&rc, zd.path());
    let check_zc_cold =
        run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_check_args_for).await;
    eprintln!("        cold:  {}", fmt_dur(check_zc_cold));
    let mut check_zc_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        check_zc_warm.push(
            run_zccache_rustc_batch(&mut cl, &sid, &rc, &zc, &srcs, rustc_check_args_for).await,
        );
    }
    print_trials("warm:", &check_zc_warm);

    cl.send(&Request::SessionEnd { session_id: sid })
        .await
        .unwrap();
    let _ = cl.recv::<Response>().await;
    sd.notify_one();
    sh.await.unwrap();

    // ── Results table ──────────────────────────────────────────────────
    let dash = "\u{2014}";
    let build_zm = median(&build_zc_warm);
    let check_zm = median(&check_zc_warm);

    eprintln!();
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!("  RESULTS");
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!();
    eprintln!("## Rust Benchmark: {RUSTC_NUM_FILES} .rs files, {RUSTC_WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");

    // Helper closure for table rows
    let row = |label: &str, bl: Duration, sc: Option<Duration>, zc: Duration, bold: bool| {
        let sc_s = sc.map(fmt_dur);
        let sc_str = sc_s.as_deref().unwrap_or(dash);
        let vs_sc = sc.map(|s| fmt_ratio(s, zc, bold));
        let vs_bl = fmt_ratio(bl, zc, bold);
        let zc_fmt = if bold {
            format!("**{}**", fmt_dur(zc))
        } else {
            fmt_dur(zc)
        };
        eprintln!(
            "| {} | {} | {} | {} | {} | {} |",
            label,
            fmt_dur(bl),
            sc_str,
            zc_fmt,
            vs_sc.as_deref().unwrap_or(dash),
            vs_bl
        );
    };

    row(
        "Build, Cold",
        build_bl_cold,
        build_sc_cold,
        build_zc_cold,
        false,
    );
    row(
        "Build, Warm",
        build_bl_warm,
        build_sc_warm.as_ref().map(|t| median(t)),
        build_zm,
        true,
    );
    row(
        "Check, Cold",
        check_bl_cold,
        check_sc_cold,
        check_zc_cold,
        false,
    );
    row(
        "Check, Warm",
        check_bl_warm,
        check_sc_warm.as_ref().map(|t| median(t)),
        check_zm,
        true,
    );

    eprintln!();
    eprintln!("> **Build** = `--emit=dep-info,metadata,link` (cargo build). **Check** = `--emit=dep-info,metadata` (cargo check).");
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {RUSTC_WARM_TRIALS} subsequent runs.");

    eprintln!();
    eprintln!("### Bottom Line");
    eprintln!();
    let bld_vs_rc = build_bl_warm.as_secs_f64() / build_zm.as_secs_f64();
    let chk_vs_rc = check_bl_warm.as_secs_f64() / check_zm.as_secs_f64();
    if let Some(ref t) = build_sc_warm {
        let bld_vs_sc = median(t).as_secs_f64() / build_zm.as_secs_f64();
        eprintln!("  Build warm:  {bld_vs_rc:.1}x faster than bare rustc, {bld_vs_sc:.1}x faster than sccache");
    } else {
        eprintln!("  Build warm:  {bld_vs_rc:.1}x faster than bare rustc");
    }
    if let Some(ref t) = check_sc_warm {
        let chk_vs_sc = median(t).as_secs_f64() / check_zm.as_secs_f64();
        eprintln!("  Check warm:  {chk_vs_rc:.1}x faster than bare rustc, {chk_vs_sc:.1}x faster than sccache");
    } else {
        eprintln!("  Check warm:  {chk_vs_rc:.1}x faster than bare rustc");
    }
    eprintln!();
}
