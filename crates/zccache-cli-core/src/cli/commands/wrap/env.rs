//! Wrapper environment and strict-path option handling.

use crate::compiler::strict_paths::StrictPathsMode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WrapperOverrides {
    pub(crate) strict_paths: Option<StrictPathsMode>,
    pub(crate) fast: bool,
    pub(crate) scan_system_headers: Option<bool>,
}

impl WrapperOverrides {
    pub(crate) fn overlay(self, base: Self) -> Self {
        Self {
            strict_paths: self.strict_paths.or(base.strict_paths),
            fast: self.fast || base.fast,
            scan_system_headers: self.scan_system_headers.or(base.scan_system_headers),
        }
    }
}

pub(crate) fn strip_leading_wrapper_flags(
    args: &[String],
) -> Result<(WrapperOverrides, Vec<String>), String> {
    let mut overrides = WrapperOverrides::default();
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if arg == "--strict-paths" {
            overrides.strict_paths = Some(StrictPathsMode::Absolute);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--strict-paths=") {
            overrides.strict_paths =
                Some(StrictPathsMode::parse(value).map_err(|err| err.to_string())?);
            index += 1;
        } else if arg == "--fast" {
            overrides.fast = true;
            index += 1;
        } else if arg == "--scan-system-headers" {
            set_scan_override(&mut overrides, true)?;
            index += 1;
        } else if arg == "--skip-system-headers" {
            set_scan_override(&mut overrides, false)?;
            index += 1;
        } else {
            break;
        }
    }

    Ok((overrides, args[index..].to_vec()))
}

pub(crate) fn parse_wrapper_overrides(
    strict_paths: Option<&str>,
    fast: bool,
    scan_system_headers: bool,
    skip_system_headers: bool,
) -> Result<WrapperOverrides, String> {
    let mut overrides = WrapperOverrides {
        strict_paths: parse_optional_strict_paths(strict_paths)?,
        fast,
        scan_system_headers: None,
    };
    if scan_system_headers {
        set_scan_override(&mut overrides, true)?;
    }
    if skip_system_headers {
        set_scan_override(&mut overrides, false)?;
    }
    Ok(overrides)
}

fn set_scan_override(overrides: &mut WrapperOverrides, value: bool) -> Result<(), String> {
    if overrides
        .scan_system_headers
        .is_some_and(|current| current != value)
    {
        return Err(
            "--scan-system-headers conflicts with --skip-system-headers; choose one".to_string(),
        );
    }
    overrides.scan_system_headers = Some(value);
    Ok(())
}

pub(crate) fn parse_optional_strict_paths(
    value: Option<&str>,
) -> Result<Option<StrictPathsMode>, String> {
    value
        .map(|value| StrictPathsMode::parse(value).map_err(|err| err.to_string()))
        .transpose()
}

pub(super) fn effective_strict_paths_mode(
    overrides: WrapperOverrides,
) -> Result<StrictPathsMode, String> {
    if let Some(mode) = overrides.strict_paths {
        return Ok(mode);
    }

    match std::env::var("ZCCACHE_STRICT_PATHS") {
        Ok(value) => StrictPathsMode::parse(&value).map_err(|err| err.to_string()),
        Err(std::env::VarError::NotPresent) => Ok(windows_pch_guard_default()),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("ZCCACHE_STRICT_PATHS is not valid Unicode".to_string())
        }
    }
}

/// Issue #619: Windows-only opt-in. When `ZCCACHE_WINDOWS_PCH_GUARD=1` and
/// `ZCCACHE_STRICT_PATHS` is unset, default to `Consistent` mode so the
/// mixed-separator `-I` / `-include` patterns that defeat clang's
/// `#pragma once` dedup across PCH boundaries get rejected at the
/// compile-command level. Off-by-default on Windows for backward compat;
/// the env var is the safe opt-in path until consistent-on-Windows
/// proves out in the field.
fn windows_pch_guard_default() -> StrictPathsMode {
    let guard_value = std::env::var("ZCCACHE_WINDOWS_PCH_GUARD").ok();
    windows_pch_guard_default_for(crate::platform::host::is_windows(), guard_value.as_deref())
}

/// Pure helper for `windows_pch_guard_default` — separated so unit tests
/// don't have to mutate process env (which races under cargo's
/// parallel-test default).
fn windows_pch_guard_default_for(is_windows: bool, guard_value: Option<&str>) -> StrictPathsMode {
    if is_windows && matches!(guard_value, Some("1" | "true" | "yes" | "on")) {
        StrictPathsMode::Consistent
    } else {
        StrictPathsMode::Off
    }
}

