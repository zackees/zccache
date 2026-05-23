//! Parser for GNU make dependency files (`.d` files).
//!
//! GCC/Clang emit these with `-MD -MF`. The format is:
//!
//! ```text
//! target.o: source.c header1.h \
//!   /usr/include/stdio.h path\ with\ spaces/foo.h
//! ```
//!
//! The parser extracts dependency paths, resolves relative paths against
//! a working directory, excludes the source file itself, and deduplicates.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::args::UserDepFlags;
use super::scanner::ScanResult;
use crate::core::NormalizedPath;

/// Errors that can occur while parsing a `.d` file.
#[derive(Debug)]
pub enum DepfileError {
    /// I/O error reading the file.
    Io(std::io::Error),
    /// The depfile content is malformed (empty or missing colon separator).
    Malformed(String),
}

impl std::fmt::Display for DepfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DepfileError::Io(e) => write!(f, "depfile I/O error: {e}"),
            DepfileError::Malformed(msg) => write!(f, "malformed depfile: {msg}"),
        }
    }
}

impl std::error::Error for DepfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DepfileError::Io(e) => Some(e),
            DepfileError::Malformed(_) => None,
        }
    }
}

impl From<std::io::Error> for DepfileError {
    fn from(e: std::io::Error) -> Self {
        DepfileError::Io(e)
    }
}

/// Parse `.d` file content into a [`ScanResult`].
///
/// The source file is excluded from the resolved list. Relative paths are
/// resolved against `cwd`. Duplicate paths are collapsed (preserving order).
pub fn parse_depfile(content: &str, source: &Path, cwd: &Path) -> Result<ScanResult, DepfileError> {
    if content.trim().is_empty() {
        return Err(DepfileError::Malformed("empty depfile content".to_string()));
    }

    // Step 1: Join continuation lines (replace `\<newline>` with space).
    let joined = join_continuations(content);

    // Step 2: Find the colon separator (handling Windows drive letters).
    let colon_pos = find_separator_colon(&joined)?;

    // Step 3: Everything after the colon is the dependency list.
    let deps_str = &joined[colon_pos + 1..];

    // Step 4: Split on unescaped whitespace and unescape tokens.
    let tokens = split_and_unescape(deps_str);

    // Step 5-7: Resolve paths, filter source, deduplicate.
    let source_canonical = canonicalize_path(source, cwd);

    let mut seen = HashSet::new();
    let mut resolved = Vec::new();

    for token in tokens {
        if token.is_empty() {
            continue;
        }

        let dep_path = Path::new(&token);
        let abs_path = if dep_path.is_absolute() {
            canonicalize_path(dep_path, cwd)
        } else {
            canonicalize_path(&cwd.join(dep_path), cwd)
        };

        // Exclude the source file itself.
        if abs_path == source_canonical {
            continue;
        }

        // Deduplicate, preserving insertion order.
        if seen.insert(abs_path.clone()) {
            resolved.push(abs_path);
        }
    }

    Ok(ScanResult {
        resolved,
        unresolved: Vec::new(),
        has_computed: false,
    })
}

/// Read and parse a `.d` file from disk.
///
/// Reads the file at `path`, then delegates to [`parse_depfile`].
pub fn parse_depfile_path(
    path: &Path,
    source: &Path,
    cwd: &Path,
) -> Result<ScanResult, DepfileError> {
    let content = std::fs::read_to_string(path)?;
    parse_depfile(&content, source, cwd)
}

// â”€â”€ Depfile Strategy â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// How to obtain the depfile for a compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepfileStrategy {
    /// We injected `-MD -MF <path>` â€” read and clean up after compilation.
    Injected { path: NormalizedPath },
    /// User already had `-MF <path>` â€” read it (don't delete).
    UserSpecified { path: NormalizedPath },
    /// User had `-MD` but no `-MF` â€” derive path from output stem.
    UserDefault { path: NormalizedPath },
    /// MSVC `/showIncludes` â€” parse stderr after compilation.
    ShowIncludes,
    /// Compiler doesn't support depfiles â€” use fallback scanner.
    Unsupported,
}

