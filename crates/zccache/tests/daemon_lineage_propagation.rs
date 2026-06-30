//! Integration tests for spawn-lineage env propagation.
//!
//! These tests spawn a real child process and verify that the lineage env
//! vars set by `zccache::daemon::lineage::Lineage` actually reach the child.
//! See issue #7: future zombie/orphan investigations rely on these markers
//! being present in every descendant of the daemon.
//!
//! Unix-only because the test runs `/bin/sh -c "printf '%s' \"$VAR\""` to
//! read a single env var out of the child reliably. The unit tests in
//! `zccache::daemon::lineage` already cover the platform-independent
//! invariants of which env vars get set; this file verifies the full
//! spawn-and-receive round trip on the host that supports it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

#![cfg(unix)]

use zccache::daemon::lineage::{
    Lineage, ENV_CLIENT_PID, ENV_DAEMON_PID, ENV_LINEAGE, ENV_ORIGINATOR, ENV_PARENT_PID,
    ENV_SESSION_ID,
};

/// Spawn a tiny "print one env var" child and return its stdout.
fn capture_child_env(
    apply: impl FnOnce(&mut tokio::process::Command),
    var: &str,
) -> Option<String> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.args(["-c", &format!("printf '%s' \"${{{var}}}\"")]);
    apply(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let output = rt
        .block_on(async { cmd.spawn().expect("spawn").wait_with_output().await })
        .expect("wait");

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

#[test]
fn lineage_env_reaches_real_child() {
    let lineage = Lineage {
        daemon_pid: 12345,
        client_pid: Some(99),
        session_id: Some("test-session".into()),
    };

    let originator =
        capture_child_env(|cmd| lineage.apply_to_tokio(cmd, None), ENV_ORIGINATOR).unwrap();
    assert_eq!(originator, "zccache:12345");

    let chain = capture_child_env(|cmd| lineage.apply_to_tokio(cmd, None), ENV_LINEAGE).unwrap();
    assert_eq!(chain, "99>12345");

    let daemon_pid =
        capture_child_env(|cmd| lineage.apply_to_tokio(cmd, None), ENV_DAEMON_PID).unwrap();
    assert_eq!(daemon_pid, "12345");

    let parent_pid =
        capture_child_env(|cmd| lineage.apply_to_tokio(cmd, None), ENV_PARENT_PID).unwrap();
    assert_eq!(parent_pid, "12345");

    let client_pid =
        capture_child_env(|cmd| lineage.apply_to_tokio(cmd, None), ENV_CLIENT_PID).unwrap();
    assert_eq!(client_pid, "99");

    let session_id =
        capture_child_env(|cmd| lineage.apply_to_tokio(cmd, None), ENV_SESSION_ID).unwrap();
    assert_eq!(session_id, "test-session");
}

#[test]
fn nested_spawn_extends_chain() {
    // Round 1: a "build" tool spawns the CLI. It claims the originator slot
    // and seeds the lineage with its own PID.
    let build_env: Vec<(String, String)> = vec![
        (ENV_ORIGINATOR.into(), "build:7".into()),
        (ENV_LINEAGE.into(), "7".into()),
    ];

    // Round 2: the CLI spawns the daemon, which now spawns a compiler.
    // From the daemon's viewpoint, `build_env` is the incoming env.
    let daemon_lineage = Lineage {
        daemon_pid: 200,
        client_pid: Some(100), // CLI PID
        session_id: None,
    };

    let chain = capture_child_env(
        |cmd| {
            // Replicate apply_client_env: clear, replay env, overlay lineage.
            cmd.env_clear();
            for (k, v) in &build_env {
                cmd.env(k, v);
            }
            daemon_lineage.apply_to_tokio(cmd, Some(&build_env));
        },
        ENV_LINEAGE,
    )
    .unwrap();
    assert_eq!(chain, "7>100>200");

    // The outer originator must NOT be overwritten — it identifies the
    // outermost contained owner that running-process-style scanners look for.
    let originator = capture_child_env(
        |cmd| {
            cmd.env_clear();
            for (k, v) in &build_env {
                cmd.env(k, v);
            }
            daemon_lineage.apply_to_tokio(cmd, Some(&build_env));
        },
        ENV_ORIGINATOR,
    )
    .unwrap();
    assert_eq!(originator, "build:7");
}

/// `apply_to_sync` is used for the linker / post-link-deploy / system-include
/// passes that run on a `std::process::Command`. Verify the env actually
/// reaches a synchronously-spawned child — the spawn helper has different
/// plumbing from the tokio version and could regress independently.
#[test]
fn lineage_env_reaches_sync_child() {
    let lineage = Lineage {
        daemon_pid: 4242,
        client_pid: Some(8),
        session_id: None,
    };
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!("printf '%s' \"${{{var}}}\"", var = ENV_LINEAGE),
    ]);
    lineage.apply_to_sync(&mut cmd, None);
    let output = cmd.output().expect("spawn /bin/sh");
    let chain = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(chain, "8>4242");
}

/// When the CLI's captured env (passed in `client_env`) already includes a
/// running-process originator, the daemon must NOT clobber it — that's the
/// whole point of preserving the outer claim across spawn boundaries.
#[test]
fn outer_originator_survives_full_apply_client_env_flow() {
    let lineage = Lineage {
        daemon_pid: 555,
        client_pid: Some(444),
        session_id: Some("flow".into()),
    };
    let outer = vec![
        (ENV_ORIGINATOR.into(), "fbuild:1".to_string()),
        (ENV_LINEAGE.into(), "1>444".to_string()),
        ("RUSTC".into(), "/usr/bin/rustc".to_string()),
    ];

    // Apply env_clear + replay client env, then overlay lineage — exactly the
    // sequence `apply_client_env_sync` performs in the daemon.
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!(
            "printf '%s\\n%s\\n%s' \"${{{}}}\" \"${{{}}}\" \"${{RUSTC}}\"",
            ENV_ORIGINATOR, ENV_LINEAGE
        ),
    ]);
    cmd.env_clear();
    for (k, v) in &outer {
        cmd.env(k, v);
    }
    lineage.apply_to_sync(&mut cmd, Some(&outer));

    let output = cmd.output().expect("spawn /bin/sh");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("fbuild:1"));
    // CLI's PID was already trailing in the chain — the daemon collapses the
    // duplicate and only appends its own PID.
    assert_eq!(lines.next(), Some("1>444>555"));
    assert_eq!(lines.next(), Some("/usr/bin/rustc"));
}
