//! `#include` directive scanner.
//!
//! Scans C/C++ source files for `#include` directives, skipping comments
//! and string literals. Does not evaluate preprocessor conditionals â€”
//! all `#include` directives are returned unconditionally.

use std::path::Path;

use super::search_paths::IncludeSearchPaths;
use zccache_core::NormalizedPath;

/// The kind of `#include` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeKind {
    /// `#include "foo.h"` â€” quoted include.
    Quoted,
    /// `#include <foo.h>` â€” angle-bracket include.
    AngleBracket,
    /// `#include MACRO` â€” computed include, cannot resolve by text scanning.
    Computed(String),
}

/// A parsed `#include` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeDirective {
    /// The kind of include.
    pub kind: IncludeKind,
    /// The path as written in the source (for Quoted/AngleBracket),
    /// or the macro name (for Computed).
    pub path: String,
    /// 1-based line number in the file.
    pub line: u32,
}

/// Result of a recursive include scan.
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// All resolved include paths (absolute, deduplicated).
    pub resolved: Vec<NormalizedPath>,
    /// Include paths that could not be resolved to an existing file.
    pub unresolved: Vec<String>,
    /// True if any `#include MACRO` (computed include) was found.
    pub has_computed: bool,
}

/// Scan a source string for `#include` directives.
///
/// Skips directives inside `//` line comments, `/* */` block comments,
/// and string/character literals. Handles backslash line continuations.
pub fn scan_includes_str(source: &str) -> Vec<IncludeDirective> {
    let joined = join_continuations(source);
    let mut results = Vec::new();

    // Track original line numbers: each line in `joined` maps to a source line.
    // After joining continuations, we need to track the starting line of each
    // logical line.
    let line_map = build_line_map(source);

    let mut in_block_comment = false;

    for (logical_idx, line) in joined.lines().enumerate() {
        let source_line = if logical_idx < line_map.len() {
            line_map[logical_idx]
        } else {
            (logical_idx + 1) as u32
        };

        if in_block_comment {
            if let Some(end) = line.find("*/") {
                // Block comment ends on this line. Check rest of line.
                let rest = &line[end + 2..];
                if let Some(dir) = parse_include_from_line(rest) {
                    results.push(IncludeDirective {
                        line: source_line,
                        ..dir
                    });
                }
                in_block_comment = false;
                // Could have another block comment start after — fuse the
                // detect-and-locate into one search to drop the expect and
                // halve the scanner's per-line work on the hot path.
                if let Some(after_end) = rest.find("/*") {
                    if !rest[..after_end].contains("*/") {
                        in_block_comment = true;
                    }
                }
            }
            continue;
        }

        // Strip line comments first.
        let effective = strip_comments(line, &mut in_block_comment);
        if let Some(dir) = parse_include_from_line(&effective) {
            results.push(IncludeDirective {
                line: source_line,
                ..dir
            });
        }
    }

    results
}

/// Scan a file on disk for `#include` directives.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn scan_includes(path: &Path) -> std::io::Result<Vec<IncludeDirective>> {
    let source = std::fs::read_to_string(path)?;
    Ok(scan_includes_str(&source))
}

/// Resolve a single `#include` directive to an absolute path.
///
/// For quoted includes, searches the including file's directory first,
/// then `-iquote`, `-I`, `-isystem`, `-idirafter` in order.
///
/// For angle-bracket includes, searches `-I`, `-isystem`, `-idirafter`.
///
/// Returns `None` if the file is not found in any search path.
pub fn resolve_include(
    directive: &IncludeDirective,
    search: &IncludeSearchPaths,
    including_file_dir: &Path,
) -> Option<NormalizedPath> {
    match &directive.kind {
        IncludeKind::Quoted => {
            // 1. Directory of the including file.
            let candidate = including_file_dir.join(&directive.path);
            if candidate.is_file() {
                return Some(normalize(&candidate));
            }
            // 2. Search paths for quoted includes.
            for dir in search.quoted_search_dirs() {
                let candidate = dir.join(&directive.path);
                if candidate.is_file() {
                    return Some(normalize(&candidate));
                }
            }
            None
        }
        IncludeKind::AngleBracket => {
            for dir in search.angle_search_dirs() {
                let candidate = dir.join(&directive.path);
                if candidate.is_file() {
                    return Some(normalize(&candidate));
                }
            }
            None
        }
        IncludeKind::Computed(_) => None,
    }
}

