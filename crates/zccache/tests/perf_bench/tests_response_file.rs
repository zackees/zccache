//! Response-file C++ warm-cache benchmark.

use std::path::Path;
use zccache::protocol::{Request, Response};

use super::common::{
    find_sccache, fmt_dur, fmt_ratio, median, print_trials, start_daemon, NUM_FILES,
    RSP_NUM_DEFINES, RSP_NUM_INCLUDES, WARM_TRIALS,
};
use super::cpp_project::{
    generate_project, nuke_and_regenerate_with_rsp, source_names, warmup_compiler,
};
use super::response_file::{
    baseline_multi_rsp, baseline_single_rsp, generate_response_files, sccache_compile_multi_rsp,
    sccache_compile_single_rsp, zccache_compile_multi_rsp, zccache_compile_single_rsp,
};

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_response_file --nocapture --ignored
async fn perf_response_file() {
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
    eprintln!("  RESPONSE-FILE BENCHMARK");
    eprintln!(
        "  {NUM_FILES} C++ files \u{00b7} {WARM_TRIALS} warm trials \u{00b7} ~{} expanded args per compile",
        RSP_NUM_DEFINES + RSP_NUM_INCLUDES + 30 + 3,
    );
    eprintln!("  Compiler: {compiler}");
    eprintln!("\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    eprintln!();

    // ── Baseline RSP (fresh dir) ─────────────────────────────────────
    let bl_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_project(bl_dir.path());
    generate_response_files(bl_dir.path());

    eprintln!("  [1/3] Bare clang (baseline)");

    nuke_and_regenerate_with_rsp(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_single = baseline_single_rsp(&compiler, bl_dir.path(), &sources);
    eprintln!("        single cold:  {}", fmt_dur(bl_cold_single));

    let bl_warm_single = baseline_single_rsp(&compiler, bl_dir.path(), &sources);
    eprintln!("        single warm:  {}", fmt_dur(bl_warm_single));

    nuke_and_regenerate_with_rsp(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_multi = baseline_multi_rsp(&compiler, bl_dir.path());
    eprintln!("        multi cold:   {}", fmt_dur(bl_cold_multi));

    let bl_warm_multi = baseline_multi_rsp(&compiler, bl_dir.path());
    eprintln!("        multi warm:   {}", fmt_dur(bl_warm_multi));
    eprintln!();
    drop(bl_dir);

    // ── sccache RSP (fresh dir) ──────────────────────────────────────
    let sccache_cold_single;
    let sccache_cold_multi;
    let sccache_single_times;
    let sccache_multi_times;

    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = zccache::test_support::temp_cache_dir().unwrap();
        generate_project(sc_dir.path());
        generate_response_files(sc_dir.path());

        let sc_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("  [2/3] sccache ({})", sccache_bin.display());

        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
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

        stop_purge_start(&sccache_bin, &sc_cache_str);

        nuke_and_regenerate_with_rsp(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        single cold:  ");
        let cold_s = sccache_compile_single_rsp(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!("{}", fmt_dur(cold_s));
        sccache_cold_single = Some(cold_s);

        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_single_rsp(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("single warm:", &times);
        sccache_single_times = Some(times);

        stop_purge_start(&sccache_bin, &sc_cache_str);

        nuke_and_regenerate_with_rsp(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("        multi cold:   ");
        let cold_m = sccache_compile_multi_rsp(&sccache_bin, &compiler, sc_dir.path());
        eprintln!("{}", fmt_dur(cold_m));
        sccache_cold_multi = Some(cold_m);

        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_multi_rsp(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
            ));
        }
        print_trials("multi warm:", &times);
        sccache_multi_times = Some(times);

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

    // ── zccache RSP (fresh dir, in-process daemon) ───────────────────
    let zc_dir = zccache::test_support::temp_cache_dir().unwrap();
    generate_project(zc_dir.path());
    generate_response_files(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("  [3/3] zccache (in-process daemon)");

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
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    nuke_and_regenerate_with_rsp(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    eprint!("        single cold:  ");
    let zc_cold_single =
        zccache_compile_single_rsp(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!("{}", fmt_dur(zc_cold_single));

    let mut zc_single_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_single_times.push(
            zccache_compile_single_rsp(&mut client, &session_id, &compiler, &zc_cwd, &sources)
                .await,
        );
    }
    print_trials("single warm:", &zc_single_times);

    client.send(&Request::Clear).await.unwrap();
    let _ = client.recv::<Response>().await;
    nuke_and_regenerate_with_rsp(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());

    eprint!("        multi cold:   ");
    let zc_cold_multi =
        zccache_compile_multi_rsp(&mut client, &session_id, &compiler, &zc_cwd).await;
    eprintln!("{}", fmt_dur(zc_cold_multi));

    let mut zc_multi_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_multi_times
            .push(zccache_compile_multi_rsp(&mut client, &session_id, &compiler, &zc_cwd).await);
    }
    print_trials("multi warm:", &zc_multi_times);

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
    let dash = "\u{2014}";

    eprintln!();
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!("  RESULTS");
    eprintln!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprintln!();
    eprintln!(
        "## Response-File Benchmark: {NUM_FILES} C++ files, ~{} expanded args, {WARM_TRIALS} warm trials",
        RSP_NUM_DEFINES + RSP_NUM_INCLUDES + 30 + 3,
    );
    eprintln!();
    eprintln!("| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");

    // Single-file RSP, Cold
    let scc_cs = scc_cold_s_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_s = sccache_cold_single.map(|t| fmt_ratio(t, zc_cold_single, false));
    let vs_bare_cold_s = fmt_ratio(bl_cold_single, zc_cold_single, false);
    eprintln!(
        "| Single-file RSP, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        scc_cs,
        fmt_dur(zc_cold_single),
        vs_scc_cold_s.as_deref().unwrap_or(dash),
        vs_bare_cold_s,
    );

    // Single-file RSP, Warm
    let scc_ws = scc_single_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_s = sccache_single_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_single_med, true));
    let vs_bare_warm_s = fmt_ratio(bl_warm_single, zc_single_med, true);
    eprintln!(
        "| Single-file RSP, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_single),
        scc_ws,
        fmt_dur(zc_single_med),
        vs_scc_warm_s.as_deref().unwrap_or(dash),
        vs_bare_warm_s,
    );

    // Multi-file RSP, Cold
    let scc_cm = scc_cold_m_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_m = sccache_cold_multi.map(|t| fmt_ratio(t, zc_cold_multi, false));
    let vs_bare_cold_m = fmt_ratio(bl_cold_multi, zc_cold_multi, false);
    eprintln!(
        "| Multi-file RSP, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        scc_cm,
        fmt_dur(zc_cold_multi),
        vs_scc_cold_m.as_deref().unwrap_or(dash),
        vs_bare_cold_m,
    );

    // Multi-file RSP, Warm
    let scc_wm = scc_multi_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_m = sccache_multi_times
        .as_ref()
        .map(|t| fmt_ratio(median(t), zc_multi_med, true));
    let vs_bare_warm_m = fmt_ratio(bl_warm_multi, zc_multi_med, true);
    eprintln!(
        "| Multi-file RSP, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_multi),
        scc_wm,
        fmt_dur(zc_multi_med),
        vs_scc_warm_m.as_deref().unwrap_or(dash),
        vs_bare_warm_m,
    );

    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!(
        "> All args passed via nested response files: flags.rsp -> @warnings.rsp + @defines.rsp"
    );
    eprintln!("> {RSP_NUM_DEFINES} -D defines + {RSP_NUM_INCLUDES} -I paths + 30 warning flags = ~{} total expanded args per compile.",
        RSP_NUM_DEFINES + RSP_NUM_INCLUDES + 30 + 3);

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