/// Determine depfile strategy and return extra args to append to the compiler.
///
/// `supports_depfile`: whether the compiler family supports `-MD -MF`.
/// `dep_flags`: user's existing dependency flags.
/// `output_file`: the `-o` output file path (used to derive default `.d` path).
/// `tmpdir`: directory for injected depfiles.
///
/// Returns `(extra_args, strategy)`. `extra_args` is empty unless we inject flags.
pub fn prepare_depfile(
    supports_depfile: bool,
    dep_flags: &UserDepFlags,
    output_file: &Path,
    tmpdir: &Path,
) -> (Vec<String>, DepfileStrategy) {
    if !supports_depfile {
        return (Vec::new(), DepfileStrategy::Unsupported);
    }

    // User already specified -MF <path>: use their file.
    if let Some(ref mf_path) = dep_flags.mf_path {
        return (
            Vec::new(),
            DepfileStrategy::UserSpecified {
                path: mf_path.clone(),
            },
        );
    }

    // User has -MD/-MMD but no -MF: derive from output file stem.
    if dep_flags.has_md {
        let d_path = output_file.with_extension("d");
        return (
            Vec::new(),
            DepfileStrategy::UserDefault {
                path: d_path.into(),
            },
        );
    }

    // No user dep flags: inject -MD -MF <tmpfile>.
    // Re-create tmpdir if it was deleted (e.g. by Windows temp cleanup)
    // while the daemon is still running. Without this, the compiler fails
    // with "error opening ... no such file or directory".
    if !tmpdir.exists() {
        let _ = std::fs::create_dir_all(tmpdir);
    }
    static DEPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = DEPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = output_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("depfile");
    let tmp_path = tmpdir.join(format!("{stem}_{}_{unique}.d", std::process::id()));
    let tmp_path: NormalizedPath = tmp_path.into();
    let extra_args = vec![
        "-MD".to_string(),
        "-MF".to_string(),
        tmp_path.to_string_lossy().into_owned(),
    ];
    (extra_args, DepfileStrategy::Injected { path: tmp_path })
}

// â”€â”€ Internal helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Join backslash-continued lines: replace `\<newline>` sequences with a
/// single space so that the entire depfile becomes one logical line.
fn join_continuations(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('\n') => {
                    chars.next();
                    result.push(' ');
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    result.push(' ');
                }
                _ => {
                    result.push(ch);
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Find the position of the colon that separates target(s) from dependencies.
///
/// Handles Windows drive letters: if the character before `:` is a single
/// ASCII letter and the character after `:` is `\` or `/`, it is a drive
/// letter, not the target separator. Keep scanning.
fn find_separator_colon(line: &str) -> Result<usize, DepfileError> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b':' {
            // Check if this is a Windows drive letter (e.g., C:\...).
            // A drive letter colon has: single ASCII letter before it,
            // and `\` or `/` after it.
            let is_drive_letter = i > 0
                && (i == 1 || !bytes[i - 2].is_ascii_alphanumeric())
                && bytes[i - 1].is_ascii_alphabetic()
                && i + 1 < len
                && (bytes[i + 1] == b'\\' || bytes[i + 1] == b'/');

            if !is_drive_letter {
                return Ok(i);
            }
        }
        i += 1;
    }

    Err(DepfileError::Malformed(
        "no colon separator found".to_string(),
    ))
}

/// Split the dependency string on unescaped whitespace, then unescape each
/// token (`\ ` â†’ ` `, `\#` â†’ `#`).
fn split_and_unescape(deps: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = deps.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some(' ') => {
                    chars.next();
                    current.push(' ');
                }
                Some('#') => {
                    chars.next();
                    current.push('#');
                }
                _ => {
                    // Preserve other backslashes (e.g., Windows path separators).
                    current.push(ch);
                }
            }
        } else if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Canonicalize a path, falling back to the joined path if canonicalization
/// fails (e.g., the file does not exist on disk).
///
/// On Windows, `std::fs::canonicalize` produces `\\?\` extended-length paths.
/// These must be stripped so paths match the format used by the file watcher
/// (which also strips `\\?\`), ensuring journal/metadata lookups work correctly.
pub(crate) fn canonicalize_path(path: &Path, cwd: &Path) -> NormalizedPath {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    });
    strip_win_prefix(canonical.into())
}

