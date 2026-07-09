#![cfg(test)]
//! Unit tests for the `rust_plan` module.
//!
//! Tests are grouped by topic into submodules; this file owns the shared
//! helpers and sample-plan factories that the submodules consume via
//! `super::*`.

use super::*;
use std::path::Path;

mod classes_and_packages;
mod delta;
mod restore_errors;
mod save_restore;
mod schema_validation;
mod summary_tests;
mod tar_threads;
mod thin_v2;

pub(super) fn sample_plan(root: &Path, mode: RustPlanMode) -> RustArtifactPlanV1 {
    RustArtifactPlanV1 {
        schema_version: 1,
        mode,
        workspace_root: root.into(),
        target_dir: root.join("target").into(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.94.1".to_string(),
            cargo: "cargo 1.94.1".to_string(),
            channel: "1.94.1".to_string(),
            host: "x86_64-pc-windows-msvc".to_string(),
        },
        target_triple: "x86_64-pc-windows-msvc".to_string(),
        profile: "debug".to_string(),
        inputs: RustPlanInputs {
            features_hash: "features".to_string(),
            rustflags_hash: "rustflags".to_string(),
            env_hash: "env".to_string(),
            lockfile_hash: "lock".to_string(),
            cargo_config_hash: "config".to_string(),
            manifest_hashes: vec!["manifest".to_string()],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["app 0.1.0".to_string()],
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
        journal_log_path: Some(root.join("zccache-session.jsonl").into()),
        cache_profile: None,
        dropped_artifact_classes: Vec::new(),
    }
}

pub(super) fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub(super) fn set_mtime_nanos(path: &Path, nanos: u64) {
    let time = unix_nanos_to_system_time(nanos);
    let file_times = std::fs::FileTimes::new()
        .set_accessed(time)
        .set_modified(time);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.set_times(file_times).unwrap();
}

pub(super) fn file_mtime_nanos(path: &Path) -> u64 {
    system_time_to_unix_nanos(std::fs::metadata(path).unwrap().modified().unwrap())
}

pub(super) fn load_manifest(bundle_dir: &Path) -> RustArtifactBundleManifest {
    read_bundle_manifest(bundle_dir).unwrap()
}

pub(super) fn write_manifest(bundle_dir: &Path, manifest: &RustArtifactBundleManifest) {
    write_bundle_manifest(bundle_dir, manifest).unwrap();
}

pub(super) fn synthetic_target(root: &Path) {
    let target = root.join("target").join("debug");
    write(
        &target.join("deps").join("libserde-abc.rlib"),
        b"serde rlib",
    );
    write(
        &target.join("deps").join("libserde-abc.rmeta"),
        b"serde rmeta",
    );
    write(&target.join("deps").join("serde-abc.d"), b"serde depinfo");
    write(
        &target.join("deps").join("libapp-abc.rlib"),
        b"workspace rlib",
    );
    write(
        &target.join("deps").join("liblocal_dep-abc.rlib"),
        b"path dep rlib",
    );
    write(
        &target
            .join(".fingerprint")
            .join("serde-abc")
            .join("dep-lib-serde"),
        b"fingerprint",
    );
    write(
        &target
            .join("build")
            .join("serde-abc")
            .join("invoked.timestamp"),
        b"timestamp",
    );
    write(
        &target
            .join("build")
            .join("serde-abc")
            .join("out")
            .join("gen.rs"),
        b"generated",
    );
    write(&target.join("incremental").join("state.bin"), b"transient");
}

pub(super) fn synthetic_target_with_final_binary(root: &Path) {
    synthetic_target(root);
    let target = root.join("target").join("debug");
    #[cfg(windows)]
    write(&target.join("app.exe"), b"final binary");
    #[cfg(not(windows))]
    write(&target.join("app"), b"final binary");
}

pub(super) fn synthetic_target_with_proc_macro_outputs(root: &Path) {
    synthetic_target(root);
    let target = root.join("target").join("debug");
    #[cfg(windows)]
    let proc_macro = target.join("deps").join("libproc_macro2-def456.dll");
    #[cfg(not(windows))]
    let proc_macro = target.join("deps").join("libproc_macro2-def456.so");
    write(&proc_macro, b"proc-macro dylib");

    #[cfg(windows)]
    let shared_lib = target.join("deps").join("libserde_shared-def456.dll");
    #[cfg(not(windows))]
    let shared_lib = target.join("deps").join("libserde_shared-def456.so");
    write(&shared_lib, b"shared lib");
}

pub(super) fn synthetic_target_with_package_exclusions(root: &Path) {
    synthetic_target(root);
    let target = root.join("target").join("debug");
    write(
        &target.join("deps").join("libapp-abc.rmeta"),
        b"workspace rmeta",
    );
    write(&target.join("deps").join("app-abc.d"), b"workspace depinfo");
    write(
        &target.join("deps").join("liblocal_dep-abc.rmeta"),
        b"path dep rmeta",
    );
    write(
        &target.join("deps").join("local_dep-abc.d"),
        b"path dep depinfo",
    );
    write(
        &target
            .join(".fingerprint")
            .join("app-abc")
            .join("dep-lib-app"),
        b"workspace fingerprint",
    );
    write(
        &target
            .join(".fingerprint")
            .join("local_dep-abc")
            .join("dep-lib-local_dep"),
        b"path dep fingerprint",
    );
    write(
        &target
            .join("build")
            .join("app-abc")
            .join("invoked.timestamp"),
        b"workspace timestamp",
    );
    write(
        &target
            .join("build")
            .join("local_dep-abc")
            .join("invoked.timestamp"),
        b"path dep timestamp",
    );
    write(
        &target
            .join("build")
            .join("app-abc")
            .join("out")
            .join("gen.rs"),
        b"workspace generated",
    );
    write(
        &target
            .join("build")
            .join("local_dep-abc")
            .join("out")
            .join("gen.rs"),
        b"path dep generated",
    );
}
