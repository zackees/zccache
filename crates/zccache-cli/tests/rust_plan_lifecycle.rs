use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use zccache_artifact::{
    RustArtifactClass, RustArtifactPlanV1, RustPlanInputs, RustPlanMode, RustPlanPackages,
    RustToolchainIdentity,
};

fn zccache_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zccache"))
}

fn cargo_bin() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn rust_plan_output<F>(args: &[&str], configure: F) -> Output
where
    F: FnOnce(&mut Command),
{
    let mut cmd = Command::new(zccache_bin());
    cmd.args(["rust-plan"]);
    cmd.args(args);
    configure(&mut cmd);
    cmd.output()
        .unwrap_or_else(|err| panic!("failed to run zccache rust-plan: {err}"))
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(path, contents).expect("write file");
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).expect("read file")
}

fn hash_str(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

fn hash_file(path: &Path) -> String {
    hash_str(&read_to_string(path))
}

fn run_command(mut cmd: Command, label: &str) -> Output {
    let output = cmd
        .output()
        .unwrap_or_else(|err| panic!("failed to run {label}: {err}"));
    if !output.status.success() {
        panic!(
            "{label} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output
}

fn run_rust_plan(args: &[&str]) -> Value {
    let output = run_command(
        {
            let mut cmd = Command::new(zccache_bin());
            cmd.args(["rust-plan"]);
            cmd.args(args);
            cmd
        },
        "zccache rust-plan",
    );

    serde_json::from_slice(&output.stdout).expect("parse rust-plan JSON")
}

fn run_rust_plan_failure(args: &[&str]) -> Output {
    let output = {
        let mut cmd = Command::new(zccache_bin());
        cmd.args(["rust-plan"]);
        cmd.args(args);
        cmd.output()
            .unwrap_or_else(|err| panic!("failed to run zccache rust-plan: {err}"))
    };
    assert!(
        !output.status.success(),
        "zccache rust-plan unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_cargo_build(root: &Path, target_dir: &Path, verbose: bool) -> Output {
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(root);
    cmd.env("CARGO_TERM_COLOR", "never");
    let target_dir = target_dir.to_string_lossy().to_string();
    cmd.args(["build", "--target-dir"]);
    cmd.arg(target_dir);
    if verbose {
        cmd.arg("-vv");
    }
    run_command(cmd, "cargo build")
}

fn toolchain_identity() -> RustToolchainIdentity {
    let rustc = run_command(
        {
            let mut cmd = Command::new("rustc");
            cmd.arg("-Vv");
            cmd
        },
        "rustc -Vv",
    );
    let rustc_text = String::from_utf8_lossy(&rustc.stdout);
    let host = rustc_text
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_string();

    let cargo = run_command(
        {
            let mut cmd = Command::new(cargo_bin());
            cmd.arg("--version");
            cmd
        },
        "cargo --version",
    );

    RustToolchainIdentity {
        rustc: rustc_text.lines().next().unwrap_or("rustc").to_string(),
        cargo: String::from_utf8_lossy(&cargo.stdout).trim().to_string(),
        channel: std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "unknown".to_string()),
        host,
    }
}

fn has_file_with_prefix(dir: &Path, prefix: &str, extension: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(prefix)
                        && path.extension().and_then(|ext| ext.to_str()) == Some(extension)
                })
    })
}

fn has_dir_with_prefix(dir: &Path, prefix: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
    })
}

fn write_workspace(root: &Path) {
    write_file(
        &root.join("Cargo.toml"),
        r#"[workspace]
members = ["app"]
resolver = "2"
"#,
    );

    write_file(
        &root.join("local_dep/Cargo.toml"),
        r#"[package]
name = "local_dep"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    );
    write_file(
        &root.join("local_dep/src/lib.rs"),
        r#"pub fn dep_value() -> i32 {
    41
}
"#,
    );

    write_file(
        &root.join("app/Cargo.toml"),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
local_dep = { path = "../local_dep" }

[lib]
path = "src/lib.rs"
"#,
    );
    write_file(
        &root.join("app/src/lib.rs"),
        r#"pub fn app_value() -> i32 {
    local_dep::dep_value() + 1
}
"#,
    );
}

