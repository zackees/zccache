//! Tests for `apply_client_env_builder` / `apply_client_env_sync` — verify that
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

/// Temporarily set the daemon's Nix dynamic-library path. The cache-dir test
/// guard is the daemon crate's shared process-environment lock, so this cannot
/// race another test that reads or mutates daemon environment variables.
#[cfg(target_os = "linux")]
struct NixLdLibraryPathEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous_nix_ld_library_path: Option<std::ffi::OsString>,
    previous_ld_library_path: Option<std::ffi::OsString>,
}

#[cfg(target_os = "linux")]
impl NixLdLibraryPathEnvGuard {
    fn set(value: &str) -> Self {
        Self::set_with_daemon_ld_library_path(value, None)
    }

    fn set_with_daemon_ld_library_path(value: &str, ld_library_path: Option<&str>) -> Self {
        let lock = super::CacheDirEnvGuard::lock();
        let previous_nix_ld_library_path = std::env::var_os("NIX_LD_LIBRARY_PATH");
        let previous_ld_library_path = std::env::var_os("LD_LIBRARY_PATH");
        std::env::set_var("NIX_LD_LIBRARY_PATH", value);
        match ld_library_path {
            Some(ld_library_path) => std::env::set_var("LD_LIBRARY_PATH", ld_library_path),
            None => std::env::remove_var("LD_LIBRARY_PATH"),
        }
        Self {
            _lock: lock,
            previous_nix_ld_library_path,
            previous_ld_library_path,
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for NixLdLibraryPathEnvGuard {
    fn drop(&mut self) {
        match &self.previous_nix_ld_library_path {
            Some(previous) => std::env::set_var("NIX_LD_LIBRARY_PATH", previous),
            None => std::env::remove_var("NIX_LD_LIBRARY_PATH"),
        }
        match &self.previous_ld_library_path {
            Some(previous) => std::env::set_var("LD_LIBRARY_PATH", previous),
            None => std::env::remove_var("LD_LIBRARY_PATH"),
        }
    }
}

#[tokio::test]
async fn apply_client_env_filters_stale_jobserver_vars_for_compiler_spawns() {
    let env = jobserver_client_env();
    #[cfg(unix)]
    let builder = kernal_api::SpawnSpec::new("/usr/bin/env");
    #[cfg(windows)]
    let builder = kernal_api::SpawnSpec::new(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
            .join("System32")
            .join("cmd.exe"),
    )
    .args(["/D", "/C", "set"]);
    let builder = apply_client_env_builder(builder, &Some(env), &test_lineage(), false);
    let output = crate::daemon::process::async_builder_output_with_priority_timeout(
        builder,
        CompilePriority::Normal,
        std::time::Duration::from_secs(10),
        "client environment fixture".to_string(),
    )
    .await
    .expect("canonical environment fixture must run");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("fixture environment is UTF-8");
    let envs: Vec<(String, String)> = text
        .lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
        })
        .collect();
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
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage(), false);

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

#[test]
fn internal_dylint_cache_salt_is_never_replayed() {
    let env = vec![
        (
            crate::compiler::DYLINT_CACHE_INPUT_HASH_ENV.to_string(),
            "internal-only".to_string(),
        ),
        ("DYLINT_METADATA".to_string(), "user-value".to_string()),
    ];
    let mut cmd = std::process::Command::new("env");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage(), false);

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(
        env_value(&envs, crate::compiler::DYLINT_CACHE_INPUT_HASH_ENV),
        None
    );
    assert_eq!(env_value(&envs, "DYLINT_METADATA"), Some("user-value"));
}

#[cfg(target_os = "linux")]
#[test]
fn dylint_driver_appends_daemon_nix_library_path_after_client_path() {
    let _nix_library_path = NixLdLibraryPathEnvGuard::set("/nix/store/daemon-lib");
    let env = vec![("LD_LIBRARY_PATH".to_string(), "/client/lib".to_string())];
    let mut cmd = std::process::Command::new("dylint-driver");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage(), true);

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(
        env_value(&envs, "LD_LIBRARY_PATH"),
        Some("/client/lib:/nix/store/daemon-lib"),
        "the client path must take precedence over daemon-provided Nix libraries"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dylint_driver_uses_daemon_nix_library_path_without_client_path() {
    let _nix_library_path = NixLdLibraryPathEnvGuard::set("/nix/store/daemon-lib");
    let env = vec![("PATH".to_string(), "/client/bin".to_string())];
    let mut cmd = std::process::Command::new("dylint-driver");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage(), true);

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(
        env_value(&envs, "LD_LIBRARY_PATH"),
        Some("/nix/store/daemon-lib")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dylint_driver_appends_nix_path_after_inherited_daemon_library_path() {
    let _nix_library_path = NixLdLibraryPathEnvGuard::set_with_daemon_ld_library_path(
        "/nix/store/daemon-lib",
        Some("/daemon/lib"),
    );
    let mut cmd = std::process::Command::new("dylint-driver");
    apply_client_env_sync(&mut cmd, None, &test_lineage(), true);

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(
        env_value(&envs, "LD_LIBRARY_PATH"),
        Some("/daemon/lib:/nix/store/daemon-lib"),
        "the inherited daemon path must remain ahead of its Nix loader path"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_nix_library_path_is_not_applied_to_non_dylint_compilers() {
    let _nix_library_path = NixLdLibraryPathEnvGuard::set("/nix/store/daemon-lib");
    let env = vec![("LD_LIBRARY_PATH".to_string(), "/client/lib".to_string())];
    let mut cmd = std::process::Command::new("rustc");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage(), false);

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(env_value(&envs, "LD_LIBRARY_PATH"), Some("/client/lib"));
}

#[cfg(target_os = "linux")]
#[test]
fn empty_daemon_nix_library_path_does_not_change_dylint_environment() {
    let _nix_library_path = NixLdLibraryPathEnvGuard::set("");
    let env = vec![("LD_LIBRARY_PATH".to_string(), "/client/lib".to_string())];
    let mut cmd = std::process::Command::new("dylint-driver");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage(), true);

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(env_value(&envs, "LD_LIBRARY_PATH"), Some("/client/lib"));
}
