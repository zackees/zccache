//! #1653/#1722: a Rust interface hit is delivered as a read-only hardlink to
//! the cache blob. Cargo can later rebuild that crate *without* the wrapper,
//! and rustc then writes the same output path. rustc writes each output to a
//! temporary file and renames it over the target, so the rebuild must succeed
//! and must leave the shared blob untouched on every OS. The rename over a
//! read-only file is the Windows risk: `MoveFileEx` refuses it there.

use super::super::*;
use std::process::Command;
use std::time::{Duration, SystemTime};

fn build_fixture(cargo: &std::ffi::OsStr, project: &Path) {
    let output = Command::new(cargo)
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
        .expect("toolchain cargo must start");
    assert!(
        output.status.success(),
        "wrapper-less cargo build failed over read-only hardlinked outputs:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn interface_outputs(project: &Path) -> Vec<std::path::PathBuf> {
    let deps = project.join("target").join("debug").join("deps");
    let mut outputs: Vec<_> = std::fs::read_dir(&deps)
        .expect("fixture deps directory")
        .map(|entry| entry.expect("deps entry").path())
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("libprobe") && (name.ends_with(".rmeta") || name.ends_with(".rlib"))
        })
        .collect();
    outputs.sort();
    outputs
}

#[test]
fn wrapperless_rustc_rebuild_replaces_readonly_hardlinked_interfaces() {
    // Cargo sets CARGO for the test binaries it runs; that is the resolved
    // toolchain cargo, so no nested Soldr front door is involved.
    let cargo = std::env::var_os("CARGO").expect("cargo test sets CARGO");
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("probe");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let source = project.join("src").join("lib.rs");
    std::fs::write(&source, "pub fn value() -> u32 { 1 }\n").unwrap();
    build_fixture(&cargo, &project);

    let outputs = interface_outputs(&project);
    assert_eq!(outputs.len(), 2, "one .rmeta and one .rlib: {outputs:?}");

    // Deliver each interface exactly as an AUTO cache hit does: a staged blob,
    // hardlinked (read-only) to the requested output path.
    let artifact_dir = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let mut delivered = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        let original = std::fs::read(output).unwrap();
        let staged_source = dir.path().join(format!("staged-{index}"));
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
            MaterializationMode::Auto,
        )
        .unwrap();
        if !crate::platform::fs::identity::same_file(output, &blob).unwrap() {
            eprintln!("SKIP wrapperless_rustc_rebuild: volume did not hardlink {output:?}");
            return;
        }
        assert!(
            std::fs::metadata(output).unwrap().permissions().readonly(),
            "the probe must exercise a read-only delivered hardlink"
        );
        delivered.push((output.clone(), blob, original));
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
    build_fixture(&cargo, &project);

    for (output, blob, original) in delivered {
        assert_eq!(
            std::fs::read(&blob).unwrap(),
            original,
            "the rebuild wrote through the hardlink into the cache blob {blob:?}"
        );
        assert!(
            verify_registered_blob(&blob).is_ok(),
            "the cache blob must still verify after a wrapper-less rebuild"
        );
        assert!(
            !crate::platform::fs::identity::same_file(&output, &blob).unwrap(),
            "rustc must replace {output:?} with a new file, not reuse the blob inode"
        );
        assert_ne!(
            std::fs::read(&output).unwrap(),
            original,
            "the rebuild must produce new interface bytes at {output:?}"
        );
    }
}
