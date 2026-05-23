//! Integration tests for `zccache defender-exclusions` (issue #273).
//!
//! On non-Windows the subcommand must be a clean no-op so cross-platform
//! scripts can invoke it unconditionally. On Windows runners we only
//! exercise the read-only `check` mode — the destructive `add`/`remove`
//! verbs touch real machine state and need an admin shell, so we stay
//! out of them in CI.

use std::process::Command;

fn zccache_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zccache")
}

#[cfg(not(windows))]
#[test]
fn non_windows_check_exits_zero_with_known_message() {
    let out = Command::new(zccache_bin())
        .args(["defender-exclusions", "check"])
        .output()
        .expect("spawn zccache");
    assert!(
        out.status.success(),
        "expected exit 0, got {:?}; stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Defender exclusion is Windows-only."),
        "stdout={stdout}"
    );
}

#[cfg(not(windows))]
#[test]
fn non_windows_check_json_emits_supported_false() {
    let out = Command::new(zccache_bin())
        .args(["defender-exclusions", "check", "--json"])
        .output()
        .expect("spawn zccache");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid json");
    assert_eq!(v["supported"], serde_json::Value::Bool(false));
    assert!(v["message"].as_str().unwrap_or("").contains("Windows-only"));
}

#[cfg(not(windows))]
#[test]
fn non_windows_add_and_remove_exit_zero() {
    for sub in ["add", "remove"] {
        let out = Command::new(zccache_bin())
            .args(["defender-exclusions", sub])
            .output()
            .expect("spawn zccache");
        assert!(out.status.success(), "{sub} should no-op on non-Windows");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Defender exclusion is Windows-only."),
            "{sub} stdout={stdout}"
        );
    }
}

/// On Windows runners `check` is read-only and must not crash.
///
/// Either Defender responds (exit 0) or PowerShell/Defender isn't
/// reachable in the runner environment (exit 2 — "unknown"). Anything
/// else is a regression.
#[cfg(windows)]
#[test]
fn windows_check_is_safe_to_invoke() {
    let out = Command::new(zccache_bin())
        .args(["defender-exclusions", "check"])
        .output()
        .expect("spawn zccache");
    let code = out.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 2,
        "unexpected exit {code}; stdout={}; stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