/// Recursively scan a source file for all transitive includes.
///
/// Builds the full include list by scanning the source file, resolving
/// each `#include`, then scanning each resolved header, and so on, using
/// a parallel BFS over per-level frontiers. Headers within a frontier are
/// read and parsed in parallel via rayon; new resolutions feed the next
/// frontier. A `DashSet` deduplicates so each header is scanned exactly
/// once across the DAG, even with circular or diamond includes.
///
/// `resolved` returns in BFS-level order (was DFS-post-order before
/// parallelization). Callers in `graph.rs` only iterate the list to hash
/// all files; no order invariant is broken.
pub fn scan_recursive(source: &Path, search: &IncludeSearchPaths) -> ScanResult {
    use dashmap::DashSet;
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    let visited: DashSet<NormalizedPath> = DashSet::new();
    let resolved: Mutex<Vec<NormalizedPath>> = Mutex::new(Vec::new());
    let unresolved: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let has_computed = AtomicBool::new(false);

    // Mark the source itself as visited so we don't re-scan it via a
    // self-include chain.
    if let Some(abs) = try_normalize(source) {
        visited.insert(abs);
    }

    let mut frontier: Vec<NormalizedPath> = vec![NormalizedPath::from(source)];
    while !frontier.is_empty() {
        let next: Vec<NormalizedPath> = frontier
            .par_iter()
            .flat_map_iter(|file| {
                scan_one_level(
                    file.as_path(),
                    search,
                    &visited,
                    &resolved,
                    &unresolved,
                    &has_computed,
                )
            })
            .collect();
        frontier = next;
    }

    ScanResult {
        // Poison only happens if a rayon worker panicked; recovering the
        // inner Vec preserves the partial scan output of the surviving
        // workers, which is what callers want.
        resolved: resolved.into_inner().unwrap_or_else(|e| e.into_inner()),
        unresolved: unresolved.into_inner().unwrap_or_else(|e| e.into_inner()),
        has_computed: has_computed.load(Ordering::Relaxed),
    }
}