fn rust_plan_for_workspace(root: &Path, target_dir: &Path) -> RustArtifactPlanV1 {
    let toolchain = toolchain_identity();
    let target_triple = toolchain.host.clone();
    let cargo_toml = root.join("Cargo.toml");
    let app_toml = root.join("app/Cargo.toml");
    let dep_toml = root.join("local_dep/Cargo.toml");
    let lockfile = root.join("Cargo.lock");

    RustArtifactPlanV1 {
        schema_version: 1,
        mode: RustPlanMode::Thin,
        workspace_root: root.into(),
        target_dir: target_dir.into(),
        toolchain,
        target_triple,
        profile: "debug".to_string(),
        inputs: RustPlanInputs {
            features_hash: hash_str("default"),
            rustflags_hash: hash_str(""),
            env_hash: hash_str(""),
            lockfile_hash: hash_file(&lockfile),
            cargo_config_hash: hash_str(""),
            manifest_hashes: vec![
                hash_file(&cargo_toml),
                hash_file(&app_toml),
                hash_file(&dep_toml),
            ],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["app 0.1.0".to_string(), "local_dep 0.1.0".to_string()],
            workspace_package_ids: vec!["app 0.1.0".to_string()],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::CargoFingerprint,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ],
        cache_schema_version: 1,
        journal_log_path: None,
    }
}

fn write_synthetic_full_target(target_dir: &Path) {
    let debug = target_dir.join("debug");
    write_file(&debug.join("app"), "workspace executable");
    write_file(&debug.join("deps/libapp-abc.rlib"), "workspace rlib");
    write_file(&debug.join("deps/app-abc.d"), "workspace depinfo");
    write_file(&debug.join("deps/libdep-abc.rmeta"), "dependency rmeta");
    write_file(
        &debug.join(".fingerprint/app-abc/dep-lib-app"),
        "workspace fingerprint",
    );
    write_file(&debug.join("build/app-abc/output"), "build metadata");
    write_file(
        &debug.join("build/app-abc/out/generated.rs"),
        "generated output",
    );
    write_file(
        &debug.join("incremental/app-abc/session-state.bin"),
        "transient incremental state",
    );
}

fn synthetic_full_plan(root: &Path, target_dir: &Path) -> RustArtifactPlanV1 {
    RustArtifactPlanV1 {
        schema_version: 1,
        mode: RustPlanMode::Full,
        workspace_root: root.into(),
        target_dir: target_dir.into(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0-test".to_string(),
            cargo: "cargo 1.0.0-test".to_string(),
            channel: "test".to_string(),
            host: "x86_64-unknown-test".to_string(),
        },
        target_triple: "x86_64-unknown-test".to_string(),
        profile: "debug".to_string(),
        inputs: RustPlanInputs {
            features_hash: hash_str("default"),
            rustflags_hash: hash_str(""),
            env_hash: hash_str(""),
            lockfile_hash: hash_str("synthetic lockfile"),
            cargo_config_hash: hash_str(""),
            manifest_hashes: vec![hash_str("synthetic manifest")],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["app 0.1.0".to_string()],
            workspace_package_ids: vec!["app 0.1.0".to_string()],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec![],
        cache_schema_version: 1,
        journal_log_path: None,
    }
}

fn json_u64(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing numeric field {key}"))
}

fn json_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field {key}"))
}

fn json_object<'a>(value: &'a Value, key: &str) -> &'a serde_json::Map<String, Value> {
    value
        .get(key)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("missing object field {key}"))
}

