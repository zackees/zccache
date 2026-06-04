//! Response file (`@file`) expansion for compiler arguments.
//!
//! Both GCC and Clang support `@filename` syntax where the file contains
//! additional command-line arguments. This module handles reading and
//! expanding those files into the argument list.

use std::collections::HashSet;
use std::path::Path;

use crate::core::NormalizedPath;

/// Maximum nesting depth for response files to prevent stack overflow.
const MAX_DEPTH: usize = 10;

/// Errors that can occur during response file expansion.
#[derive(Debug, thiserror::Error)]
pub enum ResponseFileError {
    /// The response file could not be read.
    #[error("failed to read response file '{path}': {source}")]
    ReadError {
        path: NormalizedPath,
        source: std::io::Error,
    },
    /// Circular reference detected among response files.
    #[error("circular response file reference: '{path}'")]
    CircularReference { path: NormalizedPath },
    /// Response file nesting exceeded the maximum depth.
    #[error("response file nesting too deep (max {MAX_DEPTH}): '{path}'")]
    TooDeep { path: NormalizedPath },
}

/// Parse the content of a response file into individual arguments.
///
/// Follows GCC/Clang response file conventions:
/// - Arguments are separated by whitespace (spaces, tabs, newlines)
/// - Arguments can be quoted with single (`'`) or double (`"`) quotes
/// - Inside double quotes, backslash escapes `\\`, `\"`, and `\n`
/// - Inside single quotes, no escape processing (literal content)
/// - Unquoted backslash is literal (important for Windows paths)
#[must_use]
pub fn parse_response_file_content(content: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut chars = content.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            // Whitespace separates arguments
            ' ' | '\t' | '\n' | '\r' => {
                if in_arg {
                    result.push(std::mem::take(&mut current));
                    in_arg = false;
                }
                chars.next();
            }
            // Double-quoted string
            '"' => {
                in_arg = true;
                chars.next(); // consume opening quote
                loop {
                    match chars.next() {
                        Some('"') | None => break,
                        Some('\\') => match chars.next() {
                            Some('n') => current.push('\n'),
                            Some(c) => current.push(c),
                            None => break,
                        },
                        Some(c) => current.push(c),
                    }
                }
            }
            // Single-quoted string (no escapes)
            '\'' => {
                in_arg = true;
                chars.next(); // consume opening quote
                loop {
                    match chars.next() {
                        Some('\'') | None => break,
                        Some(c) => current.push(c),
                    }
                }
            }
            // Regular character (backslash is literal in unquoted context)
            _ => {
                in_arg = true;
                current.push(ch);
                chars.next();
            }
        }
    }

    if in_arg {
        result.push(current);
    }

    result
}

/// Expand response file references (`@filename`) in an argument list.
///
/// Scans `args` for arguments starting with `@`. For each such argument,
/// reads the referenced file, parses its contents, and splices the
/// resulting arguments into the list. Supports nested response files
/// up to `MAX_DEPTH` levels. Detects circular references.
///
/// Arguments that are exactly `@` (with no filename) are passed through
/// unchanged, as they are not valid response file references.
///
/// Resolves relative `@file` paths against the process's current working
/// directory. Use [`expand_response_files_in`] to specify a custom base directory.
pub fn expand_response_files(args: &[String]) -> Result<Vec<String>, ResponseFileError> {
    let cwd = std::env::current_dir().map_err(|e| ResponseFileError::ReadError {
        path: NormalizedPath::new("."),
        source: e,
    })?;
    expand_response_files_in(args, &cwd)
}

/// Expand response file references (`@filename`) with a custom base directory.
///
/// Like [`expand_response_files`], but resolves relative `@file` paths against
/// `base_dir` instead of the process's current working directory. For nested
/// `@file` references inside a response file, paths are resolved against the
/// parent file's directory (matching compiler behavior).
pub fn expand_response_files_in(
    args: &[String],
    base_dir: &Path,
) -> Result<Vec<String>, ResponseFileError> {
    let mut seen = HashSet::new();
    expand_recursive(args, base_dir, &mut seen, 0)
}

