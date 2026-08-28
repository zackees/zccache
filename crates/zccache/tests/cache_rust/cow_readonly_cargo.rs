//! Cargo compatibility acceptance for read-only COW materialization (#1039).

#![allow(clippy::expect_used, clippy::panic)]

use std::process::Command;

#[test]
#[ignore = "integration: builds a real crate twice through RUSTC_WRAPPER"]
fn readonly_cache_hit_survives_cargo_clean() {
    let fixture = tempfile::tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let cache = fixture.path().join("cache");
    std::fs::create_dir_all(project.join("src")).expect("create project");
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='cow-clean'\nversion='0.1.0'\nedition='2021'\n",
    )
    .expect("manifest");
    std::fs::write(project.join("src/lib.rs"), "pub fn value() -> u32 { 42 }\n").expect("source");
    let zccache = env!("CARGO_BIN_EXE_zccache");

    let run = |verb: &str| {
        Command::new("soldr")
            .args(["cargo", verb])
            .current_dir(&project)
            .env("RUSTC_WRAPPER", zccache)
            .env("ZCCACHE_CACHE_DIR", &cache)
            .env("ZCCACHE_COW_READONLY", "1")
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .unwrap_or_else(|error| panic!("soldr cargo {verb} failed to start: {error}"))
    };
    for cycle in 0..2 {
        let build = run("build");
        assert!(
            build.status.success(),
            "cycle {cycle} build failed: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let clean = run("clean");
        assert!(
            clean.status.success(),
            "cycle {cycle} cargo clean failed: {}",
            String::from_utf8_lossy(&clean.stderr)
        );
    }
    let _ = Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", cache)
        .output();
}