#[test]
fn rust_plan_validate_json_reports_compatibility_error_without_cache_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let cache_dir = root.join("cache");
    let plan_path = root.join("bad-rust-plan.json");

    write_file(
        &plan_path,
        r#"{
  "schema_version": 999,
  "cache_schema_version": 1
}
"#,
    );

    let plan_path_str = plan_path.to_string_lossy().to_string();
    let cache_dir_str = cache_dir.to_string_lossy().to_string();
    let output = run_rust_plan_failure(&[
        "validate",
        "--plan",
        &plan_path_str,
        "--json",
        "--cache-dir",
        &cache_dir_str,
    ]);

    let summary: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("parse rust-plan failure JSON: {err}"));
    assert_eq!(json_str(&summary, "operation"), "validate");

    let compatibility = json_object(&summary, "compatibility");
    assert_eq!(
        compatibility.get("status").and_then(Value::as_str),
        Some("error")
    );
    let error_text = compatibility
        .get("errors")
        .and_then(Value::as_array)
        .expect("compatibility errors")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        error_text.contains("unsupported Rust artifact plan schema version 999"),
        "unexpected compatibility error: {error_text}"
    );
    assert!(
        error_text.contains("supported version is 1"),
        "compatibility error should identify the supported schema version: {error_text}"
    );
    assert!(
        !cache_dir.exists(),
        "validate compatibility failures should not create or mutate the cache directory"
    );
}

#[test]
fn rust_plan_auto_backend_without_gha_env_uses_local_backend() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let cache_dir = root.join("cache");
    let target_dir = root.join("target");
    write_synthetic_full_target(&target_dir);

    let plan = synthetic_full_plan(root, &target_dir);
    let plan_path = root.join("auto-plan.json");
    write_file(
        &plan_path,
        &serde_json::to_string_pretty(&plan).expect("serialize plan"),
    );

    let plan_path_str = plan_path.to_string_lossy().to_string();
    let cache_dir_str = cache_dir.to_string_lossy().to_string();
    let output = rust_plan_output(
        &[
            "save",
            "--plan",
            &plan_path_str,
            "--json",
            "--backend",
            "auto",
            "--cache-dir",
            &cache_dir_str,
        ],
        |cmd| {
            cmd.env_remove("ACTIONS_CACHE_URL");
            cmd.env_remove("ACTIONS_RUNTIME_TOKEN");
        },
    );
    assert!(output.status.success(), "auto backend save should succeed");

    let summary: Value =
        serde_json::from_slice(&output.stdout).expect("parse rust-plan auto backend JSON");
    assert_eq!(json_str(&summary, "operation"), "save");
    assert_eq!(json_str(&summary, "backend"), "local");
    assert!(summary["backend_cache_key"].is_null());
    assert!(summary["backend_cache_version"].is_null());
    assert!(!json_str(&summary, "cache_key").is_empty());
}

#[test]
fn rust_plan_explicit_gha_backend_without_env_reports_backend_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let cache_dir = root.join("cache");
    let plan = synthetic_full_plan(root, &root.join("target"));
    let plan_path = root.join("gha-plan.json");
    write_file(
        &plan_path,
        &serde_json::to_string_pretty(&plan).expect("serialize plan"),
    );

    let plan_path_str = plan_path.to_string_lossy().to_string();
    let cache_dir_str = cache_dir.to_string_lossy().to_string();
    let output = rust_plan_output(
        &[
            "restore",
            "--plan",
            &plan_path_str,
            "--json",
            "--backend",
            "gha",
            "--cache-dir",
            &cache_dir_str,
        ],
        |cmd| {
            cmd.env_remove("ACTIONS_CACHE_URL");
            cmd.env_remove("ACTIONS_RUNTIME_TOKEN");
        },
    );
    assert!(
        !output.status.success(),
        "explicit gha backend should fail without env"
    );

    let summary: Value =
        serde_json::from_slice(&output.stdout).expect("parse rust-plan gha failure JSON");
    assert_eq!(json_str(&summary, "operation"), "restore");
    assert_eq!(json_str(&summary, "backend"), "gha");
    assert!(!json_str(&summary, "cache_key").is_empty());
    assert!(!summary["backend_cache_key"].is_null());
    assert!(!summary["backend_cache_version"].is_null());
    let compatibility = json_object(&summary, "compatibility");
    assert_eq!(
        compatibility.get("status").and_then(Value::as_str),
        Some("error")
    );
    let error_text = compatibility
        .get("errors")
        .and_then(Value::as_array)
        .expect("compatibility errors")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        error_text.contains("GHA cache backend unavailable"),
        "expected backend unavailable wording: {error_text}"
    );
}