fn expand_recursive(
    args: &[String],
    base_dir: &Path,
    seen: &mut HashSet<NormalizedPath>,
    depth: usize,
) -> Result<Vec<String>, ResponseFileError> {
    let mut result = Vec::new();

    for arg in args {
        if let Some(filename) = arg.strip_prefix('@') {
            if filename.is_empty() {
                // Bare `@` is not a response file reference
                result.push(arg.clone());
                continue;
            }

            let raw_path = Path::new(filename);
            let resolved = if raw_path.is_absolute() {
                NormalizedPath::new(raw_path)
            } else {
                base_dir.join(raw_path).into()
            };
            let canonical = NormalizedPath::new(resolved.canonicalize().map_err(|e| {
                ResponseFileError::ReadError {
                    path: resolved.clone(),
                    source: e,
                }
            })?);

            if !seen.insert(canonical.clone()) {
                return Err(ResponseFileError::CircularReference { path: resolved });
            }

            if depth >= MAX_DEPTH {
                return Err(ResponseFileError::TooDeep { path: resolved });
            }

            let content =
                std::fs::read_to_string(&canonical).map_err(|e| ResponseFileError::ReadError {
                    path: resolved,
                    source: e,
                })?;

            // Nested @file references resolve against the parent file's directory
            let parent_dir = canonical.parent().unwrap_or(base_dir);

            let expanded_args = parse_response_file_content(&content);
            let nested = expand_recursive(&expanded_args, parent_dir, seen, depth + 1)?;
            result.extend(nested);

            // Remove from seen so the same file can appear in sibling branches
            // (circular = same file in ancestor chain, not sibling)
            seen.remove(&canonical);
        } else {
            result.push(arg.clone());
        }
    }

    Ok(result)
}

/// Maximum command-line length (in bytes) before we spill to a response file.
/// Windows `CreateProcess` has a 32,767 character limit. We use a conservative
/// threshold to account for the compiler path, env block, and quoting overhead.
#[cfg(windows)]
const MAX_CMDLINE_LEN: usize = 30_000;

/// Atomic counter for unique response file names.
#[cfg(windows)]
static RSP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Format arguments as response file content with proper quoting.
///
/// Each argument is written on its own line. Arguments containing spaces,
/// double quotes, or starting with `@` are double-quoted to prevent the
/// compiler from misinterpreting them. Inside quotes, `"` and `\` are
/// backslash-escaped.
#[cfg(any(windows, test))]
#[cfg_attr(not(test), allow(dead_code))]
fn format_rsp_content(args: &[String]) -> String {
    let estimated_len: usize = args.iter().map(|a| a.len() + 3).sum();
    let mut content = String::with_capacity(estimated_len);
    for arg in args {
        content.push_str(&format_rsp_argument(arg).expect("argument should be representable"));
        content.push('\n');
    }
    content
}

#[cfg(any(windows, test))]
fn format_rsp_content_if_safe(args: &[String]) -> Option<String> {
    let estimated_len: usize = args.iter().map(|a| a.len() + 3).sum();
    let mut content = String::with_capacity(estimated_len);
    for arg in args {
        content.push_str(&format_rsp_argument(arg)?);
        content.push('\n');
    }
    Some(content)
}

#[cfg(any(windows, test))]
fn format_rsp_argument(arg: &str) -> Option<String> {
    if arg.is_empty() {
        return Some("''".to_string());
    }

    let needs_quoting = arg.contains(char::is_whitespace)
        || arg.contains('"')
        || arg.contains('\'')
        || arg.starts_with('@');

    if !needs_quoting {
        return Some(arg.to_string());
    }

    // GCC/Clang response files on Windows treat single quotes literally.
    // Prefer them when possible so we preserve backslashes and embedded
    // double quotes exactly as they appeared in the original argv.
    if !arg.contains('\'') {
        return Some(format!("'{arg}'"));
    }

    // Double-quoted response file args are only safe when the argument
    // contains no backslashes, since our parser/compiler model would treat
    // backslashes as escapes and change Windows path semantics.
    if !arg.contains('\\') {
        let escaped = arg.replace('"', "\\\"");
        return Some(format!("\"{escaped}\""));
    }

    None
}