pub(super) fn client_env(overrides: WrapperOverrides) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    if let Some(mode) = overrides.strict_paths {
        set_client_env(&mut env, "ZCCACHE_STRICT_PATHS", mode.as_str().to_string());
    }
    if overrides.fast {
        set_client_env(&mut env, "ZCCACHE_FAST", "1".to_string());
    }
    if let Some(scan) = overrides.scan_system_headers {
        set_client_env(
            &mut env,
            "ZCCACHE_SCAN_SYSTEM_HEADERS",
            if scan { "1" } else { "0" }.to_string(),
        );
    }
    env
}

pub(super) fn wrapper_disabled() -> bool {
    crate::core::config::owned_env_flag_enabled("ZCCACHE_DISABLE")
}

fn set_client_env(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = env.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value;
    } else {
        env.push((key.to_string(), value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_leading_wrapper_flags_consumes_only_prefix() {
        let args = vec![
            "--strict-paths=consistent".to_string(),
            "rustc".to_string(),
            "--strict-paths=absolute".to_string(),
        ];

        let (overrides, rest) = strip_leading_wrapper_flags(&args).unwrap();

        assert_eq!(overrides.strict_paths, Some(StrictPathsMode::Consistent));
        assert_eq!(rest, vec!["rustc", "--strict-paths=absolute"]);
    }

    #[test]
    fn fast_and_header_policy_flags_are_consumed_before_compiler() {
        let args = vec![
            "--fast".to_string(),
            "--skip-system-headers".to_string(),
            "clang".to_string(),
            "--fast".to_string(),
        ];
        let (overrides, rest) = strip_leading_wrapper_flags(&args).unwrap();
        assert!(overrides.fast);
        assert_eq!(overrides.scan_system_headers, Some(false));
        assert_eq!(rest, vec!["clang", "--fast"]);
    }

    #[test]
    fn explicit_scan_setting_overrides_fast_preset() {
        let overrides = parse_wrapper_overrides(None, true, true, false).unwrap();
        assert!(overrides.fast);
        assert_eq!(overrides.scan_system_headers, Some(true));
    }

    #[test]
    fn contradictory_scan_flags_are_rejected() {
        let error = parse_wrapper_overrides(None, false, true, true).unwrap_err();
        assert!(error.contains("conflicts"));
    }

    #[test]
    fn client_env_overrides_existing_strict_paths() {
        let mut env = vec![("ZCCACHE_STRICT_PATHS".to_string(), "off".to_string())];

        set_client_env(
            &mut env,
            "ZCCACHE_STRICT_PATHS",
            StrictPathsMode::Absolute.as_str().to_string(),
        );

        assert_eq!(
            env.iter()
                .find(|(key, _)| key == "ZCCACHE_STRICT_PATHS")
                .map(|(_, value)| value.as_str()),
            Some("absolute")
        );
    }

    #[test]
    fn windows_pch_guard_default_off_when_not_windows() {
        // Linux/macOS never auto-enable the guard, regardless of env value.
        assert_eq!(
            windows_pch_guard_default_for(false, Some("1")),
            StrictPathsMode::Off
        );
        assert_eq!(
            windows_pch_guard_default_for(false, None),
            StrictPathsMode::Off
        );
    }

    #[test]
    fn windows_pch_guard_default_off_when_env_unset_or_falsy() {
        assert_eq!(
            windows_pch_guard_default_for(true, None),
            StrictPathsMode::Off
        );
        assert_eq!(
            windows_pch_guard_default_for(true, Some("0")),
            StrictPathsMode::Off
        );
        assert_eq!(
            windows_pch_guard_default_for(true, Some("")),
            StrictPathsMode::Off
        );
        assert_eq!(
            windows_pch_guard_default_for(true, Some("garbage")),
            StrictPathsMode::Off
        );
    }

    #[test]
    fn windows_pch_guard_default_consistent_when_windows_and_opt_in() {
        for truthy in ["1", "true", "yes", "on"] {
            assert_eq!(
                windows_pch_guard_default_for(true, Some(truthy)),
                StrictPathsMode::Consistent,
                "ZCCACHE_WINDOWS_PCH_GUARD={truthy} on Windows should enable Consistent"
            );
        }
    }
}