#[test]
fn rust_plan_session_stats_lookup_errors_surface_in_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let cache_dir = root.join("cache");
    let plan = synthetic_full_plan(root, &root.join("target"));
    let plan_path = root.join("stats-plan.json");
    write_file(
        &plan_path,
        &serde_json::to_string_pretty(&plan).expect("serialize plan"),
    );

    let plan_path_str = plan_path.to_string_lossy().to_string();
    let cache_dir_str = cache_dir.to_string_lossy().to_string();
    let output = rust_plan_output(
        &[
            "validate",
            "--plan",
            &plan_path_str,
            "--json",
            "--session-id",
            "session-123",
            "--endpoint",
            "tcp:127.0.0.1:9",
            "--cache-dir",
            &cache_dir_str,
        ],
        |_| {},
    );
    assert!(output.status.success(), "validate should still succeed");

    let summary: Value =
        serde_json::from_slice(&output.stdout).expect("parse rust-plan session-stats JSON");
    let stats = json_object(&summary, "compile_cache_stats");
    assert_eq!(stats.get("status").and_then(Value::as_str), Some("error"));
    assert_eq!(
        stats.get("session_id").and_then(Value::as_str),
        Some("session-123")
    );
    assert!(
        stats
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("cannot connect to daemon at tcp:127.0.0.1:9")),
        "expected endpoint lookup failure to be surfaced in compile_cache_stats"
    );
}

#[test]
fn rust_plan_full_mode_cli_restores_target_tree_and_prunes_incremental() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let target_dir = root.join("target");
    let cache_dir = root.join("cache");

    write_synthetic_full_target(&target_dir);

    let plan = synthetic_full_plan(root, &target_dir);
    let plan_path = root.join("rust-plan-full.json");
    write_file(
        &plan_path,
        &serde_json::to_string_pretty(&plan).expect("serialize plan"),
    );

    let plan_path_str = plan_path.to_string_lossy().to_string();
    let cache_dir_str = cache_dir.to_string_lossy().to_string();

    let save = run_rust_plan(&[
        "save",
        "--plan",
        &plan_path_str,
        "--json",
        "--backend",
        "local",
        "--cache-dir",
        &cache_dir_str,
    ]);
    assert_eq!(json_str(&save, "operation"), "save");
    assert_eq!(json_str(&save, "mode"), "full");
    assert_eq!(json_u64(&save, "saved_file_count"), 7);
    assert!(json_u64(&save, "saved_bytes") > 0);
    assert_eq!(
        save.get("skipped_reasons")
            .and_then(|reasons| reasons.get("transient_state"))
            .and_then(Value::as_u64),
        Some(1)
    );

    fs::remove_dir_all(&target_dir).expect("remove target dir");

    let restore = run_rust_plan(&[
        "restore",
        "--plan",
        &plan_path_str,
        "--json",
        "--backend",
        "local",
        "--cache-dir",
        &cache_dir_str,
    ]);
    assert_eq!(json_str(&restore, "operation"), "restore");
    assert_eq!(json_str(&restore, "mode"), "full");
    assert_eq!(json_u64(&restore, "restored_file_count"), 7);
    assert!(json_u64(&restore, "restored_bytes") > 0);

    let effectiveness = json_object(&restore, "target_artifact_effectiveness");
    assert_eq!(
        effectiveness
            .get("eligible_file_count")
            .and_then(Value::as_u64),
        Some(7)
    );
    assert_eq!(
        effectiveness
            .get("restored_file_count")
            .and_then(Value::as_u64),
        Some(7)
    );
    assert_eq!(
        effectiveness.get("reuse_ratio").and_then(Value::as_f64),
        Some(1.0)
    );
    assert!(
        restore
            .get("compile_cache_stats")
            .is_some_and(Value::is_null),
        "target artifact effectiveness must be reported separately from compile-cache stats"
    );

    assert!(target_dir.join("debug/app").exists());
    assert!(target_dir.join("debug/deps/libapp-abc.rlib").exists());
    assert!(target_dir.join("debug/deps/app-abc.d").exists());
    assert!(target_dir.join("debug/deps/libdep-abc.rmeta").exists());
    assert!(target_dir
        .join("debug/.fingerprint/app-abc/dep-lib-app")
        .exists());
    assert!(target_dir.join("debug/build/app-abc/output").exists());
    assert!(target_dir
        .join("debug/build/app-abc/out/generated.rs")
        .exists());
    assert!(
        !target_dir
            .join("debug/incremental/app-abc/session-state.bin")
            .exists(),
        "full-mode restore should still prune transient incremental state"
    );
}