/// Format args for a response file that **rustc** will read.
///
/// Rustc's response-file parser at
/// `compiler/rustc_driver_impl/src/args.rs` is simply
/// `contents.lines().for_each(|arg| ...)` — every line becomes one argv
/// element verbatim, with **no quote handling at all**. That means the
/// GCC/Clang-style single-quote wrapping (`'arg'`) used by
/// [`format_rsp_argument`] would leak the literal `'` characters into
/// the argument value and break parsing of structured options like
/// `--check-cfg "cfg(feature, values(\"...\", \"...\"))"` (issue #634:
/// rustc reported "multiple input filenames provided" when zccache
/// spilled a long web-sys cargo invocation through the GCC-style
/// formatter and the cfg expression got mangled).
///
/// For rustc, the safe shape is: **one arg per line, no quoting, no
/// escaping** — provided no argument contains a newline. Newline-bearing
/// args cannot be represented in rustc's line-oriented format; we
/// return `None` and the caller falls back to passing the args directly
/// on the command line.
#[cfg(any(windows, test))]
fn format_rsp_content_rustc(args: &[String]) -> Option<String> {
    let estimated_len: usize = args.iter().map(|a| a.len() + 1).sum();
    let mut content = String::with_capacity(estimated_len);
    for arg in args {
        if arg.contains('\n') || arg.contains('\r') {
            return None;
        }
        content.push_str(arg);
        content.push('\n');
    }
    Some(content)
}

/// If the total length of `args` exceeds the Windows command-line limit, write
/// them to a temporary `.rsp` file and return a single `@path` argument.
/// Otherwise return `None` (caller should pass args directly).
///
/// `family_hint` selects the response-file dialect:
///
/// - [`CompilerFamily::Rustc`] uses rustc's line-oriented format (one arg
///   per line, no quoting). Required because rustc does not unquote
///   `'..."..."...'`-style values from response files; see #634.
/// - All other families use the GCC/Clang/MSVC-compatible single- or
///   double-quoted format produced by [`format_rsp_argument`].
///
/// The returned [`TempResponseFile`] keeps the temporary file alive via RAII.
/// Drop it after the compiler process finishes.
#[cfg(windows)]
pub fn write_response_file_if_needed(
    args: &[String],
    tmp_dir: &Path,
    family_hint: crate::compiler::CompilerFamily,
) -> std::io::Result<Option<TempResponseFile>> {
    let estimated_len: usize = args.iter().map(|a| a.len() + 3).sum();
    if estimated_len < MAX_CMDLINE_LEN {
        return Ok(None);
    }

    let id = RSP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let rsp_path =
        NormalizedPath::new(tmp_dir.join(format!("zccache_{}_{}.rsp", std::process::id(), id)));
    let content = match family_hint {
        crate::compiler::CompilerFamily::Rustc => format_rsp_content_rustc(args),
        _ => format_rsp_content_if_safe(args),
    };
    let Some(content) = content else {
        return Ok(None);
    };
    std::fs::write(&rsp_path, content)?;

    Ok(Some(TempResponseFile { path: rsp_path }))
}

/// No-op on non-Windows platforms (command-line length is not an issue).
#[cfg(not(windows))]
pub fn write_response_file_if_needed(
    _args: &[String],
    _tmp_dir: &Path,
    _family_hint: crate::compiler::CompilerFamily,
) -> std::io::Result<Option<TempResponseFile>> {
    Ok(None)
}

/// RAII guard for a temporary response file. Deletes the file on drop.
pub struct TempResponseFile {
    pub path: NormalizedPath,
}

impl TempResponseFile {
    /// Returns the `@path` argument to pass to the compiler.
    pub fn at_arg(&self) -> String {
        format!("@{}", self.path.display())
    }
}

