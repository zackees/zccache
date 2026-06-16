//! Low-level depfile parsing: line continuation, target/dep separation,
//! token splitting and unescaping.

use std::collections::HashSet;
use std::path::Path;

use super::canonicalize::canonicalize_path;
use super::error::DepfileError;
use super::super::scanner::ScanResult;

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

    // Issue #578: pre-size both collections to the (over-)bound `tokens.len()`.
    // After dedup + source filter, actual `resolved.len()` ≤ tokens.len();
    // pre-sizing eliminates the grow-from-zero reallocations.
    let mut seen = HashSet::with_capacity(tokens.len());
    let mut resolved = Vec::with_capacity(tokens.len());

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

/// Join backslash-continued lines: replace `\<newline>` sequences with a
/// single space so that the entire depfile becomes one logical line.
pub(super) fn join_continuations(content: &str) -> String {
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
pub(super) fn find_separator_colon(line: &str) -> Result<usize, DepfileError> {
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
/// token (`\ ` → ` `, `\#` → `#`).
pub(super) fn split_and_unescape(deps: &str) -> Vec<String> {
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