#[test]
#[ignore = "Slow real Cargo lifecycle test; run with `cargo test --test rust_plan_lifecycle -- --ignored`"]
fn rust_plan_lifecycle_keeps_path_dep_fresh_and_rebuilds_workspace_crate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let target_dir = root.join("target");
    let cache_dir = root.join("cache");

    write_workspace(root);

    let first_build = run_cargo_build(root, &target_dir, false);
    let first_build_log = format!(
        "{}{}",
        String::from_utf8_lossy(&first_build.stdout),
        String::from_utf8_lossy(&first_build.stderr)
    );
    assert!(
        first_build_log.contains("Compiling local_dep v0.1.0"),
        "initial build should compile local_dep\n{first_build_log}"
    );
    assert!(
        first_build_log.contains("Compiling app v0.1.0"),
        "initial build should compile app\n{first_build_log}"
    );

    let plan = rust_plan_for_workspace(root, &target_dir);
    let plan_path = root.join("rust-plan.json");
    write_file(
        &plan_path,
        &serde_json::to_string_pretty(&plan).expect("serialize plan"),
    );

    let plan_path_str = plan_path.to_string_lossy().to_string();
    let cache_dir_str = cache_dir.to_string_lossy().to_string();

    let save = run_rust_plan(&[
        "save",
        "--plan",
        &plan_path_str,
        "--json",
        "--backend",
        "local",
        "--cache-dir",
        &cache_dir_str,
    ]);
    assert_eq!(json_str(&save, "operation"), "save");
    assert_eq!(json_str(&save, "mode"), "thin");
    assert!(json_u64(&save, "saved_file_count") > 0);
    assert!(json_u64(&save, "saved_bytes") > 0);

    fs::remove_dir_all(&target_dir).expect("remove target dir");

    let restore = run_rust_plan(&[
        "restore",
        "--plan",
        &plan_path_str,
        "--json",
        "--backend",
        "local",
        "--cache-dir",
        &cache_dir_str,
    ]);
    assert_eq!(json_str(&restore, "operation"), "restore");
    assert_eq!(json_str(&restore, "mode"), "thin");
    assert!(json_u64(&restore, "restored_file_count") > 0);
    assert!(json_u64(&restore, "restored_bytes") > 0);

    let deps_dir = target_dir.join("debug/deps");
    let fingerprint_dir = target_dir.join("debug/.fingerprint");
    assert!(
        has_file_with_prefix(&deps_dir, "liblocal_dep-", "rlib"),
        "restore should bring back the path dependency rlib"
    );
    assert!(
        has_dir_with_prefix(&fingerprint_dir, "local_dep-"),
        "restore should bring back the path dependency fingerprint state"
    );
    assert!(
        !has_file_with_prefix(&deps_dir, "libapp-", "rlib"),
        "thin restore should not restore the workspace crate rlib"
    );

    write_file(
        &root.join("app/src/lib.rs"),
        r#"pub fn app_value() -> i32 {
    local_dep::dep_value() + 2
}
"#,
    );

    let rebuild = run_cargo_build(root, &target_dir, true);
    let rebuild_log = format!(
        "{}{}",
        String::from_utf8_lossy(&rebuild.stdout),
        String::from_utf8_lossy(&rebuild.stderr)
    );
    assert!(
        rebuild_log.contains("Fresh local_dep v0.1.0"),
        "path dependency should remain fresh after restore\n{rebuild_log}"
    );
    assert!(
        rebuild_log.contains("Compiling app v0.1.0") || rebuild_log.contains("Dirty app v0.1.0"),
        "workspace crate should rebuild after source edit\n{rebuild_log}"
    );
    assert!(
        !rebuild_log.contains("Fresh app v0.1.0"),
        "workspace crate should not stay fresh after source edit\n{rebuild_log}"
    );
    assert!(
        has_file_with_prefix(&deps_dir, "libapp-", "rlib"),
        "rebuild should recreate the workspace crate rlib"
    );
}