impl Drop for TempResponseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // â”€â”€ parse_response_file_content tests â”€â”€

    #[test]
    fn parse_simple_whitespace_separated() {
        let result = parse_response_file_content("-c foo.cpp -o foo.o");
        assert_eq!(result, s(&["-c", "foo.cpp", "-o", "foo.o"]));
    }

    #[test]
    fn parse_newline_separated() {
        let result = parse_response_file_content("-c\nfoo.cpp\n-o\nfoo.o\n");
        assert_eq!(result, s(&["-c", "foo.cpp", "-o", "foo.o"]));
    }

    #[test]
    fn parse_mixed_whitespace() {
        let result = parse_response_file_content("  -c \t foo.cpp \n -O2  ");
        assert_eq!(result, s(&["-c", "foo.cpp", "-O2"]));
    }

    #[test]
    fn parse_double_quoted_string() {
        let result = parse_response_file_content(r#"-DMSG="hello world" -c foo.c"#);
        assert_eq!(result, s(&["-DMSG=hello world", "-c", "foo.c"]));
    }

    #[test]
    fn parse_single_quoted_string() {
        let result = parse_response_file_content("-DMSG='hello world' -c foo.c");
        assert_eq!(result, s(&["-DMSG=hello world", "-c", "foo.c"]));
    }

    #[test]
    fn parse_escaped_backslash_in_double_quotes() {
        let result = parse_response_file_content(r#""-I C:\\path\\to\\include""#);
        assert_eq!(result, s(&["-I C:\\path\\to\\include"]));
    }

    #[test]
    fn parse_escaped_quote_in_double_quotes() {
        let result = parse_response_file_content(r#""-DMSG=\"hi\"""#);
        assert_eq!(result, s(&[r#"-DMSG="hi""#]));
    }

    #[test]
    fn parse_escaped_newline_in_double_quotes() {
        let result = parse_response_file_content(r#""-DMSG=line1\nline2""#);
        assert_eq!(result, s(&["-DMSG=line1\nline2"]));
    }

    #[test]
    fn parse_single_quotes_no_escapes() {
        // Single quotes are literal â€” backslash is not special
        let result = parse_response_file_content(r"'-DMSG=a\nb'");
        assert_eq!(result, s(&[r"-DMSG=a\nb"]));
    }

    #[test]
    fn parse_unquoted_backslash_literal() {
        // Backslash is literal in unquoted context (important for Windows paths)
        let result = parse_response_file_content(r"-IC:\Users\include");
        assert_eq!(result, s(&[r"-IC:\Users\include"]));
    }

    #[test]
    fn parse_empty_content() {
        let result = parse_response_file_content("");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_only_whitespace() {
        let result = parse_response_file_content("   \n\t\r\n  ");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_empty_quoted_string() {
        let result = parse_response_file_content(r#""""#);
        assert_eq!(result, s(&[""]));
    }

    #[test]
    fn parse_adjacent_quoted_and_unquoted() {
        // -I"path with spaces" should produce -Ipath with spaces as one arg
        let result = parse_response_file_content(r#"-I"path with spaces""#);
        assert_eq!(result, s(&["-Ipath with spaces"]));
    }

    // â”€â”€ expand_response_files tests â”€â”€

    #[test]
    fn expand_no_at_files() {
        let args = s(&["-c", "foo.cpp", "-o", "foo.o"]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, args);
    }

    #[test]
    fn expand_bare_at_passthrough() {
        // A bare `@` with no filename is not a response file reference
        let args = s(&["-c", "@", "foo.cpp"]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, args);
    }

    #[test]
    fn expand_single_response_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-O2 -Wall -DNDEBUG").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-c", "foo.cpp", "-O2", "-Wall", "-DNDEBUG"]));
    }

    #[test]
    fn expand_response_file_with_quoted_args() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"-DMSG="hello world" -I"/path/to/include""#).unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(
            result,
            s(&["-c", "foo.cpp", "-DMSG=hello world", "-I/path/to/include"])
        );
    }

    #[test]
    fn expand_multiple_response_files() {
        let mut f1 = NamedTempFile::new().unwrap();
        writeln!(f1, "-O2 -Wall").unwrap();
        let mut f2 = NamedTempFile::new().unwrap();
        writeln!(f2, "-DNDEBUG -std=c++17").unwrap();

        let p1 = f1.path().to_str().unwrap();
        let p2 = f2.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{p1}"), &format!("@{p2}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(
            result,
            s(&["-c", "foo.cpp", "-O2", "-Wall", "-DNDEBUG", "-std=c++17"])
        );
    }

    #[test]
    fn expand_nested_response_files() {
        let mut inner = NamedTempFile::new().unwrap();
        writeln!(inner, "-DINNER=1").unwrap();

        let inner_path = inner.path().to_str().unwrap();
        let mut outer = NamedTempFile::new().unwrap();
        writeln!(outer, "-DOUTER=1 @{inner_path}").unwrap();

        let outer_path = outer.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{outer_path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-c", "foo.cpp", "-DOUTER=1", "-DINNER=1"]));
    }

    #[test]
    fn expand_missing_file_errors() {
        let args = s(&["-c", "foo.cpp", "@/nonexistent/file.rsp"]);
        let result = expand_response_files(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ResponseFileError::ReadError { .. }));
    }

    #[test]
    fn expand_circular_reference_errors() {
        // Create two files that reference each other
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.rsp");
        let path_b = dir.path().join("b.rsp");

        std::fs::write(&path_a, format!("@{}", path_b.display())).unwrap();
        std::fs::write(&path_b, format!("@{}", path_a.display())).unwrap();

        let args = s(&["-c", "foo.cpp", &format!("@{}", path_a.display())]);
        let result = expand_response_files(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ResponseFileError::CircularReference { .. }),
            "expected CircularReference, got: {err}"
        );
    }

    #[test]
    fn expand_self_reference_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self.rsp");
        std::fs::write(&path, format!("-O2 @{}", path.display())).unwrap();

        let args = s(&[&format!("@{}", path.display())]);
        let result = expand_response_files(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ResponseFileError::CircularReference { .. }),
            "expected CircularReference, got: {err}"
        );
    }

    #[test]
    fn expand_preserves_arg_order() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-B -C").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-A", &format!("@{path}"), "-D"]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-A", "-B", "-C", "-D"]));
    }

    #[test]
    fn expand_same_file_in_siblings_ok() {
        // The same file referenced twice at the same depth is OK
        // (circular = same file in ancestor chain)
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-O2").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&[&format!("@{path}"), &format!("@{path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-O2", "-O2"]));
    }

    #[test]
    fn expand_integration_with_parse_invocation() {
        // End-to-end: expand response files, then parse the invocation
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-O2 -Wall -DNDEBUG").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", "-o", "foo.o", &format!("@{path}")]);
        let expanded = expand_response_files(&args).unwrap();
        let result = super::super::parse_invocation("gcc", &expanded);
        match result {
            super::super::ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, NormalizedPath::new("foo.cpp"));
                assert_eq!(c.output_file, NormalizedPath::new("foo.o"));
                assert!(c.original_args.contains(&"-O2".to_string()));
                assert!(c.original_args.contains(&"-Wall".to_string()));
                assert!(c.original_args.contains(&"-DNDEBUG".to_string()));
            }
            _ => panic!("unexpected variant"),
        }
    }

    // â”€â”€ expand_response_files_in tests â”€â”€

    #[test]
    fn expand_in_resolves_relative_against_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rsp_path = dir.path().join("flags.rsp");
        std::fs::write(&rsp_path, "-O2 -Wall").unwrap();

        // Use relative name, resolve against dir
        let args = s(&["@flags.rsp", "-c", "foo.cpp"]);
        let result = expand_response_files_in(&args, dir.path()).unwrap();
        assert_eq!(result, s(&["-O2", "-Wall", "-c", "foo.cpp"]));
    }

    #[test]
    fn expand_in_absolute_path_ignores_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rsp_path = dir.path().join("flags.rsp");
        std::fs::write(&rsp_path, "-O2").unwrap();

        let abs_ref = format!("@{}", rsp_path.display());
        let args = s(&[&abs_ref, "-c", "foo.cpp"]);
        // base_dir is irrelevant for absolute paths
        let other_dir = tempfile::tempdir().unwrap();
        let result = expand_response_files_in(&args, other_dir.path()).unwrap();
        assert_eq!(result, s(&["-O2", "-c", "foo.cpp"]));
    }

    #[test]
    fn expand_in_nested_resolves_against_parent_dir() {
        // outer/ contains outer.rsp which references @inner.rsp
        // inner/ (sibling) contains inner.rsp
        // But inner.rsp is in same dir as outer.rsp, so it resolves correctly.
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let inner_path = sub.join("inner.rsp");
        std::fs::write(&inner_path, "-DINNER=1").unwrap();

        let outer_path = sub.join("outer.rsp");
        std::fs::write(&outer_path, "-DOUTER=1 @inner.rsp").unwrap();

        // Resolve @sub/outer.rsp against base dir
        let args = s(&["-c", "foo.cpp", "@sub/outer.rsp"]);
        let result = expand_response_files_in(&args, dir.path()).unwrap();
        assert_eq!(result, s(&["-c", "foo.cpp", "-DOUTER=1", "-DINNER=1"]));
    }

    #[test]
    fn expand_in_nested_relative_cross_directory() {
        // base_dir/outer.rsp references @subdir/inner.rsp
        // subdir/inner.rsp exists relative to outer.rsp's directory
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();

        std::fs::write(subdir.join("inner.rsp"), "-DINNER=1").unwrap();
        std::fs::write(dir.path().join("outer.rsp"), "@subdir/inner.rsp -DOUTER=1").unwrap();

        let args = s(&["@outer.rsp"]);
        let result = expand_response_files_in(&args, dir.path()).unwrap();
        assert_eq!(result, s(&["-DINNER=1", "-DOUTER=1"]));
    }

    #[test]
    fn expand_in_dotdot_traversal() {
        // base_dir/sub/outer.rsp references @../sibling.rsp
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        std::fs::write(dir.path().join("sibling.rsp"), "-DSIBLING=1").unwrap();
        std::fs::write(sub.join("outer.rsp"), "@../sibling.rsp -DOUTER=1").unwrap();

        let args = s(&["@sub/outer.rsp"]);
        let result = expand_response_files_in(&args, dir.path()).unwrap();
        assert_eq!(result, s(&["-DSIBLING=1", "-DOUTER=1"]));
    }

    #[test]
    fn expand_in_nested_absolute_inside_relative() {
        // @relative.rsp contains @/absolute/path.rsp â€” absolute ref ignores parent dir
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let abs_rsp = sub.join("abs.rsp");
        std::fs::write(&abs_rsp, "-DABS=1").unwrap();

        // outer.rsp in base_dir references abs.rsp by absolute path
        std::fs::write(
            dir.path().join("outer.rsp"),
            format!("@{} -DOUTER=1", abs_rsp.display()),
        )
        .unwrap();

        let args = s(&["@outer.rsp"]);
        let result = expand_response_files_in(&args, dir.path()).unwrap();
        assert_eq!(result, s(&["-DABS=1", "-DOUTER=1"]));
    }

    #[test]
    fn expand_in_three_level_relative_chain() {
        // a/1.rsp -> @b/2.rsp -> @c/3.rsp â€” each relative to its parent
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let ab = a.join("b");
        let abc = ab.join("c");
        std::fs::create_dir_all(&abc).unwrap();

        std::fs::write(abc.join("3.rsp"), "-DLEVEL3=1").unwrap();
        std::fs::write(ab.join("2.rsp"), "@c/3.rsp -DLEVEL2=1").unwrap();
        std::fs::write(a.join("1.rsp"), "@b/2.rsp -DLEVEL1=1").unwrap();

        let args = s(&["@a/1.rsp"]);
        let result = expand_response_files_in(&args, dir.path()).unwrap();
        assert_eq!(result, s(&["-DLEVEL3=1", "-DLEVEL2=1", "-DLEVEL1=1"]));
    }

    #[test]
    fn expand_in_error_shows_resolved_path() {
        // @relative.rsp that doesn't exist â€” error should show resolved path, not raw
        let dir = tempfile::tempdir().unwrap();
        let args = s(&["@missing.rsp"]);
        let err = expand_response_files_in(&args, dir.path()).unwrap_err();
        match &err {
            ResponseFileError::ReadError { path, .. } => {
                // Error path should be the resolved path (base_dir/missing.rsp)
                assert!(
                    path.starts_with(dir.path()),
                    "error path {path:?} should be under {dir:?}",
                    dir = dir.path()
                );
            }
            other => panic!("expected ReadError, got: {other}"),
        }
    }

    #[test]
    fn expand_in_circular_in_custom_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rsp");
        let b = dir.path().join("b.rsp");
        std::fs::write(&a, format!("@{}", b.display())).unwrap();
        std::fs::write(&b, format!("@{}", a.display())).unwrap();

        let args = s(&["@a.rsp"]);
        let err = expand_response_files_in(&args, dir.path()).unwrap_err();
        assert!(
            matches!(err, ResponseFileError::CircularReference { .. }),
            "expected CircularReference, got: {err}"
        );
    }

    #[test]
    fn expand_in_depth_limit_with_custom_base_dir() {
        // Chain of 11 files (depth 0..10, exceeds MAX_DEPTH=10)
        let dir = tempfile::tempdir().unwrap();
        for i in (0..=MAX_DEPTH).rev() {
            let name = format!("{i}.rsp");
            let content = if i == MAX_DEPTH {
                "-DLEAF=1".to_string()
            } else {
                format!("@{}.rsp", i + 1)
            };
            std::fs::write(dir.path().join(name), content).unwrap();
        }

        let args = s(&["@0.rsp"]);
        let err = expand_response_files_in(&args, dir.path()).unwrap_err();
        assert!(
            matches!(err, ResponseFileError::TooDeep { .. }),
            "expected TooDeep, got: {err}"
        );
    }

    // â”€â”€ format_rsp_content / write_response_file tests â”€â”€

    #[test]
    fn format_rsp_simple_args() {
        let args = s(&["-c", "foo.cpp", "-O2"]);
        let content = format_rsp_content(&args);
        assert_eq!(content, "-c\nfoo.cpp\n-O2\n");
    }

    #[test]
    fn format_rsp_quotes_spaces() {
        let args = s(&["-I/path with spaces/include", "-c"]);
        let content = format_rsp_content(&args);
        assert_eq!(content, "'-I/path with spaces/include'\n-c\n");
    }

    #[test]
    fn format_rsp_escapes_quotes() {
        let args = s(&[r#"-DMSG="hello""#]);
        let content = format_rsp_content(&args);
        assert_eq!(content, "'-DMSG=\"hello\"'\n");
    }

    #[test]
    fn format_rsp_escapes_backslash_in_quoted() {
        let args = s(&[r"-IC:\path with spaces\include"]);
        let content = format_rsp_content(&args);
        assert_eq!(content, "'-IC:\\path with spaces\\include'\n");
    }

    #[test]
    fn format_rsp_quotes_at_prefix() {
        // Args starting with @ must be quoted to prevent the compiler
        // from interpreting them as nested response file references.
        let args = s(&["@rpath/lib", "-c"]);
        let content = format_rsp_content(&args);
        assert_eq!(content, "'@rpath/lib'\n-c\n");
    }

    #[test]
    fn format_rsp_roundtrip() {
        // Write -> parse should recover the original args
        let args = s(&[
            "-c",
            "foo.cpp",
            "-I/path with spaces",
            r#"-DMSG="hello""#,
            "@rpath/lib",
            "-O2",
            r"-IC:\Users\include",
        ]);
        let content = format_rsp_content(&args);
        let parsed = parse_response_file_content(&content);
        assert_eq!(parsed, args);
    }

    #[test]
    fn format_rsp_declines_unsafe_single_quote_with_backslash() {
        let args = s(&[r"C:\path with spaces\o'hare"]);
        assert!(format_rsp_content_if_safe(&args).is_none());
    }

    #[test]
    fn format_rsp_prefers_single_quotes_for_windows_gcc_shapes() {
        let args = s(&[
            r#"-DVALUE="C:\Program Files\SDK\include""#,
            r"C:\work tree\main.c",
        ]);
        let content = format_rsp_content_if_safe(&args).unwrap();
        assert_eq!(
            content,
            "'-DVALUE=\"C:\\Program Files\\SDK\\include\"'\n'C:\\work tree\\main.c'\n"
        );
        assert_eq!(parse_response_file_content(&content), args);
    }

    // ── #634: rustc response-file dialect ─────────────────────────────

    /// rustc's response-file parser is `file.lines().for_each(|arg|...)`
    /// — every line is one argv element, no quote handling. The rustc
    /// format function must therefore write each arg literally, on its
    /// own line.
    #[test]
    fn format_rsp_content_rustc_writes_args_literally() {
        let args = s(&["--crate-name", "web_sys", "--edition=2021"]);
        let content = format_rsp_content_rustc(&args).unwrap();
        assert_eq!(content, "--crate-name\nweb_sys\n--edition=2021\n");
    }

    /// Regression guard for #634: the cargo `--check-cfg` value for
    /// web-sys is a structured expression containing whitespace, commas,
    /// parens, and embedded double quotes. The GCC-style formatter
    /// wraps it in single quotes (`'cfg(feature, values(\"a\", ...))'`)
    /// which rustc reads literally and chokes on. The rustc formatter
    /// must NOT add quoting characters.
    #[test]
    fn format_rsp_content_rustc_preserves_check_cfg_with_embedded_quotes() {
        let check_cfg = r#"cfg(feature, values("AbortController", "AbortSignal"))"#;
        let args = s(&["--check-cfg", check_cfg]);
        let content = format_rsp_content_rustc(&args).unwrap();
        // No leading/trailing quote characters were added to either line.
        assert_eq!(
            content,
            "--check-cfg\ncfg(feature, values(\"AbortController\", \"AbortSignal\"))\n"
        );
        // Round-trip: rustc's `lines()` parse recovers exactly the
        // original argv. We model rustc's parser as `lines()` here.
        let recovered: Vec<String> = content.lines().map(str::to_string).collect();
        assert_eq!(recovered, args);
    }

    /// Newlines cannot be represented in rustc's line-oriented format;
    /// the formatter must refuse so the caller can fall back to
    /// passing args directly on the command line.
    #[test]
    fn format_rsp_content_rustc_refuses_args_with_newline() {
        let args = s(&["--foo", "value-with-\n-newline"]);
        assert!(format_rsp_content_rustc(&args).is_none());

        let args_cr = s(&["--foo", "value-with-\r-cr"]);
        assert!(format_rsp_content_rustc(&args_cr).is_none());
    }

    /// Contrast with the GCC-style formatter: the same `--check-cfg`
    /// arg gets wrapped in single quotes, which is exactly the bug
    /// shape from #634 when fed to rustc.
    #[test]
    fn format_rsp_content_gcc_style_wraps_in_single_quotes() {
        let check_cfg = r#"cfg(feature, values("AbortController"))"#;
        let args = s(&["--check-cfg", check_cfg]);
        let content = format_rsp_content_if_safe(&args).unwrap();
        // GCC-style: the value got wrapped in single quotes. rustc would
        // see the leading `'` as part of the value and choke.
        assert!(
            content.contains("'cfg(feature, values(\"AbortController\"))'"),
            "GCC-style format wraps in single quotes; got: {content:?}"
        );
    }

    #[test]
    fn temp_response_file_at_arg() {
        let path = NormalizedPath::new("/tmp/test.rsp");
        let rsp = TempResponseFile { path };
        assert_eq!(rsp.at_arg(), format!("@{}", rsp.path.display()));
        std::mem::forget(rsp);
    }

    #[test]
    fn temp_response_file_cleanup_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let rsp_path = dir.path().join("test.rsp");
        std::fs::write(&rsp_path, "test").unwrap();
        assert!(rsp_path.exists());

        let rsp = TempResponseFile {
            path: rsp_path.clone().into(),
        };
        drop(rsp);
        assert!(!rsp_path.exists());
    }
}
