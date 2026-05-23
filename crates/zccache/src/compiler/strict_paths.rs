//! Strict compiler path flag validation.
//!
//! This catches include/force-include path spellings that can make compilers
//! see one physical header through multiple raw path strings.

use std::fmt;

/// Validation policy for compiler path flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictPathsMode {
    /// Do not validate path spellings.
    Off,
    /// Require all checked path flags to use one separator style.
    Consistent,
    /// Require forward-slash absolute paths with no `.` or `..` components.
    Absolute,
}

impl StrictPathsMode {
    /// Parse a `--strict-paths` / `ZCCACHE_STRICT_PATHS` value.
    pub fn parse(value: &str) -> Result<Self, StrictPathsParseError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Self::Absolute),
            "0" | "false" | "no" | "off" => Ok(Self::Off),
            "consistent" => Ok(Self::Consistent),
            "absolute" | "strict" => Ok(Self::Absolute),
            other => Err(StrictPathsParseError {
                value: other.to_string(),
            }),
        }
    }

    /// Read a mode from an environment-like key/value list.
    pub fn from_env_vars<'a>(
        vars: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, StrictPathsParseError> {
        for (key, value) in vars {
            if key == "ZCCACHE_STRICT_PATHS" {
                return Self::parse(value);
            }
        }
        Ok(Self::Off)
    }

    /// Canonical command-line spelling for diagnostics and env forwarding.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Consistent => "consistent",
            Self::Absolute => "absolute",
        }
    }
}

/// Invalid strict-path mode value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictPathsParseError {
    value: String,
}

impl fmt::Display for StrictPathsParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid strict paths mode '{}'; expected off, consistent, or absolute",
            self.value
        )
    }
}

impl std::error::Error for StrictPathsParseError {}

/// A strict-path validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictPathsViolation {
    /// Mode that rejected the flag.
    pub mode: StrictPathsMode,
    /// Offending compiler flag as it appeared in argv.
    pub flag: String,
    /// Path value extracted from the flag.
    pub path: String,
    reason: String,
}

impl StrictPathsViolation {
    /// Render a user-facing diagnostic with the wrapped compiler invocation.
    #[must_use]
    pub fn diagnostic(&self, compiler: &str, args: &[String]) -> String {
        let mut caller_parts = Vec::with_capacity(args.len() + 1);
        caller_parts.push(shell_quote(compiler));
        caller_parts.extend(args.iter().map(|arg| shell_quote(arg)));
        format!(
            "zccache: {} flag `{}` violates --strict-paths={} ({}).\n         Caller: {}",
            self.flag_name(),
            self.flag,
            self.mode.as_str(),
            self.reason,
            caller_parts.join(" ")
        )
    }

    fn flag_name(&self) -> &str {
        const FLAG_NAMES: &[&str] = &[
            "-include-pch",
            "-include",
            "-idirafter",
            "-iframework",
            "-isystem",
            "-iquote",
            "-imacros",
            "-imsvc",
            "-I",
            "-F",
            "/I",
        ];
        FLAG_NAMES
            .iter()
            .copied()
            .find(|name| self.flag == *name || self.flag.starts_with(&format!("{name} ")))
            .or_else(|| {
                FLAG_NAMES
                    .iter()
                    .copied()
                    .find(|name| self.flag.starts_with(*name))
            })
            .unwrap_or(self.flag.as_str())
    }
}

