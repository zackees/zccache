//! #1791/#1792: after a cache hit, Cargo can rebuild the same crate *without*
//! the wrapper (`ZCCACHE_DISABLE=1`, a plain `cargo`, rust-analyzer), and
//! rustc then writes the same output paths. Before it renames its temp file
//! over an output, rustc refuses a read-only one (`check_file_is_writeable`),
//! so a read-only hardlink to the cache blob fails that build.
//!
//! Every mode must leave the cache blob untouched and let the rebuild
//! succeed. Where LINK or REFLINK_OR_LINK_OR_COPY actually hardlinks, the
//! shared file is sealed `r--rw-r--` on Unix (#1791): rustc sees a writable
//! output and renames over it, while the owner's in-place writes are still
//! refused. Windows keeps the READONLY attribute, so a hardlinked delivery
//! there still hits rustc's refusal until the ACL follow-up in #1791.

use super::super::*;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

fn cargo_build(cargo: &std::ffi::OsStr, project: &Path) -> Output {
    Command::new(cargo)
        .args(["build", "--offline", "--quiet"])
        .current_dir(project)
        .env("CARGO_TARGET_DIR", project.join("target"))
        // The rebuild under test is the wrapper-less one: no zccache/soldr
        // wrapper may sit between Cargo and rustc.
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("toolchain cargo must start")
}

/// Whether this process ignores file permissions (root), which would make
/// the in-place-write refusal unobservable.
fn privileged(root: &Path) -> bool {
    let probe = root.join("privilege-probe");
    std::fs::write(&probe, b"x").unwrap();
    crate::platform::fs::permissions::set_readonly(&probe, true).unwrap();
    let writable = std::fs::OpenOptions::new()
        .append(true)
        .open(&probe)
        .is_ok();
    let _ = crate::platform::fs::permissions::make_writable(&probe);
    let _ = std::fs::remove_file(probe);
    writable
}

fn interface_outputs(project: &Path) -> Vec<std::path::PathBuf> {
    // Search all of target/: a CI-provided build target nests outputs under
    // target/<triple>/debug/deps instead of target/debug/deps.
    let mut outputs = Vec::new();
    let mut pending = vec![project.join("target")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("fixture target directory") {
            let path = entry.expect("target entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let in_deps = path.parent().and_then(|p| p.file_name()) == Some("deps".as_ref());
            if in_deps
                && name.starts_with("libprobe")
                && (name.ends_with(".rmeta") || name.ends_with(".rlib"))
            {
                outputs.push(path);
            }
        }
    }
    outputs.sort();
    outputs
}

/// Build the fixture, deliver its interfaces as a hit under `mode`, edit the
/// source, and rebuild without the wrapper. Returns whether any interface was
/// delivered as a hardlink to the cache, and the rebuild's output.
fn hit_then_wrapperless_rebuild(
    cargo: &std::ffi::OsStr,
    root: &Path,
    mode: MaterializationMode,
) -> (bool, Output) {
    let project = root.join("probe");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let source = project.join("src").join("lib.rs");
    std::fs::write(&source, "pub fn value() -> u32 { 1 }\n").unwrap();
    let first = cargo_build(cargo, &project);
    assert!(
        first.status.success(),
        "{mode}: fixture build failed:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let outputs = interface_outputs(&project);
    assert_eq!(
        outputs.len(),
        2,
        "{mode}: one .rmeta and one .rlib under {:?}: {outputs:?}",
        project.join("target")
    );

    // Deliver each interface exactly as a cache hit under `mode` does.
    let artifact_dir = root.join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let mut delivered = Vec::new();
    let mut any_shared = false;
    for (index, output) in outputs.iter().enumerate() {
        let original = std::fs::read(output).unwrap();
        let staged_source = root.join(format!("staged-{index}"));
        std::fs::copy(output, &staged_source).unwrap();
        let key = format!("{index:064}");
        persist_staged_artifact_paths(&artifact_dir, &key, &[staged_source.into()]).unwrap();
        let blob = load_staged_artifact_paths(&artifact_dir, &key, &[original.len() as u64])
            .unwrap()
            .unwrap()
            .remove(0);
        let target = NormalizedPath::from(output.as_path());
        write_payloads_par_with_mtime_floor_and_policies_observed(
            &[&target],
            &[CachedPayload::File(blob.clone())],
            &Vec::<NormalizedPath>::new(),
            &[crate::compiler::DeliveryPolicy::HardlinkEligible],
            mode,
        )
        .unwrap();
        let shared = crate::platform::fs::identity::same_file(output, &blob).unwrap();
        if shared && !kernal_api::platform::host::target_is_windows() && !privileged(root) {
            assert!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(output)
                    .is_err(),
                "{mode}: a sealed shared output must refuse the owner's in-place write"
            );
        }
        if !shared {
            assert!(
                !std::fs::metadata(output).unwrap().permissions().readonly(),
                "{mode}: an independent hit must be writable so rustc accepts {output:?}"
            );
        }
        any_shared |= shared;
        delivered.push((blob, original));
    }

    // A source change newer than every recorded fingerprint forces cargo to
    // rerun rustc over the same output paths.
    std::fs::write(
        &source,
        "pub fn value() -> u32 { 2 }\npub fn added() -> u32 { 3 }\n",
    )
    .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    let rebuild = cargo_build(cargo, &project);

    for (blob, original) in delivered {
        assert_eq!(
            std::fs::read(&blob).unwrap(),
            original,
            "{mode}: the rebuild wrote through into the cache blob {blob:?}"
        );
        assert!(
            verify_registered_blob(&blob).is_ok(),
            "{mode}: the cache blob must still verify after a wrapper-less rebuild"
        );
    }
    (any_shared, rebuild)
}

#[test]
fn wrapperless_rustc_rebuild_after_a_hit_in_every_mode() {
    // Cargo sets CARGO for the test binaries it runs; that is the resolved
    // toolchain cargo, so no nested Soldr front door is involved.
    let cargo = std::env::var_os("CARGO").expect("cargo test sets CARGO");
    for mode in MaterializationMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let (shared, rebuild) = hit_then_wrapperless_rebuild(&cargo, dir.path(), mode);
        let stderr = String::from_utf8_lossy(&rebuild.stderr);
        let linking = matches!(
            mode,
            MaterializationMode::Link | MaterializationMode::ReflinkOrLinkOrCopy
        );
        assert!(
            !shared || linking,
            "{mode} shared the cache inode; only LINK and REFLINK_OR_LINK_OR_COPY may"
        );
        if shared && kernal_api::platform::host::target_is_windows() {
            // #1791's remaining Windows part: READONLY blocks the replace.
            assert!(
                !rebuild.status.success() && stderr.contains("not writeable"),
                "{mode}: expected the #1791 Windows refusal over a read-only hardlink, got \
                 success={} stderr:\n{stderr}",
                rebuild.status.success()
            );
        } else {
            assert!(
                rebuild.status.success(),
                "{mode}: wrapper-less rebuild after a hit failed (shared: {shared}):\n{stderr}"
            );
        }
    }
}
