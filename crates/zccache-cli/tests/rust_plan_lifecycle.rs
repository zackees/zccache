use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use zccache_artifact::{
    RustArtifactClass, RustArtifactPlanV1, RustPlanInputs, RustPlanMode, RustPlanPackages,
    RustToolchainIdentity,
};

fn zccache_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zccache"))
}

fn write_file(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("file parent")).expect("create parent dirs");
    std::fs::write(path, contents).expect("write file");
}

fn synthetic_target(root: &Path) {
    let debug = root.join("target").join("debug");

    write_file(
        &debug.join("deps").join("libserde-abc123.rlib"),
        b"serde rlib",
    );
    write_file(
        &debug.join("deps").join("libserde-abc123.rmeta"),
        b"serde rmeta",
    );
    write_file(
        &debug.join("deps").join("serde-abc123.d"),
        b"serde dep-info",
    );
    write_file(
        &debug.join("deps").join("libapp-abc123.rlib"),
        b"workspace rlib",
    );
    write_file(
        &debug.join("deps").join("liblocal_dep-abc123.rlib"),
        b"path dep rlib",
    );

    write_file(
        &debug
            .join(".fingerprint")
            .join("serde-abc123")
            .join("dep-lib-serde"),
        b"fingerprint",
    );

    write_file(
        &debug.join("build").join("serde-abc123").join("output"),
        b"stdout",
    );
    write_file(
        &debug
            .join("build")
            .join("serde-abc123")
            .join("invoked.timestamp"),
        b"timestamp",
    );
    write_file(
        &debug
            .join("build")
            .join("serde-abc123")
            .join("out")
            .join("gen.rs"),
        b"generated",
    );

    write_file(&debug.join("incremental").join("state.bin"), b"incremental");
}

fn synthetic_plan(root: &Path) -> RustArtifactPlanV1 {
    RustArtifactPlanV1 {
        schema_version: 1,
        mode: RustPlanMode::Thin,
        workspace_root: root.into(),
        target_dir: root.join("target").into(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: std::env::var("HOST").unwrap_or_else(|_| {
                if cfg!(windows) {
                    "x86_64-pc-windows-msvc".to_string()
                } else {
                    "x86_64-unknown-linux-gnu".to_string()
                }
            }),
        },
        target_triple: if cfg!(windows) {
            "x86_64-pc-windows-msvc".to_string()
        } else {
            "x86_64-unknown-linux-gnu".to_string()
        },
        profile: "debug".to_string(),
        inputs: RustPlanInputs {
            features_hash: "features".to_string(),
            rustflags_hash: "rustflags".to_string(),
            env_hash: "env".to_string(),
            lockfile_hash: "lockfile".to_string(),
            cargo_config_hash: "config".to_string(),
            manifest_hashes: vec!["manifest".to_string()],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![
                "app 0.1.0".to_string(),
                "serde 1.0.0".to_string(),
                "local_dep 0.1.0".to_string(),
            ],
            workspace_package_ids: vec!["app 0.1.0".to_string()],
            excluded_path_package_ids: vec!["local_dep 0.1.0".to_string()],
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

fn write_plan_json(path: &Path, plan: &RustArtifactPlanV1) {
    let json = serde_json::to_string_pretty(plan).expect("serialize plan");
    std::fs::write(path, json).expect("write plan json");
}

fn run_rust_plan(args: &[&str]) -> Value {
    let output = Command::new(zccache_bin())
        .args(["rust-plan"])
        .args(args)
        .output()
        .expect("run zccache rust-plan");

    if !output.status.success() {
        panic!(
            "zccache rust-plan failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    serde_json::from_slice(&output.stdout).expect("parse JSON summary")
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

#[test]
fn rust_plan_lifecycle_round_trips_cargo_like_target_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let plan = synthetic_plan(root);
    let plan_path = root.join("rust-plan.json");
    let cache_dir = root.join("cache");

    synthetic_target(root);
    write_plan_json(&plan_path, &plan);

    let validate = run_rust_plan(&[
        "validate",
        "--plan",
        &plan_path.to_string_lossy(),
        "--json",
        "--cache-dir",
        &cache_dir.to_string_lossy(),
    ]);
    assert_eq!(json_str(&validate, "operation"), "validate");
    assert_eq!(json_str(&validate, "mode"), "thin");
    assert_eq!(
        validate
            .get("compatibility")
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .expect("validate compatibility status"),
        "ok"
    );
    assert_eq!(json_u64(&validate, "saved_file_count"), 0);
    assert_eq!(json_u64(&validate, "restored_file_count"), 0);
    assert!(validate
        .get("skipped_reasons")
        .expect("validate skipped_reasons")
        .is_object());

    let save = run_rust_plan(&[
        "save",
        "--plan",
        &plan_path.to_string_lossy(),
        "--json",
        "--backend",
        "local",
        "--cache-dir",
        &cache_dir.to_string_lossy(),
    ]);
    assert_eq!(json_str(&save, "operation"), "save");
    assert_eq!(json_u64(&save, "saved_file_count"), 7);
    assert!(json_u64(&save, "saved_bytes") > 0);
    assert_eq!(json_u64(&save, "restored_file_count"), 0);
    assert_eq!(json_u64(&save, "skipped_count"), 3);
    let skipped_reasons = save
        .get("skipped_reasons")
        .and_then(Value::as_object)
        .expect("save skipped_reasons");
    assert_eq!(
        skipped_reasons
            .get("workspace_or_path_dependency_excluded_by_plan")
            .and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        skipped_reasons
            .get("transient_state")
            .and_then(Value::as_u64),
        Some(1)
    );

    std::fs::remove_dir_all(root.join("target")).expect("remove target tree");
    std::fs::create_dir_all(root.join("target").join("debug")).expect("recreate target root");

    let restore = run_rust_plan(&[
        "restore",
        "--plan",
        &plan_path.to_string_lossy(),
        "--json",
        "--backend",
        "local",
        "--cache-dir",
        &cache_dir.to_string_lossy(),
    ]);
    assert_eq!(json_str(&restore, "operation"), "restore");
    assert_eq!(json_u64(&restore, "restored_file_count"), 7);
    assert!(json_u64(&restore, "restored_bytes") > 0);
    assert_eq!(json_u64(&restore, "saved_file_count"), 0);
    assert_eq!(json_u64(&restore, "skipped_count"), 0);
    assert!(restore
        .get("skipped_reasons")
        .and_then(Value::as_object)
        .expect("restore skipped_reasons")
        .is_empty());

    let debug = root.join("target").join("debug");
    assert!(debug.join("deps").join("libserde-abc123.rlib").exists());
    assert!(debug.join("deps").join("libserde-abc123.rmeta").exists());
    assert!(debug.join("deps").join("serde-abc123.d").exists());
    assert!(debug
        .join(".fingerprint")
        .join("serde-abc123")
        .join("dep-lib-serde")
        .exists());
    assert!(debug
        .join("build")
        .join("serde-abc123")
        .join("output")
        .exists());
    assert!(debug
        .join("build")
        .join("serde-abc123")
        .join("invoked.timestamp")
        .exists());
    assert!(debug
        .join("build")
        .join("serde-abc123")
        .join("out")
        .join("gen.rs")
        .exists());
    assert!(!debug.join("deps").join("libapp-abc123.rlib").exists());
    assert!(!debug.join("deps").join("liblocal_dep-abc123.rlib").exists());
    assert!(!debug.join("incremental").join("state.bin").exists());
}
