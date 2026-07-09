//! Tests for `run_post_link_deploy_hook`.
//!
//! These tests use a tiny helper program that writes a file next to the
//! provided output path and exits 0, simulating a real deploy tool like
//! `clang-tool-chain-libdeploy`. They verify:
//!   - the hook runs when invoked
//!   - failures don't panic / propagate (hook is best-effort)
//!   - the env is propagated

use super::super::*;

/// Run the hook with a command that creates a sidecar file next to the
/// output. Verifies the sidecar appears — this is the contract that
/// `side_effect::detect_side_effects` relies on.
#[cfg(unix)] // uses /bin/sh; Windows has its own test below
#[tokio::test]
async fn post_link_deploy_hook_runs_and_creates_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app");
    std::fs::write(&output, b"binary").unwrap();

    // Fake deploy tool: creates a sidecar DLL next to the passed path.
    let script = dir.path().join("fake_deploy.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ntouch \"$(dirname \"$1\")/libruntime.so\"\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .args(["+x"])
        .arg(&script)
        .status()
        .unwrap();

    let cmd_str = script.to_string_lossy().to_string();
    let lineage = super::super::super::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook(&cmd_str, &output, None, &lineage).await;

    assert!(
        dir.path().join("libruntime.so").exists(),
        "hook should have created the sidecar"
    );
}

/// Hook that exits non-zero must not panic — failures are best-effort.
#[cfg(unix)]
#[tokio::test]
async fn post_link_deploy_hook_failure_is_non_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app");
    std::fs::write(&output, b"binary").unwrap();

    // Just exit 1 — no side effect.
    let lineage = super::super::super::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook("false", &output, None, &lineage).await;
    // If we reached here without panic, the test passes. A warning should
    // have been logged by the hook.
}

/// Nonexistent program — hook should log a warning, not panic.
#[tokio::test]
async fn post_link_deploy_hook_nonexistent_program_is_non_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app.dll");
    std::fs::write(&output, b"binary").unwrap();

    let lineage = super::super::super::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook(
        "this-program-does-not-exist-zccache-test-12345",
        &output,
        None,
        &lineage,
    )
    .await;
    // No panic = pass.
}

/// Empty command string — must early-return without attempting to spawn.
#[tokio::test]
async fn post_link_deploy_hook_empty_cmd_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app.dll");
    std::fs::write(&output, b"binary").unwrap();

    let lineage = super::super::super::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook("", &output, None, &lineage).await;
    run_post_link_deploy_hook("   ", &output, None, &lineage).await;
    // No panic = pass.
}

/// Env is propagated to the hook process.
#[cfg(unix)]
#[tokio::test]
async fn post_link_deploy_hook_propagates_env() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app");
    std::fs::write(&output, b"binary").unwrap();

    // Script reads $ZCCACHE_TEST_MARKER from env and writes it to a
    // marker file next to the output.
    let script = dir.path().join("read_env.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s' \"$ZCCACHE_TEST_MARKER\" > \"$(dirname \"$1\")/marker.txt\"\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .args(["+x"])
        .arg(&script)
        .status()
        .unwrap();

    let env = vec![
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("ZCCACHE_TEST_MARKER".to_string(), "hello-hook".to_string()),
    ];
    let lineage = super::super::super::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook(&script.to_string_lossy(), &output, Some(&env), &lineage).await;

    let marker = std::fs::read_to_string(dir.path().join("marker.txt")).unwrap();
    assert_eq!(marker, "hello-hook");
}
