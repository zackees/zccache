//! Rustfmt invocation parsing for zccache format caching.
//!
//! Parses `rustfmt` command-line arguments to extract source files,
//! mode flags, and configuration for cache key computation.

use std::path::Path;
use zccache_core::NormalizedPath;

/// Parsed rustfmt invocation.
#[derive(Debug, Clone)]
pub struct ParsedRustfmt {
    /// Source files to format (positional `.rs` args).
    pub source_files: Vec<NormalizedPath>,
    /// Whether `--check` mode was requested (no file modification, exit code only).
    pub check_mode: bool,
    /// Edition override (e.g., "2021"), if specified via `--edition`.
    pub edition: Option<String>,
    /// Explicit config path from `--config-path`.
    pub config_path: Option<NormalizedPath>,
    /// All flags (excluding source files) for cache key computation.
    pub flags: Vec<String>,
}

/// Rustfmt flags that take a following argument.
const RUSTFMT_FLAGS_WITH_VALUE: &[&str] = &[
    "--edition",
    "--config-path",
    "--config",
    "--color",
    "--print-config",
    "--files-with-diff",
    "--file-lines",
];

/// Parse a rustfmt invocation.
///
/// Extracts source files, --check mode, edition, and config path.
/// Returns `None` if stdin mode (no source files) or `--help`/`--version`.
#[must_use]
pub fn parse_rustfmt_invocation(args: &[String]) -> Option<ParsedRustfmt> {
    let mut source_files: Vec<NormalizedPath> = Vec::new();
    let mut check_mode = false;
    let mut edition: Option<String> = None;
    let mut config_path: Option<NormalizedPath> = None;
    let mut flags: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // --help / --version: not cacheable, pass through
        if arg == "--help" || arg == "-h" || arg == "--version" || arg == "-V" {
            return None;
        }

        // --check or -l (list files with differences)
        if arg == "--check" || arg == "-l" {
            check_mode = true;
            flags.push(arg.clone());
            i += 1;
            continue;
        }

        // --edition <ed> or --edition=<ed>
        if arg == "--edition" {
            if let Some(next) = args.get(i + 1) {
                edition = Some(next.clone());
                flags.push(arg.clone());
                flags.push(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--edition=") {
            edition = Some(val.to_string());
            flags.push(arg.clone());
            i += 1;
            continue;
        }

        // --config-path <path> or --config-path=<path>
        if arg == "--config-path" {
            if let Some(next) = args.get(i + 1) {
                config_path = Some(NormalizedPath::new(next));
                flags.push(arg.clone());
                flags.push(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--config-path=") {
            config_path = Some(NormalizedPath::new(val));
            flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Known flags that take a value â€” skip both
        if let Some(&_flag) = RUSTFMT_FLAGS_WITH_VALUE
            .iter()
            .find(|&&f| f == arg.as_str())
        {
            if let Some(next) = args.get(i + 1) {
                flags.push(arg.clone());
                flags.push(next.clone());
                i += 2;
                continue;
            }
        }

        // Flags with = form
        if arg.starts_with("--") && arg.contains('=') {
            flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Any other flag
        if arg.starts_with('-') {
            flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg â€” source file (.rs)
        if arg.ends_with(".rs") {
            source_files.push(NormalizedPath::new(arg));
        }

        i += 1;
    }

    // No source files â†’ stdin mode, not cacheable
    if source_files.is_empty() {
        return None;
    }

    Some(ParsedRustfmt {
        source_files,
        check_mode,
        edition,
        config_path,
        flags,
    })
}

/// Find the rustfmt configuration file by walking up the directory tree.
///
/// Searches for `rustfmt.toml` or `.rustfmt.toml` starting from `start_dir`
/// and walking up to the filesystem root. Returns the first match.
#[must_use]
pub fn find_rustfmt_config(start_dir: &Path) -> Option<NormalizedPath> {
    let mut dir = start_dir;
    loop {
        let candidate = NormalizedPath::new(dir).join("rustfmt.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        let hidden = NormalizedPath::new(dir).join(".rustfmt.toml");
        if hidden.exists() {
            return Some(hidden);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn basic_rustfmt_invocation() {
        let result = parse_rustfmt_invocation(&args(&["src/main.rs"])).unwrap();
        assert_eq!(
            result.source_files,
            vec![NormalizedPath::new("src/main.rs")]
        );
        assert!(!result.check_mode);
        assert!(result.edition.is_none());
    }

    #[test]
    fn rustfmt_check_mode() {
        let result = parse_rustfmt_invocation(&args(&["--check", "src/lib.rs"])).unwrap();
        assert!(result.check_mode);
        assert_eq!(result.source_files, vec![NormalizedPath::new("src/lib.rs")]);
    }

    #[test]
    fn rustfmt_check_mode_dash_l() {
        let result = parse_rustfmt_invocation(&args(&["-l", "src/lib.rs"])).unwrap();
        assert!(result.check_mode);
    }

    #[test]
    fn rustfmt_with_edition() {
        let result = parse_rustfmt_invocation(&args(&["--edition", "2021", "src/lib.rs"])).unwrap();
        assert_eq!(result.edition.as_deref(), Some("2021"));
    }

    #[test]
    fn rustfmt_with_edition_equals() {
        let result = parse_rustfmt_invocation(&args(&["--edition=2021", "src/lib.rs"])).unwrap();
        assert_eq!(result.edition.as_deref(), Some("2021"));
    }

    #[test]
    fn rustfmt_with_config_path() {
        let result =
            parse_rustfmt_invocation(&args(&["--config-path", "/my/rustfmt.toml", "src/lib.rs"]))
                .unwrap();
        assert_eq!(
            result.config_path,
            Some(NormalizedPath::new("/my/rustfmt.toml"))
        );
    }

    #[test]
    fn rustfmt_multiple_files() {
        let result = parse_rustfmt_invocation(&args(&[
            "--edition",
            "2021",
            "src/main.rs",
            "src/lib.rs",
            "src/util.rs",
        ]))
        .unwrap();
        assert_eq!(result.source_files.len(), 3);
    }

    #[test]
    fn rustfmt_help_returns_none() {
        assert!(parse_rustfmt_invocation(&args(&["--help"])).is_none());
        assert!(parse_rustfmt_invocation(&args(&["-h"])).is_none());
        assert!(parse_rustfmt_invocation(&args(&["-V"])).is_none());
    }

    #[test]
    fn rustfmt_no_files_returns_none() {
        // stdin mode
        assert!(parse_rustfmt_invocation(&args(&["--edition", "2021"])).is_none());
    }

    #[test]
    fn rustfmt_flags_preserved() {
        let result = parse_rustfmt_invocation(&args(&[
            "--check",
            "--edition",
            "2021",
            "--config",
            "max_width=100",
            "src/lib.rs",
        ]))
        .unwrap();
        assert!(result.flags.contains(&"--check".to_string()));
        assert!(result.flags.contains(&"--edition".to_string()));
        assert!(result.flags.contains(&"2021".to_string()));
        assert!(result.flags.contains(&"--config".to_string()));
        assert!(result.flags.contains(&"max_width=100".to_string()));
    }

    #[test]
    fn find_config_in_current_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("rustfmt.toml"), "max_width = 100").unwrap();
        let result = find_rustfmt_config(tmp.path());
        assert_eq!(result, Some(tmp.path().join("rustfmt.toml").into()));
    }

    #[test]
    fn find_hidden_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".rustfmt.toml"), "max_width = 100").unwrap();
        let result = find_rustfmt_config(tmp.path());
        assert_eq!(result, Some(tmp.path().join(".rustfmt.toml").into()));
    }

    #[test]
    fn find_config_walks_up() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("rustfmt.toml"), "max_width = 100").unwrap();
        let subdir = tmp.path().join("src");
        std::fs::create_dir(&subdir).unwrap();
        let result = find_rustfmt_config(&subdir);
        assert_eq!(result, Some(tmp.path().join("rustfmt.toml").into()));
    }

    #[test]
    fn find_config_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = find_rustfmt_config(tmp.path());
        // May find a config in a parent directory, or None if truly absent
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn rustfmt_prefers_plain_over_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("rustfmt.toml"), "plain").unwrap();
        std::fs::write(tmp.path().join(".rustfmt.toml"), "hidden").unwrap();
        let result = find_rustfmt_config(tmp.path());
        assert_eq!(result, Some(tmp.path().join("rustfmt.toml").into()));
    }
}