/// Validate path-bearing compiler flags in `args`.
pub fn validate_args(args: &[String], mode: StrictPathsMode) -> Result<(), StrictPathsViolation> {
    if mode == StrictPathsMode::Off {
        return Ok(());
    }

    let mut style: Option<SeparatorStyle> = None;
    for flag in collect_path_flags(args) {
        match mode {
            StrictPathsMode::Off => {}
            StrictPathsMode::Consistent => validate_consistent(&flag, &mut style)?,
            StrictPathsMode::Absolute => validate_absolute(&flag)?,
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeparatorStyle {
    Forward,
    Backslash,
}

#[derive(Debug)]
struct PathFlag {
    flag: String,
    path: String,
}

fn validate_consistent(
    flag: &PathFlag,
    style: &mut Option<SeparatorStyle>,
) -> Result<(), StrictPathsViolation> {
    let has_forward = flag.path.contains('/');
    let has_backslash = flag.path.contains('\\');

    if has_forward && has_backslash {
        return Err(StrictPathsViolation {
            mode: StrictPathsMode::Consistent,
            flag: flag.flag.clone(),
            path: flag.path.clone(),
            reason: "expected one separator style per path".to_string(),
        });
    }

    let Some(current) = separator_style(&flag.path) else {
        return Ok(());
    };
    match *style {
        Some(expected) if expected != current => Err(StrictPathsViolation {
            mode: StrictPathsMode::Consistent,
            flag: flag.flag.clone(),
            path: flag.path.clone(),
            reason: "expected all checked paths to use the same separator style".to_string(),
        }),
        Some(_) => Ok(()),
        None => {
            *style = Some(current);
            Ok(())
        }
    }
}

fn validate_absolute(flag: &PathFlag) -> Result<(), StrictPathsViolation> {
    let reason = if flag.path.contains('\\') || !is_forward_absolute(&flag.path) {
        Some("expected forward-slash absolute path")
    } else if has_dot_component(&flag.path) {
        Some("expected normalized path without /./ or /../ components")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(StrictPathsViolation {
            mode: StrictPathsMode::Absolute,
            flag: flag.flag.clone(),
            path: flag.path.clone(),
            reason: reason.to_string(),
        }),
        None => Ok(()),
    }
}

fn separator_style(path: &str) -> Option<SeparatorStyle> {
    if path.contains('/') {
        Some(SeparatorStyle::Forward)
    } else if path.contains('\\') {
        Some(SeparatorStyle::Backslash)
    } else {
        None
    }
}

fn is_forward_absolute(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }

    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn has_dot_component(path: &str) -> bool {
    path.split('/')
        .any(|component| component == "." || component == "..")
}

fn collect_path_flags(args: &[String]) -> Vec<PathFlag> {
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if matches!(
            arg.as_str(),
            "-I" | "-F"
                | "-isystem"
                | "-iquote"
                | "-idirafter"
                | "-iframework"
                | "-imsvc"
                | "-include"
                | "-include-pch"
                | "-imacros"
                | "/I"
        ) {
            if let Some(path) = args.get(i + 1) {
                flags.push(PathFlag {
                    flag: format!("{arg} {path}"),
                    path: path.clone(),
                });
                i += 2;
                continue;
            }
        }

        if let Some(path) = arg.strip_prefix("-I").filter(|path| !path.is_empty()) {
            flags.push(PathFlag {
                flag: arg.clone(),
                path: path.to_string(),
            });
        } else if let Some(path) = arg.strip_prefix("-F").filter(|path| !path.is_empty()) {
            flags.push(PathFlag {
                flag: arg.clone(),
                path: path.to_string(),
            });
        } else if let Some(path) = arg
            .strip_prefix("/I")
            .filter(|path| !path.is_empty() && !path.starts_with(':'))
        {
            flags.push(PathFlag {
                flag: arg.clone(),
                path: path.to_string(),
            });
        } else if let Some(path) = joined_path_flag(arg) {
            flags.push(PathFlag {
                flag: arg.clone(),
                path,
            });
        }

        i += 1;
    }
    flags
}

fn joined_path_flag(arg: &str) -> Option<String> {
    const PREFIXES: &[&str] = &[
        "-isystem",
        "-iquote",
        "-idirafter",
        "-iframework",
        "-imsvc",
        "/imsvc",
    ];

    for prefix in PREFIXES {
        if let Some(path) = arg.strip_prefix(prefix).filter(|path| !path.is_empty()) {
            return Some(path.strip_prefix('=').unwrap_or(path).to_string());
        }
    }
    None
}

fn shell_quote(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '$'))
    {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn absolute_rejects_relative_include() {
        let err = validate_args(
            &args(&["-c", "foo.cpp", "-Iinclude"]),
            StrictPathsMode::Absolute,
        )
        .unwrap_err();
        assert_eq!(err.flag, "-Iinclude");
        assert!(err.reason.contains("absolute"));
    }

    #[test]
    fn absolute_rejects_backslash_include() {
        let err = validate_args(
            &args(&["-c", "foo.cpp", r"-IC:\work\project\include"]),
            StrictPathsMode::Absolute,
        )
        .unwrap_err();
        assert_eq!(err.flag, r"-IC:\work\project\include");
    }

    #[test]
    fn absolute_rejects_dot_component() {
        let err = validate_args(
            &args(&["-c", "foo.cpp", "-IC:/work/project/./include"]),
            StrictPathsMode::Absolute,
        )
        .unwrap_err();
        assert_eq!(err.path, "C:/work/project/./include");
        assert!(err.reason.contains("/./"));
    }

    #[test]
    fn absolute_accepts_forward_windows_and_unix_paths() {
        validate_args(
            &args(&[
                "-IC:/work/project/include",
                "-isystem",
                "/opt/sdk/include",
                "-include-pch",
                "C:/work/project/pch.h.pch",
            ]),
            StrictPathsMode::Absolute,
        )
        .unwrap();
    }

    #[test]
    fn consistent_rejects_mixed_separators_within_one_path() {
        let err = validate_args(
            &args(&["-c", "foo.cpp", r"-Ici/meson/native\fastled.dll.p"]),
            StrictPathsMode::Consistent,
        )
        .unwrap_err();
        assert!(err.reason.contains("one separator style"));
    }

    #[test]
    fn consistent_rejects_different_styles_across_flags() {
        let err = validate_args(
            &args(&["-IC:/work/project/include", r"-IC:\work\project\generated"]),
            StrictPathsMode::Consistent,
        )
        .unwrap_err();
        assert!(err.reason.contains("same separator style"));
    }

    #[test]
    fn validates_joined_path_flags() {
        for flag in [
            "-isysteminclude",
            "-isystem=include",
            "-iquoteinclude",
            "-idirafterinclude",
            "-iframeworkinclude",
            "-imsvcinclude",
        ] {
            let err = validate_args(&args(&["-c", "foo.cpp", flag]), StrictPathsMode::Absolute)
                .unwrap_err();
            assert_eq!(err.flag, flag);
            assert_eq!(err.path, "include");
        }
    }

    #[test]
    fn consistent_allows_styleless_relative_paths() {
        validate_args(
            &args(&["-Iinclude", "-isystem", "generated"]),
            StrictPathsMode::Consistent,
        )
        .unwrap();
    }

    #[test]
    fn diagnostic_includes_caller() {
        let err = validate_args(
            &args(&["-c", "foo.cpp", "-Irelative"]),
            StrictPathsMode::Absolute,
        )
        .unwrap_err();
        let diagnostic = err.diagnostic("clang++", &args(&["-c", "foo.cpp", "-Irelative"]));
        assert!(diagnostic.contains("violates --strict-paths=absolute"));
        assert!(diagnostic.contains("Caller: clang++ -c foo.cpp -Irelative"));
    }

    #[test]
    fn parse_env_aliases() {
        assert_eq!(
            StrictPathsMode::parse("1").unwrap(),
            StrictPathsMode::Absolute
        );
        assert_eq!(
            StrictPathsMode::parse("consistent").unwrap(),
            StrictPathsMode::Consistent
        );
        assert_eq!(StrictPathsMode::parse("off").unwrap(), StrictPathsMode::Off);
        assert!(StrictPathsMode::parse("sometimes").is_err());
    }
}
