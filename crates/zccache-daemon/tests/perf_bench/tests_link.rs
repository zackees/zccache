//! Link/archive benchmarks: bare / sccache / zccache across C archive,
//! C++ driver-link, Emscripten link, and Rust workspace staticlib link.

use std::path::Path;

use super::common::{
    bench_exe_name, clear_zccache, end_zccache_session, find_archiver, find_empp, find_sccache,
    fmt_dur, median, print_trials, start_daemon, start_fresh_sccache, start_zccache_session,
    stop_sccache, try_run_tool, NUM_FILES, RUSTC_NUM_FILES, RUSTC_WARM_TRIALS, WARM_TRIALS,
};
use super::link::{
    archive_link_args, clean_rust_final_output, driver_link_args, measure_ephemeral_link_scenario,
    prepare_cpp_link_inputs, prepare_fake_archive_inputs, prepare_rust_link_inputs,
    print_link_benchmark_table, run_rust_final_link_timed, run_zccache_rust_final_link_timed,
    rust_final_link_args, rust_final_output_name, try_run_sccache_rust_final_link_timed,
    LinkBenchResult,
};

use super::common::clean_link_outputs;

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_c_archive_link --nocapture --ignored
async fn perf_c_archive_link() {
    let archiver = match find_archiver() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: neither ar nor llvm-ar found on PATH");
            return;
        }
    };
    let output = "libzccache_link_bench.a".to_string();
    let outputs = vec![output.clone()];

    let bare_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let objects = prepare_fake_archive_inputs(bare_dir.path());
    prepare_fake_archive_inputs(sccache_dir.path());
    prepare_fake_archive_inputs(zccache_dir.path());
    let args = archive_link_args(&output, &objects);

    if let Err(error) = try_run_tool(&archiver, &args, bare_dir.path(), "probe ar rcsD") {
        eprintln!(
            "SKIP: archiver does not support deterministic archive benchmark\n{}",
            error
        );
        return;
    }
    clean_link_outputs(bare_dir.path(), &outputs);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C STATIC-LIBRARY LINK BENCHMARK");
    eprintln!("  {NUM_FILES} .o inputs | {WARM_TRIALS} warm trials");
    eprintln!("  Archiver: {}", archiver.display());
    eprintln!("================================================================");
    eprintln!();

    let result = measure_ephemeral_link_scenario(
        "Static archive",
        &archiver,
        &args,
        &outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    print_link_benchmark_table(
        &format!(
            "## C Static-Library Link Benchmark: {NUM_FILES} .o inputs, {WARM_TRIALS} warm trials"
        ),
        "Bare ar",
        &[result],
    );
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_cpp_driver_link --nocapture --ignored
async fn perf_cpp_driver_link() {
    let compiler_path = match zccache_monocrate::test_support::find_clang() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let output = bench_exe_name("cpp_link_app");
    let outputs = vec![output.clone()];

    let bare_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let objects = match prepare_cpp_link_inputs(&compiler, bare_dir.path()) {
        Ok(objects) => objects,
        Err(error) => {
            eprintln!("SKIP: failed to prepare C++ link inputs\n{error}");
            return;
        }
    };
    if let Err(error) = prepare_cpp_link_inputs(&compiler, sccache_dir.path()) {
        eprintln!("SKIP: failed to prepare C++ sccache link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_cpp_link_inputs(&compiler, zccache_dir.path()) {
        eprintln!("SKIP: failed to prepare C++ zccache link inputs\n{error}");
        return;
    }
    let args = driver_link_args(&output, &objects);
    if let Err(error) = try_run_tool(
        Path::new(&compiler),
        &args,
        bare_dir.path(),
        "probe C++ link",
    ) {
        eprintln!("SKIP: C++ compiler-driver link is not available\n{error}");
        return;
    }
    clean_link_outputs(bare_dir.path(), &outputs);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  C++ DRIVER-LINK BENCHMARK");
    eprintln!("  {NUM_FILES} .cpp objects + main.o | {WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let result = measure_ephemeral_link_scenario(
        "Driver link",
        Path::new(&compiler),
        &args,
        &outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    print_link_benchmark_table(
        &format!(
            "## C++ Driver-Link Benchmark: {NUM_FILES} .cpp objects, {WARM_TRIALS} warm trials"
        ),
        "Bare clang++",
        &[result],
    );
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_emcc_link --nocapture --ignored
async fn perf_emcc_link() {
    let compiler_path = match find_empp() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: em++ not found (install emsdk and source emsdk_env)");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    let bare_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let objects = match prepare_cpp_link_inputs(&compiler, bare_dir.path()) {
        Ok(objects) => objects,
        Err(error) => {
            eprintln!("SKIP: failed to prepare Emscripten link inputs\n{error}");
            return;
        }
    };
    if let Err(error) = prepare_cpp_link_inputs(&compiler, sccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Emscripten sccache link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_cpp_link_inputs(&compiler, zccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Emscripten zccache link inputs\n{error}");
        return;
    }

    let html_output = "em_link_app.html".to_string();
    let wasm_output = "em_link_app.wasm".to_string();
    let html_outputs = vec![html_output.clone()];
    let wasm_outputs = vec![wasm_output.clone()];
    let html_args = driver_link_args(&html_output, &objects);
    let wasm_args = driver_link_args(&wasm_output, &objects);
    if let Err(error) = try_run_tool(
        Path::new(&compiler),
        &html_args,
        bare_dir.path(),
        "probe em++ html link",
    ) {
        eprintln!("SKIP: Emscripten HTML link is not available\n{error}");
        return;
    }
    clean_link_outputs(bare_dir.path(), &html_outputs);
    if let Err(error) = try_run_tool(
        Path::new(&compiler),
        &wasm_args,
        bare_dir.path(),
        "probe em++ wasm link",
    ) {
        eprintln!("SKIP: Emscripten Wasm link is not available\n{error}");
        return;
    }
    clean_link_outputs(bare_dir.path(), &wasm_outputs);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  EMSCRIPTEN LINK BENCHMARK");
    eprintln!("  {NUM_FILES} .cpp objects + main.o | {WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {compiler}");
    eprintln!("================================================================");
    eprintln!();

    let html = measure_ephemeral_link_scenario(
        "HTML link",
        Path::new(&compiler),
        &html_args,
        &html_outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    let wasm = measure_ephemeral_link_scenario(
        "Wasm link",
        Path::new(&compiler),
        &wasm_args,
        &wasm_outputs,
        bare_dir.path(),
        sccache_dir.path(),
        zccache_dir.path(),
    )
    .await;
    print_link_benchmark_table(
        &format!(
            "## Emscripten Link Benchmark: {NUM_FILES} .cpp objects, {WARM_TRIALS} warm trials"
        ),
        "Bare em++",
        &[html, wasm],
    );
}

#[tokio::test]
#[ignore] // Run explicitly: soldr cargo test -p zccache-daemon --test perf_bench_test -- perf_rust_workspace_link --nocapture --ignored
async fn perf_rust_workspace_link() {
    let rustc_path = match zccache_monocrate::test_support::find_rustc() {
        Some(path) => path,
        None => {
            eprintln!("SKIP: rustc not found");
            return;
        }
    };
    let rustc = rustc_path.to_string_lossy().to_string();
    let output = rust_final_output_name();
    let args = rust_final_link_args(&output);

    let bare_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let sccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    let zccache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
    if let Err(error) = prepare_rust_link_inputs(&rustc, bare_dir.path()) {
        eprintln!("SKIP: failed to prepare Rust link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_rust_link_inputs(&rustc, sccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Rust sccache link inputs\n{error}");
        return;
    }
    if let Err(error) = prepare_rust_link_inputs(&rustc, zccache_dir.path()) {
        eprintln!("SKIP: failed to prepare Rust zccache link inputs\n{error}");
        return;
    }
    if let Err(error) = try_run_tool(
        Path::new(&rustc),
        &args,
        bare_dir.path(),
        "probe Rust staticlib link",
    ) {
        eprintln!("SKIP: Rust staticlib link is not available\n{error}");
        return;
    }
    clean_rust_final_output(bare_dir.path(), &output);

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  RUST WORKSPACE LINK BENCHMARK");
    eprintln!("  {RUSTC_NUM_FILES} .rlib inputs | {RUSTC_WARM_TRIALS} warm trials");
    eprintln!("  Compiler: {rustc}");
    eprintln!("================================================================");
    eprintln!();

    eprintln!("  [1/3] Bare rustc");
    let _ = run_rust_final_link_timed(
        Path::new(&rustc),
        &args,
        bare_dir.path(),
        &output,
        "bare Rust link warmup",
    );
    let bare_cold = run_rust_final_link_timed(
        Path::new(&rustc),
        &args,
        bare_dir.path(),
        &output,
        "bare Rust cold link",
    );
    eprintln!("        cold: {}", fmt_dur(bare_cold));
    let mut bare_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        bare_warm.push(run_rust_final_link_timed(
            Path::new(&rustc),
            &args,
            bare_dir.path(),
            &output,
            "bare Rust warm link",
        ));
    }
    print_trials("warm:", &bare_warm);
    eprintln!();

    let (sccache_cold, sccache_warm) = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache_monocrate::test_support::temp_cache_dir().unwrap();
        let _cache_dir = start_fresh_sccache(&sccache_bin, sc_cache_dir.path());
        eprintln!("  [2/3] sccache ({})", sccache_bin.display());
        let cold = match try_run_sccache_rust_final_link_timed(
            &sccache_bin,
            Path::new(&rustc),
            &args,
            sccache_dir.path(),
            &output,
            "sccache Rust cold link",
        ) {
            Ok(duration) => duration,
            Err(error) => {
                eprintln!(
                    "        sccache Rust link passthrough failed; using direct rustc as no-cache baseline\n        {}",
                    error.lines().next().unwrap_or("unknown failure")
                );
                run_rust_final_link_timed(
                    Path::new(&rustc),
                    &args,
                    sccache_dir.path(),
                    &output,
                    "direct Rust no-cache cold link",
                )
            }
        };
        eprintln!("        cold: {}", fmt_dur(cold));
        let mut passthrough_supported = true;
        let mut warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
        for _ in 0..RUSTC_WARM_TRIALS {
            let duration = if passthrough_supported {
                match try_run_sccache_rust_final_link_timed(
                    &sccache_bin,
                    Path::new(&rustc),
                    &args,
                    sccache_dir.path(),
                    &output,
                    "sccache Rust warm link",
                ) {
                    Ok(duration) => duration,
                    Err(_) => {
                        passthrough_supported = false;
                        run_rust_final_link_timed(
                            Path::new(&rustc),
                            &args,
                            sccache_dir.path(),
                            &output,
                            "direct Rust no-cache warm link",
                        )
                    }
                }
            } else {
                run_rust_final_link_timed(
                    Path::new(&rustc),
                    &args,
                    sccache_dir.path(),
                    &output,
                    "direct Rust no-cache warm link",
                )
            };
            warm.push(duration);
        }
        print_trials("warm:", &warm);
        stop_sccache(&sccache_bin);
        eprintln!();
        (Some(cold), Some(warm))
    } else {
        eprintln!("  [2/3] sccache: not found, skipping");
        eprintln!();
        (None, None)
    };

    eprintln!("  [3/3] zccache");
    let _ = run_rust_final_link_timed(
        Path::new(&rustc),
        &args,
        zccache_dir.path(),
        &output,
        "zccache Rust linker warmup",
    );
    let (_zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    clear_zccache(&mut client).await;
    let zccache_cwd = zccache_dir.path().to_string_lossy().into_owned();
    let session_id = start_zccache_session(&mut client, &zccache_cwd).await;
    let zccache_cold = run_zccache_rust_final_link_timed(
        &mut client,
        &session_id,
        Path::new(&rustc),
        &args,
        zccache_dir.path(),
        &output,
        false,
    )
    .await;
    eprintln!("        cold: {}", fmt_dur(zccache_cold));
    let mut zccache_warm = Vec::with_capacity(RUSTC_WARM_TRIALS);
    for _ in 0..RUSTC_WARM_TRIALS {
        zccache_warm.push(
            run_zccache_rust_final_link_timed(
                &mut client,
                &session_id,
                Path::new(&rustc),
                &args,
                zccache_dir.path(),
                &output,
                true,
            )
            .await,
        );
    }
    print_trials("warm:", &zccache_warm);
    end_zccache_session(&mut client, session_id).await;
    shutdown.notify_one();
    server_handle.await.unwrap();

    let result = LinkBenchResult {
        scenario: "Workspace staticlib link",
        bare_cold,
        bare_warm: median(&bare_warm),
        sccache_cold,
        sccache_warm,
        zccache_cold,
        zccache_warm,
    };
    print_link_benchmark_table(
        &format!(
            "## Rust Workspace Link Benchmark: {RUSTC_NUM_FILES} .rlib inputs, {RUSTC_WARM_TRIALS} warm trials"
        ),
        "Bare rustc",
        &[result],
    );
}
