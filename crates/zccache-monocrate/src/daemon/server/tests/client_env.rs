//! Tests for `apply_client_env` / `apply_client_env_sync` — verify that
//! stale jobserver vars are stripped before the daemon spawns a compiler
//! or tool subprocess, and that lineage env-vars are propagated.

use super::super::*;

fn collect_command_env<'a, I>(envs: I) -> Vec<(String, String)>
where
    I: Iterator<Item = (&'a std::ffi::OsStr, Option<&'a std::ffi::OsStr>)>,
{
    envs.filter_map(|(key, value)| {
        Some((
            key.to_string_lossy().into_owned(),
            value?.to_string_lossy().into_owned(),
        ))
    })
    .collect()
}

fn env_value<'a>(envs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    envs.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn jobserver_client_env() -> Vec<(String, String)> {
    vec![
        ("PATH".to_string(), "/usr/bin".to_string()),
        (
            "MAKEFLAGS".to_string(),
            "-j --jobserver-auth=8,9".to_string(),
        ),
        (
            "CARGO_MAKEFLAGS".to_string(),
            "-j --jobserver-fds=8,9 --jobserver-auth=8,9".to_string(),
        ),
        (
            "CARGO_MANIFEST_DIR".to_string(),
            "/tmp/workspace".to_string(),
        ),
    ]
}

fn test_lineage() -> super::super::super::lineage::Lineage {
    super::super::super::lineage::Lineage {
        daemon_pid: 100,
        client_pid: Some(50),
        session_id: Some("test-session".to_string()),
    }
}

#[test]
fn apply_client_env_filters_stale_jobserver_vars_for_compiler_spawns() {
    let env = jobserver_client_env();
    let mut cmd = tokio::process::Command::new("env");
    apply_client_env(&mut cmd, &Some(env), &test_lineage());

    let envs = collect_command_env(cmd.as_std().get_envs());
    assert_eq!(env_value(&envs, "PATH"), Some("/usr/bin"));
    assert_eq!(
        env_value(&envs, "CARGO_MANIFEST_DIR"),
        Some("/tmp/workspace")
    );
    assert_eq!(env_value(&envs, "MAKEFLAGS"), None);
    assert_eq!(env_value(&envs, "CARGO_MAKEFLAGS"), None);
    assert_eq!(
        env_value(&envs, super::super::super::lineage::ENV_DAEMON_PID),
        Some("100")
    );
}

#[test]
fn apply_client_env_sync_filters_stale_jobserver_vars_for_tool_spawns() {
    let env = jobserver_client_env();
    let mut cmd = std::process::Command::new("env");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage());

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(env_value(&envs, "PATH"), Some("/usr/bin"));
    assert_eq!(
        env_value(&envs, "CARGO_MANIFEST_DIR"),
        Some("/tmp/workspace")
    );
    assert_eq!(env_value(&envs, "MAKEFLAGS"), None);
    assert_eq!(env_value(&envs, "CARGO_MAKEFLAGS"), None);
    assert_eq!(
        env_value(&envs, super::super::super::lineage::ENV_DAEMON_PID),
        Some("100")
    );
}
