//! Link/archive scenario measurement: cold + warm trials across bare /
//! sccache / zccache, plus the input-preparation helpers used by the
//! archive, driver-link, emcc-link, and rust-staticlib-link tests.

use std::path::Path;
use std::time::{Duration, Instant};
use zccache::protocol::{Request, Response};

use super::common::{
    clean_link_outputs, clear_dir_contents, clear_zccache, find_sccache, fmt_dur, fmt_ratio,
    median, print_trials, run_tool_timed, start_daemon, start_fresh_sccache, stop_sccache,
    try_run_sccache_tool_timed, try_run_tool, ClientConn, NUM_FILES, RUSTC_NUM_FILES, WARM_TRIALS,
};
use super::cpp_project::{generate_project, source_names};
use super::rust_project::{
    generate_rust_project, run_rustc_batch, rust_source_names, rustc_args_for,
};

pub async fn run_zccache_link_timed(
    client: &mut ClientConn,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    expected_cached: bool,
    description: &str,
) -> Duration {
    let start = Instant::now();
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.to_string_lossy().into_owned().into(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();
    let elapsed = start.elapsed();
    match client.recv().await.unwrap() {
        Some(Response::LinkResult {
            exit_code,
            stderr,
            cached,
            warning,
            ..
        }) => {
            assert_eq!(
                exit_code,
                0,
                "{description} failed:\n{}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(
                cached, expected_cached,
                "{description} cached={cached}, expected {expected_cached}; warning={warning:?}"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }
    elapsed
}

pub struct LinkBenchResult {
    pub scenario: &'static str,
    pub bare_cold: Duration,
    pub bare_warm: Duration,
    pub sccache_cold: Option<Duration>,
    pub sccache_warm: Option<Vec<Duration>>,
    pub zccache_cold: Duration,
    pub zccache_warm: Vec<Duration>,
}

pub async fn measure_ephemeral_link_scenario(
    scenario: &'static str,
    tool: &Path,
    args: &[String],
    outputs: &[String],
    bare_dir: &Path,
    sccache_dir: &Path,
    zccache_dir: &Path,
) -> LinkBenchResult {
    eprintln!("  Scenario: {scenario}");
    eprintln!();

    eprintln!("  [1/3] Bare {}", tool.display());
    clean_link_outputs(bare_dir, outputs);
    let _ = run_tool_timed(tool, args, bare_dir, "bare link warmup");
    clean_link_outputs(bare_dir, outputs);
    let bare_cold = run_tool_timed(tool, args, bare_dir, "bare cold link");
    eprintln!("        cold: {}", fmt_dur(bare_cold));
    let mut bare_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        clean_link_outputs(bare_dir, outputs);
        bare_warm.push(run_tool_timed(tool, args, bare_dir, "bare warm link"));
    }
    print_trials("warm:", &bare_warm);
    eprintln!();

    let (sccache_cold, sccache_warm) = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
        let _cache_dir = start_fresh_sccache(&sccache_bin, sc_cache_dir.path());
        eprintln!("  [2/3] sccache ({})", sccache_bin.display());

        clean_link_outputs(sccache_dir, outputs);
        let cold = match try_run_sccache_tool_timed(
            &sccache_bin,
            tool,
            args,
            sccache_dir,
            "sccache cold link",
        ) {
            Ok(duration) => duration,
            Err(error) => {
                eprintln!(
                    "        sccache link passthrough failed; using direct tool as no-cache baseline\n        {}",
                    error.lines().next().unwrap_or("unknown failure")
                );
                run_tool_timed(tool, args, sccache_dir, "direct no-cache cold link")
            }
        };
        eprintln!("        cold: {}", fmt_dur(cold));

        let mut passthrough_supported = true;
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            clean_link_outputs(sccache_dir, outputs);
            let duration = if passthrough_supported {
                match try_run_sccache_tool_timed(
                    &sccache_bin,
                    tool,
                    args,
                    sccache_dir,
                    "sccache warm link",
                ) {
                    Ok(duration) => duration,
                    Err(_) => {
                        passthrough_supported = false;
                        run_tool_timed(tool, args, sccache_dir, "direct no-cache warm link")
                    }
                }
            } else {
                run_tool_timed(tool, args, sccache_dir, "direct no-cache warm link")
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
    clean_link_outputs(zccache_dir, outputs);
    let _ = run_tool_timed(tool, args, zccache_dir, "zccache linker warmup");
    clean_link_outputs(zccache_dir, outputs);

    let (_zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    clear_zccache(&mut client).await;

    let zccache_cold = run_zccache_link_timed(
        &mut client,
        tool,
        args,
        zccache_dir,
        false,
        "zccache cold link",
    )
    .await;
    eprintln!("        cold: {}", fmt_dur(zccache_cold));
    let mut zccache_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        clean_link_outputs(zccache_dir, outputs);
        zccache_warm.push(
            run_zccache_link_timed(
                &mut client,
                tool,
                args,
                zccache_dir,
                true,
                "zccache warm link",
            )
            .await,
        );
    }
    print_trials("warm:", &zccache_warm);
    shutdown.notify_one();
    server_handle.await.unwrap();
    eprintln!();

    LinkBenchResult {
        scenario,
        bare_cold,
        bare_warm: median(&bare_warm),
        sccache_cold,
        sccache_warm,
        zccache_cold,
        zccache_warm,
    }
}

pub fn print_link_benchmark_table(title: &str, bare_label: &str, results: &[LinkBenchResult]) {
    let dash = "\u{2014}";
    eprintln!();
    eprintln!("{title}");
    eprintln!();
    eprintln!("| Scenario | {bare_label} | sccache | zccache | vs sccache | vs {bare_label} |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");
    for result in results {
        let cold_sccache = result.sccache_cold.map(fmt_dur);
        let cold_vs_sccache = result
            .sccache_cold
            .map(|duration| fmt_ratio(duration, result.zccache_cold, false));
        let cold_vs_bare = fmt_ratio(result.bare_cold, result.zccache_cold, false);
        eprintln!(
            "| {}, Cold | {} | {} | {} | {} | {} |",
            result.scenario,
            fmt_dur(result.bare_cold),
            cold_sccache.as_deref().unwrap_or(dash),
            fmt_dur(result.zccache_cold),
            cold_vs_sccache.as_deref().unwrap_or(dash),
            cold_vs_bare,
        );

        let zccache_warm = median(&result.zccache_warm);
        let warm_sccache = result
            .sccache_warm
            .as_ref()
            .map(|times| fmt_dur(median(times)));
        let warm_vs_sccache = result
            .sccache_warm
            .as_ref()
            .map(|times| fmt_ratio(median(times), zccache_warm, true));
        let warm_vs_bare = fmt_ratio(result.bare_warm, zccache_warm, true);
        eprintln!(
            "| {}, Warm | {} | {} | **{}** | {} | {} |",
            result.scenario,
            fmt_dur(result.bare_warm),
            warm_sccache.as_deref().unwrap_or(dash),
            fmt_dur(zccache_warm),
            warm_vs_sccache.as_deref().unwrap_or(dash),
            warm_vs_bare,
        );
    }
    eprintln!();
    eprintln!(
        "> **Cold** = first link/archive with an empty zccache. **Warm** = median of {WARM_TRIALS} subsequent cached output restores."
    );
    eprintln!();
}

// ── Archive / driver-link input preparation ─────────────────────────────

pub fn fake_archive_object_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.o")).collect()
}

pub fn prepare_fake_archive_inputs(dir: &Path) -> Vec<String> {
    clear_dir_contents(dir);
    let names = fake_archive_object_names();
    for (i, name) in names.iter().enumerate() {
        let mut content = Vec::with_capacity(4096);
        for n in 0..128 {
            content.extend_from_slice(format!("fake c object {i:03} record {n:03}\n").as_bytes());
        }
        std::fs::write(dir.join(name), content).unwrap();
    }
    names
}

pub fn archive_link_args(output: &str, objects: &[String]) -> Vec<String> {
    let mut args = vec!["rcsD".to_string(), output.to_string()];
    args.extend(objects.iter().cloned());
    args
}

pub fn prepare_cpp_link_inputs(compiler: &str, dir: &Path) -> Result<Vec<String>, String> {
    clear_dir_contents(dir);
    generate_project(dir);
    std::fs::write(dir.join("main.cpp"), "int main() { return 0; }\n").unwrap();

    let mut objects = Vec::with_capacity(NUM_FILES + 1);
    for src in source_names() {
        let obj = src.replace(".cpp", ".o");
        let args = vec![
            "-c".to_string(),
            src.clone(),
            "-o".to_string(),
            obj.clone(),
            "-Iinclude".to_string(),
            "-O2".to_string(),
            "-std=c++17".to_string(),
        ];
        try_run_tool(Path::new(compiler), &args, dir, "compile C++ link input")?;
        objects.push(obj);
    }
    let args = vec![
        "-c".to_string(),
        "main.cpp".to_string(),
        "-o".to_string(),
        "main.o".to_string(),
        "-O2".to_string(),
        "-std=c++17".to_string(),
    ];
    try_run_tool(
        Path::new(compiler),
        &args,
        dir,
        "compile C++ main link input",
    )?;
    objects.push("main.o".to_string());
    Ok(objects)
}

pub fn driver_link_args(output: &str, objects: &[String]) -> Vec<String> {
    let mut args = vec!["-o".to_string(), output.to_string()];
    args.extend(objects.iter().cloned());
    args
}

// ── Rust workspace link helpers ─────────────────────────────────────────

pub fn rust_final_output_name() -> String {
    if cfg!(windows) {
        "rust_link_app.lib".to_string()
    } else {
        "librust_link_app.a".to_string()
    }
}

pub fn rust_rlib_path(index: usize) -> String {
    format!("deps/libunit_{index:03}-unit_{index:03}.rlib")
}

pub fn rust_final_link_args(output: &str) -> Vec<String> {
    let mut args = vec![
        "--edition".to_string(),
        "2021".to_string(),
        "--crate-type".to_string(),
        "staticlib".to_string(),
        "--crate-name".to_string(),
        "rust_link_app".to_string(),
        "--emit=link".to_string(),
        "-C".to_string(),
        "metadata=rust_link_app".to_string(),
        "-L".to_string(),
        "dependency=deps".to_string(),
        "lib.rs".to_string(),
        "-o".to_string(),
        output.to_string(),
    ];
    for i in 0..RUSTC_NUM_FILES {
        args.push("--extern".to_string());
        args.push(format!("unit_{i:03}={}", rust_rlib_path(i)));
    }
    args
}

pub fn prepare_rust_link_inputs(rustc: &str, dir: &Path) -> Result<(), String> {
    clear_dir_contents(dir);
    generate_rust_project(dir);
    let srcs = rust_source_names();
    run_rustc_batch(rustc, dir, &srcs, rustc_args_for);
    for i in 0..RUSTC_NUM_FILES {
        let rlib = dir.join(rust_rlib_path(i));
        if !rlib.exists() {
            return Err(format!("expected rlib missing: {}", rlib.display()));
        }
    }

    let mut lib_rs = String::new();
    for i in 0..RUSTC_NUM_FILES {
        lib_rs.push_str(&format!("extern crate unit_{i:03};\n"));
    }
    lib_rs.push_str(
        "\n#[no_mangle]\npub extern \"C\" fn zccache_link_entry() -> f64 {\n    let mut acc = 0.0_f64;\n",
    );
    for i in 0..RUSTC_NUM_FILES {
        lib_rs.push_str(&format!("    acc += unit_{i:03}::compute_{i:03}({i});\n"));
    }
    lib_rs.push_str("    acc\n}\n");
    std::fs::write(dir.join("lib.rs"), lib_rs).unwrap();
    Ok(())
}

pub fn clean_rust_final_output(cwd: &Path, output: &str) {
    clean_link_outputs(cwd, &[output.to_string()]);
}

pub fn run_rust_final_link_timed(
    rustc: &Path,
    args: &[String],
    cwd: &Path,
    output: &str,
    description: &str,
) -> Duration {
    clean_rust_final_output(cwd, output);
    run_tool_timed(rustc, args, cwd, description)
}

pub fn try_run_sccache_rust_final_link_timed(
    sccache: &Path,
    rustc: &Path,
    args: &[String],
    cwd: &Path,
    output: &str,
    description: &str,
) -> Result<Duration, String> {
    clean_rust_final_output(cwd, output);
    try_run_sccache_tool_timed(sccache, rustc, args, cwd, description)
}

pub async fn run_zccache_rust_final_link_timed(
    client: &mut ClientConn,
    session_id: &str,
    rustc: &Path,
    args: &[String],
    cwd: &Path,
    output: &str,
    expected_cached: bool,
) -> Duration {
    clean_rust_final_output(cwd, output);
    let start = Instant::now();
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().into_owned().into(),
            compiler: rustc.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    let elapsed = start.elapsed();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code,
            stderr,
            cached,
            ..
        }) => {
            assert_eq!(
                exit_code,
                0,
                "zccache Rust link failed:\n{}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(
                cached, expected_cached,
                "zccache Rust link cached={cached}, expected {expected_cached}"
            );
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    elapsed
}
