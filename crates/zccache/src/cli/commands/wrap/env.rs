//! Wrapper environment and strict-path option handling.

use crate::compiler::strict_paths::StrictPathsMode;

pub(crate) fn strip_leading_strict_paths_flags(
    args: &[String],
) -> Result<(Option<StrictPathsMode>, Vec<String>), String> {
    let mut strict_paths = None;
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if arg == "--strict-paths" {
            strict_paths = Some(StrictPathsMode::Absolute);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--strict-paths=") {
            strict_paths = Some(StrictPathsMode::parse(value).map_err(|err| err.to_string())?);
            index += 1;
        } else {
            break;
        }
    }

    Ok((strict_paths, args[index..].to_vec()))
}

pub(crate) fn parse_optional_strict_paths(
    value: Option<&str>,
) -> Result<Option<StrictPathsMode>, String> {
    value
        .map(|value| StrictPathsMode::parse(value).map_err(|err| err.to_string()))
        .transpose()
}

pub(super) fn effective_strict_paths_mode(
    strict_paths_override: Option<StrictPathsMode>,
) -> Result<StrictPathsMode, String> {
    if let Some(mode) = strict_paths_override {
        return Ok(mode);
    }

    match std::env::var("ZCCACHE_STRICT_PATHS") {
        Ok(value) => StrictPathsMode::parse(&value).map_err(|err| err.to_string()),
        Err(std::env::VarError::NotPresent) => Ok(StrictPathsMode::Off),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("ZCCACHE_STRICT_PATHS is not valid Unicode".to_string())
        }
    }
}

pub(super) fn client_env(strict_paths_override: Option<StrictPathsMode>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    if let Some(mode) = strict_paths_override {
        set_client_env(&mut env, "ZCCACHE_STRICT_PATHS", mode.as_str().to_string());
    }
    env
}

pub(super) fn wrapper_disabled() -> bool {
    std::env::var("ZCCACHE_DISABLE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
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
    fn strip_leading_strict_paths_flags_consumes_only_prefix() {
        let args = vec![
            "--strict-paths=consistent".to_string(),
            "rustc".to_string(),
            "--strict-paths=absolute".to_string(),
        ];

        let (mode, rest) = strip_leading_strict_paths_flags(&args).unwrap();

        assert_eq!(mode, Some(StrictPathsMode::Consistent));
        assert_eq!(rest, vec!["rustc", "--strict-paths=absolute"]);
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
}