/// Strip the `\\?\` extended-length prefix on Windows.
/// No-op on other platforms.
pub(crate) fn strip_win_prefix(path: NormalizedPath) -> NormalizedPath {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return NormalizedPath::from(stripped);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use crate::core::NormalizedPath;

    use super::*;
    use tempfile::TempDir;

    /// Helper: create a file with empty content inside a temp dir.
    fn touch(dir: &Path, name: &str) -> NormalizedPath {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, "").unwrap();
        p.into()
    }

    /// Helper: canonicalize a path (or return it unchanged), stripping \\?\ on Windows.
    fn canon(p: &Path) -> NormalizedPath {
        strip_win_prefix(
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .into(),
        )
    }

    // â”€â”€ 1. parse_single_line â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_single_line() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "foo.c");
        let bar_h = touch(cwd, "bar.h");

        let content = "foo.o: foo.c bar.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0], canon(&bar_h));
        assert!(result.unresolved.is_empty());
        assert!(!result.has_computed);
    }

    // â”€â”€ 2. parse_continuations â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_continuations() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "foo.c");
        let bar_h = touch(cwd, "bar.h");
        let baz_h = touch(cwd, "baz.h");

        let content = "foo.o: foo.c bar.h \\\n  baz.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 2);
        assert!(result.resolved.contains(&canon(&bar_h)));
        assert!(result.resolved.contains(&canon(&baz_h)));
    }

    // â”€â”€ 3. parse_escaped_spaces â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_escaped_spaces() {
        // Use a synthetic depfile. We can't easily create dirs with spaces
        // in tempdir on all platforms, so just verify parsing logic: the
        // token should have the space unescaped.
        let content = r"foo.o: foo.c path\ with\ spaces/foo.h";
        let source = Path::new("/nonexistent/foo.c");
        let cwd = Path::new("/nonexistent");

        let result = parse_depfile(content, source, cwd).unwrap();

        // The resolved path won't canonicalize (files don't exist), so
        // check that it ends with the unescaped name.
        assert_eq!(result.resolved.len(), 1);
        let dep = &result.resolved[0];
        let dep_str = dep.to_string_lossy();
        assert!(
            dep_str.contains("path with spaces"),
            "expected unescaped space in path, got: {dep_str}"
        );
    }

    // â”€â”€ 4. parse_multiple_targets â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_multiple_targets() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "foo.c");
        let bar_h = touch(cwd, "bar.h");

        // Multiple targets before the colon (gcc -MD -MT produces this).
        let content = "foo.o foo.d: foo.c bar.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0], canon(&bar_h));
    }

    // â”€â”€ 5. parse_empty_deps â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_empty_deps() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "foo.c");

        // Source is the only dependency â€” it gets excluded.
        let content = "foo.o: foo.c";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert!(result.resolved.is_empty());
    }

    // â”€â”€ 6. parse_relative_paths_resolved â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_relative_paths_resolved() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "src/main.c");
        let header = touch(cwd, "inc/util.h");

        // Relative paths in the depfile should be resolved against cwd.
        let content = "src/main.o: src/main.c inc/util.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0], canon(&header));
    }

    // â”€â”€ 7. parse_source_excluded â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_source_excluded() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "main.c");
        let alpha = touch(cwd, "alpha.h");
        let beta = touch(cwd, "beta.h");

        let content = "main.o: main.c alpha.h beta.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        // main.c should not appear in resolved.
        assert_eq!(result.resolved.len(), 2);
        assert!(result.resolved.contains(&canon(&alpha)));
        assert!(result.resolved.contains(&canon(&beta)));
        assert!(!result.resolved.contains(&canon(&source)));
    }

    // â”€â”€ 8. parse_deduplicates â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_deduplicates() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "foo.c");
        let bar_h = touch(cwd, "bar.h");

        // bar.h appears twice â€” should be deduplicated.
        let content = "foo.o: foo.c bar.h bar.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0], canon(&bar_h));
    }

    // â”€â”€ 9. parse_windows_drive_letters â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    #[cfg(windows)]
    fn parse_windows_drive_letters() {
        // The colon after `C` should not be treated as the target separator.
        let content = r"C:\build\foo.o: C:\src\foo.c C:\inc\bar.h";
        let source = Path::new(r"C:\src\foo.c");
        let cwd = Path::new(r"C:\build");

        let result = parse_depfile(content, source, cwd).unwrap();

        // bar.h should be present (foo.c excluded as source).
        assert_eq!(result.resolved.len(), 1);
        let dep = &result.resolved[0];
        let dep_str = dep.to_string_lossy();
        assert!(
            dep_str.contains("bar.h"),
            "expected bar.h in resolved, got: {dep_str}"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn parse_windows_drive_letters() {
        // On non-Windows, just verify the colon parser doesn't choke on
        // drive-letter colons and finds the correct separator.
        let content = r"C:\build\foo.o: C:\src\foo.c C:\inc\bar.h";
        let source = Path::new(r"C:\src\foo.c");
        let cwd = Path::new(r"C:\build");

        let result = parse_depfile(content, source, cwd).unwrap();

        // Source exclusion relies on std::path which handles `\` differently
        // on Unix, so just check that parsing succeeded and bar.h is present.
        let dep_strs: Vec<String> = result
            .resolved
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            dep_strs.iter().any(|s| s.contains("bar.h")),
            "expected bar.h in resolved, got: {dep_strs:?}"
        );
    }

    // â”€â”€ 10. parse_empty_content_errors â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_empty_content_errors() {
        let result = parse_depfile("", Path::new("foo.c"), Path::new("/tmp"));
        assert!(result.is_err());
        match result.unwrap_err() {
            DepfileError::Malformed(msg) => {
                assert!(msg.contains("empty"), "unexpected message: {msg}");
            }
            other => panic!("expected Malformed, got: {other:?}"),
        }
    }

    // â”€â”€ 11. parse_real_gcc_output â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_real_gcc_output() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "main.c");
        let config_h = touch(cwd, "config.h");

        // Create a fake system include dir structure.
        let stdio_h = touch(cwd, "usr/include/stdio.h");
        let stdlib_h = touch(cwd, "usr/include/stdlib.h");

        let content = format!(
            "main.o: main.c config.h \\\n {} \\\n {}",
            stdio_h.display(),
            stdlib_h.display(),
        );

        let result = parse_depfile(&content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 3);
        assert!(result.resolved.contains(&canon(&config_h)));
        assert!(result.resolved.contains(&canon(&stdio_h)));
        assert!(result.resolved.contains(&canon(&stdlib_h)));
        assert!(!result.has_computed);
        assert!(result.unresolved.is_empty());
    }

    // â”€â”€ 12. parse_real_clang_output â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn parse_real_clang_output() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "app.cpp");
        let app_h = touch(cwd, "app.h");
        let types_h = touch(cwd, "include/types.h");
        let vector_h = touch(cwd, "usr/include/c++/vector");

        // Clang-style: uses absolute paths, continuation lines, targets
        // include both .o and .d.
        let content = format!(
            "app.o app.d: {} {} \\\n  {} \\\n  {}",
            source.display(),
            app_h.display(),
            types_h.display(),
            vector_h.display(),
        );

        let result = parse_depfile(&content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 3);
        assert!(result.resolved.contains(&canon(&app_h)));
        assert!(result.resolved.contains(&canon(&types_h)));
        assert!(result.resolved.contains(&canon(&vector_h)));
        assert!(!result.has_computed);
    }

    // â”€â”€ Additional edge cases â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn whitespace_only_is_malformed() {
        let result = parse_depfile("   \n  \t  \n", Path::new("x.c"), Path::new("/tmp"));
        assert!(matches!(result, Err(DepfileError::Malformed(_))));
    }

    #[test]
    fn no_colon_is_malformed() {
        let result = parse_depfile("foo.o foo.c bar.h", Path::new("foo.c"), Path::new("/tmp"));
        assert!(matches!(result, Err(DepfileError::Malformed(_))));
    }

    #[test]
    fn parse_depfile_path_reads_file() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "src.c");
        let hdr = touch(cwd, "hdr.h");

        let depfile = cwd.join("src.d");
        std::fs::write(&depfile, "src.o: src.c hdr.h").unwrap();

        let result = parse_depfile_path(&depfile, &source, cwd).unwrap();
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0], canon(&hdr));
    }

    #[test]
    fn parse_depfile_path_missing_file() {
        let result = parse_depfile_path(
            Path::new("/nonexistent.d"),
            Path::new("x.c"),
            Path::new("/tmp"),
        );
        assert!(matches!(result, Err(DepfileError::Io(_))));
    }

    #[test]
    fn escaped_hash_in_path() {
        let content = r"foo.o: foo.c path\#2/bar.h";
        let source = Path::new("/nonexistent/foo.c");
        let cwd = Path::new("/nonexistent");

        let result = parse_depfile(content, source, cwd).unwrap();
        assert_eq!(result.resolved.len(), 1);
        let dep_str = result.resolved[0].to_string_lossy();
        assert!(
            dep_str.contains("path#2"),
            "expected unescaped '#' in path, got: {dep_str}"
        );
    }

    #[test]
    fn crlf_continuations() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let source = touch(cwd, "foo.c");
        let bar_h = touch(cwd, "bar.h");

        let content = "foo.o: foo.c \\\r\n  bar.h";
        let result = parse_depfile(content, &source, cwd).unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0], canon(&bar_h));
    }

    #[test]
    fn display_impl_for_errors() {
        let io_err = DepfileError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        let msg = format!("{io_err}");
        assert!(msg.contains("I/O error"));

        let mal_err = DepfileError::Malformed("bad content".to_string());
        let msg = format!("{mal_err}");
        assert!(msg.contains("malformed"));
        assert!(msg.contains("bad content"));
    }

    // â”€â”€ Unit tests for internal helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn join_continuations_replaces_with_space() {
        assert_eq!(join_continuations("a \\\n  b"), "a    b");
        assert_eq!(join_continuations("a \\\r\n  b"), "a    b");
    }

    #[test]
    fn join_continuations_preserves_other_backslashes() {
        assert_eq!(join_continuations(r"C:\path\file"), r"C:\path\file");
    }

    #[test]
    fn find_separator_colon_simple() {
        assert_eq!(find_separator_colon("foo.o: bar.c").unwrap(), 5);
    }

    #[test]
    fn find_separator_colon_skips_drive_letter() {
        // The colon at position 1 (C:) should be skipped; the real
        // separator is after "foo.o".
        let line = r"C:\build\foo.o: C:\src\bar.c";
        let pos = find_separator_colon(line).unwrap();
        assert_eq!(&line[pos..pos + 1], ":");
        // The separator should be at position 14 (after "C:\build\foo.o").
        assert_eq!(pos, 14);
    }

    #[test]
    fn split_and_unescape_basic() {
        let tokens = split_and_unescape(" foo.c  bar.h  baz.h ");
        assert_eq!(tokens, vec!["foo.c", "bar.h", "baz.h"]);
    }

    #[test]
    fn split_and_unescape_escaped_space() {
        let tokens = split_and_unescape(r" path\ with\ spaces/foo.h bar.h ");
        assert_eq!(tokens, vec!["path with spaces/foo.h", "bar.h"]);
    }

    #[test]
    fn split_and_unescape_escaped_hash() {
        let tokens = split_and_unescape(r" file\#1.h ");
        assert_eq!(tokens, vec!["file#1.h"]);
    }

    // â”€â”€ Strategy tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn strategy_unsupported() {
        let dep_flags = UserDepFlags::default();
        let (args, strategy) =
            prepare_depfile(false, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));
        assert!(args.is_empty());
        assert_eq!(strategy, DepfileStrategy::Unsupported);
    }

    #[test]
    fn strategy_user_mf() {
        let dep_flags = UserDepFlags {
            has_md: true,
            mf_path: Some(NormalizedPath::from("/build/deps.d")),
        };
        let (args, strategy) =
            prepare_depfile(true, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));
        assert!(args.is_empty());
        assert_eq!(
            strategy,
            DepfileStrategy::UserSpecified {
                path: NormalizedPath::from("/build/deps.d")
            }
        );
    }

    #[test]
    fn strategy_user_md_no_mf() {
        let dep_flags = UserDepFlags {
            has_md: true,
            mf_path: None,
        };
        let (args, strategy) =
            prepare_depfile(true, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));
        assert!(args.is_empty());
        assert_eq!(
            strategy,
            DepfileStrategy::UserDefault {
                path: NormalizedPath::from("foo.d")
            }
        );
    }

    #[test]
    fn strategy_injected() {
        let dep_flags = UserDepFlags::default();
        let (args, strategy) =
            prepare_depfile(true, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));

        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "-MD");
        assert_eq!(args[1], "-MF");
        assert!(args[2].ends_with(".d"));

        match strategy {
            DepfileStrategy::Injected { path } => {
                assert!(path.to_string_lossy().ends_with(".d"));
                assert!(path.starts_with("/tmp"));
            }
            other => panic!("expected Injected, got: {other:?}"),
        }
    }

    #[test]
    fn strategy_injected_adds_args() {
        let dep_flags = UserDepFlags::default();
        let (args, _) = prepare_depfile(true, &dep_flags, Path::new("bar.o"), Path::new("/tmp"));
        assert_eq!(args[0], "-MD");
        assert_eq!(args[1], "-MF");
        // The path should contain the stem "bar".
        assert!(
            args[2].contains("bar"),
            "expected 'bar' in path: {}",
            args[2]
        );
    }
}