/// Scan one file: read it, parse `#include`s, resolve each, and return the
/// list of newly-discovered resolved paths for the next frontier level.
///
/// All four shared collections take exactly one lock per scanned file: the
/// per-file results are buffered locally and pushed in a single batch at
/// the end. This keeps Mutex contention proportional to (file count) and
/// not to (include count).
fn scan_one_level(
    file: &Path,
    search: &IncludeSearchPaths,
    visited: &dashmap::DashSet<NormalizedPath>,
    resolved: &std::sync::Mutex<Vec<NormalizedPath>>,
    unresolved: &std::sync::Mutex<Vec<String>>,
    has_computed: &std::sync::atomic::AtomicBool,
) -> Vec<NormalizedPath> {
    let directives = match scan_includes(file) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let file_dir = file.parent().unwrap_or(Path::new("."));

    let mut new_for_next: Vec<NormalizedPath> = Vec::new();
    let mut local_resolved: Vec<NormalizedPath> = Vec::new();
    let mut local_unresolved: Vec<String> = Vec::new();
    let mut saw_computed = false;

    for directive in &directives {
        match &directive.kind {
            IncludeKind::Computed(_) => {
                saw_computed = true;
            }
            _ => {
                if let Some(abs_path) = resolve_include(directive, search, file_dir) {
                    if visited.insert(abs_path.clone()) {
                        local_resolved.push(abs_path.clone());
                        new_for_next.push(abs_path);
                    }
                } else {
                    local_unresolved.push(directive.path.clone());
                }
            }
        }
    }

    if !local_resolved.is_empty() {
        resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(local_resolved);
    }
    if !local_unresolved.is_empty() {
        unresolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(local_unresolved);
    }
    if saw_computed {
        has_computed.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    new_for_next
}

// â”€â”€ Helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Join backslash-continued lines into single logical lines.
fn join_continuations(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('\n') => {
                    chars.next(); // consume the newline
                                  // Don't emit either the backslash or the newline.
                }
                Some('\r') => {
                    chars.next(); // consume \r
                    if chars.peek() == Some(&'\n') {
                        chars.next(); // consume \n
                    }
                    // Don't emit.
                }
                _ => result.push(ch),
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Build a map from logical line index to 1-based source line number.
/// Accounts for backslash continuations merging multiple source lines.
fn build_line_map(source: &str) -> Vec<u32> {
    let mut map = Vec::new();
    let mut source_line: u32 = 1;
    let mut continued = false;

    for line in source.split('\n') {
        if !continued {
            map.push(source_line);
        }
        let trimmed = line.trim_end_matches('\r');
        continued = trimmed.ends_with('\\');
        source_line += 1;
    }

    map
}

/// Strip line comments and block comments from a line.
/// Updates `in_block_comment` state for multi-line block comments.
///
/// String literals are NOT stripped. This is intentional:
/// `#include "foo.h"` has quotes that look like strings but are part of
/// the directive syntax. False positives like `const char* s = "#include ..."`
/// are handled by `parse_include_from_line` which requires `#` to be the
/// first non-whitespace character on the line.
fn strip_comments(line: &str, in_block_comment: &mut bool) -> String {
    let mut result = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if *in_block_comment {
            if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                *in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // Line comment â€” stop processing this line.
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            break;
        }

        // Block comment start.
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            *in_block_comment = true;
            i += 2;
            continue;
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// Parse an `#include` directive from a (comment-stripped) line.
fn parse_include_from_line(line: &str) -> Option<IncludeDirective> {
    let trimmed = line.trim();

    // Must start with #
    let after_hash = trimmed.strip_prefix('#')?;
    let after_hash = after_hash.trim();

    // Must be "include"
    let after_include = after_hash.strip_prefix("include")?;

    // "include" must not be part of a longer identifier.
    if let Some(next_ch) = after_include.chars().next() {
        if next_ch.is_alphanumeric() || next_ch == '_' {
            return None;
        }
    }

    let rest = after_include.trim();

    if rest.is_empty() {
        return None;
    }

    // #include "path"
    if let Some(inner) = rest.strip_prefix('"') {
        let end = inner.find('"')?;
        let path = &inner[..end];
        if path.is_empty() {
            return None;
        }
        return Some(IncludeDirective {
            kind: IncludeKind::Quoted,
            path: path.to_string(),
            line: 0, // Filled in by caller.
        });
    }

    // #include <path>
    if let Some(inner) = rest.strip_prefix('<') {
        let end = inner.find('>')?;
        let path = &inner[..end];
        if path.is_empty() {
            return None;
        }
        return Some(IncludeDirective {
            kind: IncludeKind::AngleBracket,
            path: path.to_string(),
            line: 0,
        });
    }

    // #include MACRO â€” computed include.
    let macro_name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if !macro_name.is_empty() {
        return Some(IncludeDirective {
            kind: IncludeKind::Computed(macro_name.clone()),
            path: macro_name,
            line: 0,
        });
    }

    None
}

/// Normalize a path to an absolute path (best-effort, no symlink resolution).
fn normalize(path: &Path) -> NormalizedPath {
    try_normalize(path).unwrap_or_else(|| path.into())
}

fn try_normalize(path: &Path) -> Option<NormalizedPath> {
    // Use canonicalize which resolves symlinks and produces an absolute path.
    // On Windows, canonicalize produces \\?\ extended-length paths which must
    // be stripped to match the watcher's path format for journal lookups.
    let p = path.canonicalize().ok()?;
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return Some(NormalizedPath::from(stripped));
        }
    }
    Some(p.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // â”€â”€ scan_includes_str tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn basic_quoted_include() {
        let source = r#"#include "foo.h""#;
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].kind, IncludeKind::Quoted);
        assert_eq!(includes[0].path, "foo.h");
        assert_eq!(includes[0].line, 1);
    }

    #[test]
    fn basic_angle_bracket_include() {
        let source = "#include <stdio.h>";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].kind, IncludeKind::AngleBracket);
        assert_eq!(includes[0].path, "stdio.h");
    }

    #[test]
    fn multiple_includes() {
        let source = r#"
#include <stdio.h>
#include "config.h"
#include <stdlib.h>
"#;
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 3);
        assert_eq!(includes[0].path, "stdio.h");
        assert_eq!(includes[1].path, "config.h");
        assert_eq!(includes[2].path, "stdlib.h");
    }

    #[test]
    fn include_with_path_separators() {
        let source = r#"#include "path/to/header.h""#;
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "path/to/header.h");
    }

    #[test]
    fn computed_include() {
        let source = "#include PLATFORM_HEADER";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(
            includes[0].kind,
            IncludeKind::Computed("PLATFORM_HEADER".to_string())
        );
        assert_eq!(includes[0].path, "PLATFORM_HEADER");
    }

    #[test]
    fn skip_line_comment() {
        let source = r#"
// #include "old.h"
#include "real.h"
"#;
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "real.h");
    }

    #[test]
    fn skip_block_comment() {
        let source = r#"
/* #include "old.h" */
#include "real.h"
"#;
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "real.h");
    }

    #[test]
    fn skip_multiline_block_comment() {
        let source = r#"
/*
#include "old1.h"
#include "old2.h"
*/
#include "real.h"
"#;
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "real.h");
    }

    #[test]
    fn skip_include_in_string_literal() {
        let source = "const char* s = \"#include \\\"fake.h\\\"\";\n#include \"real.h\"\n";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "real.h");
    }

    #[test]
    fn backslash_continuation() {
        let source = "#in\\\nclude \"continued.h\"";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "continued.h");
    }

    #[test]
    fn indented_include() {
        let source = "    #include <indented.h>";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "indented.h");
    }

    #[test]
    fn hash_space_include() {
        let source = "#  include <spaced.h>";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "spaced.h");
    }

    #[test]
    fn not_include_directive() {
        let source = "#define FOO 1\n#ifdef BAR\n#endif\n";
        let includes = scan_includes_str(source);
        assert!(includes.is_empty());
    }

    #[test]
    fn include_guard_not_confused() {
        let source = "#ifndef FOO_H\n#define FOO_H\n#include \"bar.h\"\n#endif\n";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "bar.h");
    }

    #[test]
    fn line_numbers_are_correct() {
        let source = "// preamble\n\n#include \"a.h\"\n\n#include <b.h>\n";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0].line, 3);
        assert_eq!(includes[1].line, 5);
    }

    #[test]
    fn empty_source() {
        let includes = scan_includes_str("");
        assert!(includes.is_empty());
    }

    #[test]
    fn include_after_code() {
        let source = "int x = 1;\n#include \"late.h\"\n";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "late.h");
    }

    #[test]
    fn block_comment_ending_on_include_line() {
        let source = "/* comment */ #include \"after.h\"";
        let includes = scan_includes_str(source);
        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "after.h");
    }

    // â”€â”€ resolve_include tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn resolve_quoted_in_file_dir() {
        let dir = TempDir::new().unwrap();
        let header = dir.path().join("local.h");
        std::fs::write(&header, "// header").unwrap();

        let directive = IncludeDirective {
            kind: IncludeKind::Quoted,
            path: "local.h".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths::default();
        let result = resolve_include(&directive, &search, dir.path());
        assert!(result.is_some());
        assert_eq!(result.unwrap(), normalize(&header));
    }

    #[test]
    fn resolve_quoted_in_iquote_dir() {
        let dir = TempDir::new().unwrap();
        let iquote_dir = dir.path().join("iquote");
        std::fs::create_dir(&iquote_dir).unwrap();
        let header = iquote_dir.join("q.h");
        std::fs::write(&header, "// header").unwrap();

        let directive = IncludeDirective {
            kind: IncludeKind::Quoted,
            path: "q.h".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths {
            iquote: vec![iquote_dir.into()],
            ..Default::default()
        };
        // Not in the including file's dir â€” should find via iquote.
        let other_dir = dir.path().join("other");
        std::fs::create_dir(&other_dir).unwrap();
        let result = resolve_include(&directive, &search, &other_dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), normalize(&header));
    }

    #[test]
    fn resolve_angle_bracket_in_user_dir() {
        let dir = TempDir::new().unwrap();
        let inc = dir.path().join("inc");
        std::fs::create_dir(&inc).unwrap();
        let header = inc.join("sys.h");
        std::fs::write(&header, "// header").unwrap();

        let directive = IncludeDirective {
            kind: IncludeKind::AngleBracket,
            path: "sys.h".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths {
            user: vec![inc.into()],
            ..Default::default()
        };
        let result = resolve_include(&directive, &search, dir.path());
        assert!(result.is_some());
    }

    #[test]
    fn resolve_angle_bracket_skips_iquote() {
        let dir = TempDir::new().unwrap();
        let iquote_dir = dir.path().join("iquote");
        std::fs::create_dir(&iquote_dir).unwrap();
        let header = iquote_dir.join("only_iquote.h");
        std::fs::write(&header, "// header").unwrap();

        let directive = IncludeDirective {
            kind: IncludeKind::AngleBracket,
            path: "only_iquote.h".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths {
            iquote: vec![iquote_dir.into()],
            ..Default::default()
        };
        let result = resolve_include(&directive, &search, dir.path());
        assert!(result.is_none(), "angle bracket should not search iquote");
    }

    #[test]
    fn resolve_unresolved_returns_none() {
        let directive = IncludeDirective {
            kind: IncludeKind::Quoted,
            path: "nonexistent.h".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths::default();
        let result = resolve_include(&directive, &search, Path::new("/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn resolve_computed_returns_none() {
        let directive = IncludeDirective {
            kind: IncludeKind::Computed("MACRO".to_string()),
            path: "MACRO".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths::default();
        let result = resolve_include(&directive, &search, Path::new("/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn resolve_search_order_user_before_system() {
        let dir = TempDir::new().unwrap();
        let user_dir = dir.path().join("user");
        let sys_dir = dir.path().join("sys");
        std::fs::create_dir(&user_dir).unwrap();
        std::fs::create_dir(&sys_dir).unwrap();

        let user_header = user_dir.join("shared.h");
        let sys_header = sys_dir.join("shared.h");
        std::fs::write(&user_header, "// user").unwrap();
        std::fs::write(&sys_header, "// system").unwrap();

        let directive = IncludeDirective {
            kind: IncludeKind::AngleBracket,
            path: "shared.h".to_string(),
            line: 1,
        };
        let search = IncludeSearchPaths {
            user: vec![user_dir.into()],
            system: vec![sys_dir.into()],
            ..Default::default()
        };
        let result = resolve_include(&directive, &search, dir.path()).unwrap();
        assert_eq!(result, normalize(&user_header));
    }

    // â”€â”€ scan_recursive tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn recursive_scan_finds_transitive_includes() {
        let dir = TempDir::new().unwrap();

        // main.c -> a.h -> b.h
        std::fs::write(dir.path().join("main.c"), "#include \"a.h\"\n").unwrap();
        std::fs::write(dir.path().join("a.h"), "#include \"b.h\"\n").unwrap();
        std::fs::write(dir.path().join("b.h"), "// leaf\n").unwrap();

        let search = IncludeSearchPaths::default();
        let result = scan_recursive(&dir.path().join("main.c"), &search);

        assert_eq!(result.resolved.len(), 2);
        assert!(result
            .resolved
            .contains(&normalize(&dir.path().join("a.h"))));
        assert!(result
            .resolved
            .contains(&normalize(&dir.path().join("b.h"))));
        assert!(result.unresolved.is_empty());
        assert!(!result.has_computed);
    }

    #[test]
    fn recursive_scan_handles_cycles() {
        let dir = TempDir::new().unwrap();

        // a.h -> b.h -> a.h (cycle)
        std::fs::write(dir.path().join("main.c"), "#include \"a.h\"\n").unwrap();
        std::fs::write(dir.path().join("a.h"), "#include \"b.h\"\n").unwrap();
        std::fs::write(dir.path().join("b.h"), "#include \"a.h\"\n").unwrap();

        let search = IncludeSearchPaths::default();
        let result = scan_recursive(&dir.path().join("main.c"), &search);

        // Should find both a.h and b.h without infinite loop.
        assert_eq!(result.resolved.len(), 2);
    }

    #[test]
    fn recursive_scan_records_unresolved() {
        let dir = TempDir::new().unwrap();

        std::fs::write(
            dir.path().join("main.c"),
            "#include \"exists.h\"\n#include <missing.h>\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("exists.h"), "// ok\n").unwrap();

        let search = IncludeSearchPaths::default();
        let result = scan_recursive(&dir.path().join("main.c"), &search);

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.unresolved, vec!["missing.h"]);
    }

    #[test]
    fn recursive_scan_detects_computed_includes() {
        let dir = TempDir::new().unwrap();

        std::fs::write(
            dir.path().join("main.c"),
            "#include PLATFORM_HEADER\n#include \"normal.h\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("normal.h"), "// ok\n").unwrap();

        let search = IncludeSearchPaths::default();
        let result = scan_recursive(&dir.path().join("main.c"), &search);

        assert!(result.has_computed);
        assert_eq!(result.resolved.len(), 1);
    }

    #[test]
    fn recursive_scan_deduplicates() {
        let dir = TempDir::new().unwrap();

        // main.c includes a.h and b.h, both include common.h
        std::fs::write(
            dir.path().join("main.c"),
            "#include \"a.h\"\n#include \"b.h\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("a.h"), "#include \"common.h\"\n").unwrap();
        std::fs::write(dir.path().join("b.h"), "#include \"common.h\"\n").unwrap();
        std::fs::write(dir.path().join("common.h"), "// shared\n").unwrap();

        let search = IncludeSearchPaths::default();
        let result = scan_recursive(&dir.path().join("main.c"), &search);

        // a.h, b.h, common.h â€” each once.
        assert_eq!(result.resolved.len(), 3);
    }

    #[test]
    fn recursive_scan_with_search_paths() {
        let dir = TempDir::new().unwrap();
        let inc = dir.path().join("inc");
        std::fs::create_dir(&inc).unwrap();

        std::fs::write(dir.path().join("main.c"), "#include <lib.h>\n").unwrap();
        std::fs::write(inc.join("lib.h"), "#include \"detail.h\"\n").unwrap();
        std::fs::write(inc.join("detail.h"), "// impl\n").unwrap();

        let search = IncludeSearchPaths {
            user: vec![inc.clone().into()],
            ..Default::default()
        };
        let result = scan_recursive(&dir.path().join("main.c"), &search);

        assert_eq!(result.resolved.len(), 2);
        assert!(result.resolved.contains(&normalize(&inc.join("lib.h"))));
        assert!(result.resolved.contains(&normalize(&inc.join("detail.h"))));
    }

    // â”€â”€ Helper function tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn join_continuations_merges_lines() {
        assert_eq!(join_continuations("a\\\nb"), "ab");
        assert_eq!(join_continuations("a\\\r\nb"), "ab");
    }

    #[test]
    fn join_continuations_preserves_normal_lines() {
        assert_eq!(join_continuations("a\nb"), "a\nb");
    }

    #[test]
    fn strip_comments_handles_line_comment() {
        let mut in_block = false;
        let result = strip_comments("code // comment", &mut in_block);
        assert_eq!(result, "code ");
        assert!(!in_block);
    }

    #[test]
    fn strip_comments_handles_block_comment() {
        let mut in_block = false;
        let result = strip_comments("before /* inside */ after", &mut in_block);
        assert_eq!(result, "before  after");
        assert!(!in_block);
    }

    #[test]
    fn strip_comments_handles_unterminated_block() {
        let mut in_block = false;
        let result = strip_comments("code /* start", &mut in_block);
        assert_eq!(result, "code ");
        assert!(in_block);
    }

    #[test]
    fn strip_comments_preserves_string_literal() {
        let mut in_block = false;
        let result = strip_comments(r#"x = "hello""#, &mut in_block);
        assert_eq!(result, r#"x = "hello""#);
    }
}
