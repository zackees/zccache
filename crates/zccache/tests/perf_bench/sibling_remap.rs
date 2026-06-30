//! Sibling-workspace `ZCCACHE_PATH_REMAP=auto` helpers shared between the
//! C++ and Emscripten sibling-remap benchmarks. The Rust sibling-remap
//! benchmark uses `path_remap_auto_env` directly from this module but builds
//! its own measurement loop inline.
//!
//! These helpers measure warm-state compile latency when zccache shares cache
//! entries across two sibling git roots via path-remap auto. Bare and sccache
//! run their normal same-workspace warm trials in workspace B (they cannot
//! share across sibling roots). zccache is primed from workspace A, then warm
//! trials measure compiles in workspace B that should hit the sibling cache.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::Path;
use std::time::Duration;

use super::common::{
    dir_size_bytes, end_zccache_session, find_sccache, print_trials_per, start_daemon,
    start_zccache_session, NUM_FILES, WARM_TRIALS,
};
use super::cpp_project::{
    absolute_cpp_source_names, baseline_single, generate_project, generate_project_with_file_tags,
    sccache_compile_single, source_names, warmup_compiler, zccache_compile_cpp_single_with_env,
};

pub fn make_git_workspace(dir: &Path) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
}

pub fn path_remap_auto_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let value = value.into_string().ok()?;
            let is_zccache_root = key.eq_ignore_ascii_case("ZCCACHE_WORKTREE_ROOT");
            let is_zccache_remap = key.eq_ignore_ascii_case("ZCCACHE_PATH_REMAP");
            (!is_zccache_root && !is_zccache_remap).then_some((key, value))
        })
        .collect();
    env.push(("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()));
    env
}

pub struct CppSiblingRemapResult {
    pub scenario: &'static str,
    pub bare_warm: Duration,
    pub sccache_warm: Option<Vec<Duration>>,
    pub zccache_warm: Vec<Duration>,
    pub sccache_cache_bytes: Option<u64>,
    pub zccache_cache_bytes: u64,
}

pub async fn measure_cpp_sibling_remap_mode(
    scenario: &'static str,
    compiler: &str,
    with_file_tags: bool,
    use_absolute_sources: bool,
) -> CppSiblingRemapResult {
    eprintln!("  Mode: {scenario}");
    eprintln!();

    let parent = zccache::test_support::temp_cache_dir().unwrap();
    let workspace_a = parent.path().join("workspace-a");
    let workspace_b = parent.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    make_git_workspace(&workspace_a);
    make_git_workspace(&workspace_b);
    if with_file_tags {
        generate_project_with_file_tags(&workspace_a);
        generate_project_with_file_tags(&workspace_b);
    } else {
        generate_project(&workspace_a);
        generate_project(&workspace_b);
    }

    let sources_a = if use_absolute_sources {
        absolute_cpp_source_names(&workspace_a)
    } else {
        source_names()
    };
    let sources_b = if use_absolute_sources {
        absolute_cpp_source_names(&workspace_b)
    } else {
        source_names()
    };

    eprintln!("  [1/3] Bare clang (workspace B, warm)");
    warmup_compiler(compiler, &workspace_b);
    let _ = baseline_single(compiler, &workspace_b, &sources_b);
    let mut bl_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        bl_warm.push(baseline_single(compiler, &workspace_b, &sources_b));
    }
    print_trials_per("warm:", &bl_warm, Some(NUM_FILES));
    eprintln!();

    let mut sccache_cache_bytes = None;
    let sccache_warm = if let Some(sccache_bin) = find_sccache() {
        let sc_cache_dir = zccache::test_support::temp_cache_dir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);
        eprintln!("  [2/3] sccache (prime: workspace A, warm: workspace B)");
        let mut warm = Vec::with_capacity(WARM_TRIALS);
        for trial in 0..WARM_TRIALS {
            if with_file_tags || trial == 0 {
                let _ = std::process::Command::new(&sccache_bin)
                    .arg("--stop-server")
                    .env("SCCACHE_DIR", &sc_cache_str)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if with_file_tags {
                    super::common::clear_dir_contents(sc_cache_dir.path());
                }
                let _ = std::process::Command::new(&sccache_bin)
                    .arg("--start-server")
                    .env("SCCACHE_DIR", &sc_cache_str)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                warmup_compiler(compiler, &workspace_a);
                let _ = sccache_compile_single(&sccache_bin, compiler, &workspace_a, &sources_a);
            }
            warm.push(sccache_compile_single(
                &sccache_bin,
                compiler,
                &workspace_b,
                &sources_b,
            ));
        }
        print_trials_per("warm:", &warm, Some(NUM_FILES));
        sccache_cache_bytes = Some(dir_size_bytes(sc_cache_dir.path()));
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

    eprintln!("  [3/3] zccache (prime: workspace A, warm: workspace B, remap=auto)");
    let (zccache_cache_dir, endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    let workspace_a_str = workspace_a.to_string_lossy().into_owned();
    let workspace_b_str = workspace_b.to_string_lossy().into_owned();
    let session_a = start_zccache_session(&mut client, &workspace_a_str).await;

    warmup_compiler(compiler, &workspace_a);
    let _ = zccache_compile_cpp_single_with_env(
        &mut client,
        &session_a,
        compiler,
        &workspace_a_str,
        &sources_a,
        path_remap_auto_env(),
    )
    .await;
    end_zccache_session(&mut client, session_a).await;

    let session_b = start_zccache_session(&mut client, &workspace_b_str).await;
    let mut zc_warm = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_warm.push(
            zccache_compile_cpp_single_with_env(
                &mut client,
                &session_b,
                compiler,
                &workspace_b_str,
                &sources_b,
                path_remap_auto_env(),
            )
            .await,
        );
    }
    print_trials_per("warm:", &zc_warm, Some(NUM_FILES));
    let zccache_cache_bytes = dir_size_bytes(zccache_cache_dir.path());

    end_zccache_session(&mut client, session_b).await;
    shutdown.notify_one();
    server_handle.await.unwrap();
    eprintln!();

    CppSiblingRemapResult {
        scenario,
        bare_warm: super::common::median(&bl_warm),
        sccache_warm,
        zccache_warm: zc_warm,
        sccache_cache_bytes,
        zccache_cache_bytes,
    }
}
