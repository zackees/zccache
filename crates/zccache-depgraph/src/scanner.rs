//! `#include` directive scanner.
//!
//! Scans C/C++ source files for `#include` directives, skipping comments
//! and string literals. Does not evaluate preprocessor conditionals â€”
//! all `#include` directives are returned unconditionally.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use super::search_paths::IncludeSearchPaths;

mod lex;
#[cfg(test)]
mod lex_tests;
use dashmap::DashMap;
use zccache_core::NormalizedPath;

/// The kind of `#include` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeKind {
    /// `#include "foo.h"` â€” quoted include.
    Quoted,
    /// `#include <foo.h>` â€” angle-bracket include.
    AngleBracket,
    /// A quoted `#include_next` directive.
    QuotedNext,
    /// An angle-bracket `#include_next` directive.
    AngleBracketNext,
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

/// Memo for parsed source/header directives, safe to keep across requests.
///
/// A multi-source compile commonly gives every unit the same standard-library
/// graph. Sharing this memo preserves each unit's independent recursive result
/// while avoiding repeated reads and parsing of those common headers.
///
/// Each entry is keyed by path and validated against the file's current size
/// and mtime, so a long-lived (daemon-wide) cache re-parses a file after it is
/// modified. Read failures are never cached.
#[derive(Default)]
pub struct RecursiveScanCache {
    directives: DashMap<NormalizedPath, CachedDirectives>,
    /// Number of cache-miss parses performed through this cache.
    parses: std::sync::atomic::AtomicU64,
}

/// A parsed-directive entry plus the file stat it was parsed from.
struct CachedDirectives {
    size: u64,
    mtime: Option<SystemTime>,
    directives: Arc<Vec<IncludeDirective>>,
}

impl RecursiveScanCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cached files.
    pub fn len(&self) -> usize {
        self.directives.len()
    }

    /// True if no files are cached.
    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }

    /// Drop every cached entry.
    pub fn clear(&self) {
        self.directives.clear();
    }

    /// Return the file's directives, re-parsing only if its stat changed.
    ///
    /// Returns `None` if the file cannot be read (any stale entry is removed).
    fn get_or_scan(&self, file: &Path) -> Option<Arc<Vec<IncludeDirective>>> {
        let key = NormalizedPath::from(file);
        let meta = match std::fs::metadata(file) {
            Ok(meta) => meta,
            Err(_) => {
                self.directives.remove(&key);
                return None;
            }
        };
        let size = meta.len();
        let mtime = meta.modified().ok();
        if let Some(entry) = self.directives.get(&key) {
            if entry.size == size && entry.mtime == mtime {
                return Some(Arc::clone(&entry.directives));
            }
        }
        match scan_includes(file) {
            Ok(directives) => {
                self.parses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let directives = Arc::new(directives);
                self.directives.insert(
                    key,
                    CachedDirectives {
                        size,
                        mtime,
                        directives: Arc::clone(&directives),
                    },
                );
                Some(directives)
            }
            Err(_) => {
                self.directives.remove(&key);
                None
            }
        }
    }
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

/// Process-wide count of `scan_includes_str` calls (test instrumentation).
///
/// Global rather than thread-local because `scan_recursive` fans file scans
/// out to rayon worker threads. Callers must compare deltas, and only in
/// tests that do not run other scans concurrently.
static SCAN_INCLUDES_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Total `scan_includes_str` calls in this process (all threads).
#[doc(hidden)]
pub fn scan_includes_calls() -> u64 {
    SCAN_INCLUDES_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Reset the process-wide `scan_includes_str` call counter to zero.
#[doc(hidden)]
pub fn reset_scan_includes_calls() {
    SCAN_INCLUDES_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Scan a source string for `#include` directives.
///
/// Skips directives inside `//` line comments and `/* */` block comments,
/// and only recognizes `#` as the first significant byte of a line (so an
/// `#include` inside a string literal is ignored). Handles backslash line
/// continuations. Single pass, no whole-file allocation (zccache#1670).
pub fn scan_includes_str(source: &str) -> Vec<IncludeDirective> {
    SCAN_INCLUDES_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    lex::scan_directives(source)
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
        IncludeKind::QuotedNext => resolve_include_next(
            &directive.path,
            search.quoted_search_dirs(),
            including_file_dir,
        ),
        IncludeKind::AngleBracketNext => resolve_include_next(
            &directive.path,
            search.angle_search_dirs(),
            including_file_dir,
        ),
        IncludeKind::Computed(_) => None,
    }
}

fn resolve_include_next<'a>(
    path: &str,
    search_dirs: impl Iterator<Item = &'a Path>,
    including_file_dir: &Path,
) -> Option<NormalizedPath> {
    let including_dir = try_normalize(including_file_dir)?;
    let dirs: Vec<&Path> = search_dirs.collect();
    let current_root = dirs
        .iter()
        .enumerate()
        .filter_map(|(index, dir)| {
            let normalized = try_normalize(dir)?;
            including_dir
                .starts_with(normalized.as_path())
                .then(|| (index, normalized.as_path().components().count()))
        })
        .fold(None, |best, candidate| match best {
            Some((_, best_depth)) if best_depth >= candidate.1 => best,
            _ => Some(candidate),
        })
        .map(|(index, _)| index);
    let start = current_root.map_or(0, |index| index + 1);
    dirs.into_iter().skip(start).find_map(|dir| {
        let candidate = dir.join(path);
        candidate.is_file().then(|| normalize(&candidate))
    })
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
    scan_recursive_impl(source, search, None, &|_, _, _| true)
}

/// Recursively scan with a request-scoped parsed-directive memo.
pub fn scan_recursive_cached(
    source: &Path,
    search: &IncludeSearchPaths,
    cache: &RecursiveScanCache,
) -> ScanResult {
    scan_recursive_impl(source, search, Some(cache), &|_, _, _| true)
}

/// Recursively scan with a request-scoped memo, pruning rejected headers.
///
/// A rejected header is neither returned nor scanned for transitive includes.
/// This is useful when a caller intentionally excludes an entire include-root
/// class and wants to avoid paying to read and parse that subtree.
pub fn scan_recursive_cached_pruned(
    source: &Path,
    search: &IncludeSearchPaths,
    cache: &RecursiveScanCache,
    retain: &(dyn Fn(&Path, &IncludeDirective, &NormalizedPath) -> bool + Sync),
) -> ScanResult {
    scan_recursive_impl(source, search, Some(cache), retain)
}

fn scan_recursive_impl(
    source: &Path,
    search: &IncludeSearchPaths,
    cache: Option<&RecursiveScanCache>,
    retain: &(dyn Fn(&Path, &IncludeDirective, &NormalizedPath) -> bool + Sync),
) -> ScanResult {
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
                    cache,
                    retain,
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
#[allow(clippy::too_many_arguments)] // One internal frontier worker over shared scan state.
fn scan_one_level(
    file: &Path,
    search: &IncludeSearchPaths,
    visited: &dashmap::DashSet<NormalizedPath>,
    resolved: &std::sync::Mutex<Vec<NormalizedPath>>,
    unresolved: &std::sync::Mutex<Vec<String>>,
    has_computed: &std::sync::atomic::AtomicBool,
    cache: Option<&RecursiveScanCache>,
    retain: &(dyn Fn(&Path, &IncludeDirective, &NormalizedPath) -> bool + Sync),
) -> Vec<NormalizedPath> {
    let directives = if let Some(cache) = cache {
        match cache.get_or_scan(file) {
            Some(directives) => directives,
            None => return Vec::new(),
        }
    } else {
        match scan_includes(file) {
            Ok(directives) => Arc::new(directives),
            Err(_) => return Vec::new(),
        }
    };

    let file_dir = file.parent().unwrap_or(Path::new("."));

    let mut new_for_next: Vec<NormalizedPath> = Vec::new();
    let mut local_resolved: Vec<NormalizedPath> = Vec::new();
    let mut local_unresolved: Vec<String> = Vec::new();
    let mut saw_computed = false;

    for directive in directives.iter() {
        match &directive.kind {
            IncludeKind::Computed(_) => {
                saw_computed = true;
            }
            _ => {
                if let Some(abs_path) = resolve_include(directive, search, file_dir) {
                    if retain(file, directive, &abs_path) && visited.insert(abs_path.clone()) {
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

/// Parse an `#include` directive from a (comment-stripped) line.
pub(super) fn parse_include_from_line(line: &str) -> Option<IncludeDirective> {
    let trimmed = line.trim();

    // Must start with #
    let after_hash = trimmed.strip_prefix('#')?;
    let after_hash = after_hash.trim();

    let (after_include, include_next) = if let Some(rest) = after_hash.strip_prefix("include_next")
    {
        (rest, true)
    } else {
        (after_hash.strip_prefix("include")?, false)
    };

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
            kind: if include_next {
                IncludeKind::QuotedNext
            } else {
                IncludeKind::Quoted
            },
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
            kind: if include_next {
                IncludeKind::AngleBracketNext
            } else {
                IncludeKind::AngleBracket
            },
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
    Some(NormalizedPath::new(
        zccache_core::path::strip_verbatim_prefix(&p),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scan_includes_str_increments_call_counter() {
        // Other tests may scan concurrently, so assert a lower bound on the delta.
        let before = scan_includes_calls();
        scan_includes_str("#include <a.h>\n");
        scan_includes_str("#include <a.h>\n");
        assert!(scan_includes_calls() - before >= 2);
    }

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
    fn parse_include_next_directives() {
        let includes = scan_includes_str("#include_next <system.h>\n#include_next \"quoted.h\"\n");
        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0].kind, IncludeKind::AngleBracketNext);
        assert_eq!(includes[0].path, "system.h");
        assert_eq!(includes[1].kind, IncludeKind::QuotedNext);
        assert_eq!(includes[1].path, "quoted.h");
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
    fn resolve_include_next_skips_current_search_root() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("same.h"), "// first").unwrap();
        std::fs::write(second.join("same.h"), "// second").unwrap();
        let directive = IncludeDirective {
            kind: IncludeKind::AngleBracketNext,
            path: "same.h".into(),
            line: 1,
        };
        let search = IncludeSearchPaths {
            system: vec![first.clone().into(), second.clone().into()],
            ..Default::default()
        };

        let result = resolve_include(&directive, &search, &first).unwrap();

        assert_eq!(result, normalize(&second.join("same.h")));
    }

    #[test]
    fn resolve_include_next_uses_most_specific_nested_search_root() {
        let dir = TempDir::new().unwrap();
        let outer = dir.path().join("include");
        let inner = outer.join("cxx");
        let next = dir.path().join("next");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(&next).unwrap();
        std::fs::write(inner.join("same.h"), "// inner").unwrap();
        std::fs::write(next.join("same.h"), "// next").unwrap();
        let directive = IncludeDirective {
            kind: IncludeKind::AngleBracketNext,
            path: "same.h".into(),
            line: 1,
        };
        let search = IncludeSearchPaths {
            system: vec![outer.into(), inner.clone().into(), next.clone().into()],
            ..Default::default()
        };

        let result = resolve_include(&directive, &search, &inner).unwrap();

        assert_eq!(result, normalize(&next.join("same.h")));
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
    fn recursive_scan_cache_shares_parsing_but_preserves_per_source_results() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("one.c"), "#include \"shared.h\"\n").unwrap();
        std::fs::write(dir.path().join("two.c"), "#include \"shared.h\"\n").unwrap();
        std::fs::write(dir.path().join("shared.h"), "#include \"nested.h\"\n").unwrap();
        std::fs::write(dir.path().join("nested.h"), "// leaf\n").unwrap();
        let cache = RecursiveScanCache::default();
        let search = IncludeSearchPaths::default();

        let one = scan_recursive_cached(&dir.path().join("one.c"), &search, &cache);
        let two = scan_recursive_cached(&dir.path().join("two.c"), &search, &cache);

        assert_eq!(one.resolved.len(), 2);
        assert_eq!(two.resolved.len(), 2);
        assert_eq!(cache.len(), 4);
    }

    fn parses(cache: &RecursiveScanCache) -> u64 {
        cache.parses.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn recursive_scan_cache_parses_shared_headers_once_across_tus() {
        const N: usize = 8;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.h"), "#include \"b.h\"\n").unwrap();
        std::fs::write(dir.path().join("b.h"), "#include \"c.h\"\n").unwrap();
        std::fs::write(dir.path().join("c.h"), "// leaf\n").unwrap();
        let sources: Vec<_> = (0..N)
            .map(|i| {
                let path = dir.path().join(format!("src{i}.c"));
                std::fs::write(&path, "#include \"a.h\"\n").unwrap();
                path
            })
            .collect();
        let search = IncludeSearchPaths::default();
        let cache = RecursiveScanCache::new();

        // The process-wide counter is shared with concurrently running tests,
        // so only a lower bound is meaningful; the per-cache count is exact.
        reset_scan_includes_calls();
        let before = scan_includes_calls();
        for source in &sources {
            let cached = scan_recursive_cached(source, &search, &cache);
            let uncached = scan_recursive(source, &search);
            let mut cached_resolved = cached.resolved.clone();
            let mut uncached_resolved = uncached.resolved.clone();
            cached_resolved.sort();
            uncached_resolved.sort();
            assert_eq!(cached_resolved, uncached_resolved);
            assert_eq!(cached.unresolved, uncached.unresolved);
            assert_eq!(cached.has_computed, uncached.has_computed);
            assert_eq!(cached.resolved.len(), 3);
        }
        assert!(scan_includes_calls() - before >= (N + 3) as u64);
        assert_eq!(parses(&cache), (N + 3) as u64);
        assert_eq!(cache.len(), N + 3);
        assert!(!cache.is_empty());

        // A second pass over unchanged files costs no parses at all.
        for source in &sources {
            scan_recursive_cached(source, &search, &cache);
        }
        assert_eq!(parses(&cache), (N + 3) as u64);

        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn recursive_scan_cache_rescans_header_after_modification() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("main.c");
        std::fs::write(&main, "#include \"a.h\"\n").unwrap();
        std::fs::write(dir.path().join("a.h"), "#include \"b.h\"\n").unwrap();
        std::fs::write(dir.path().join("b.h"), "// leaf\n").unwrap();
        std::fs::write(dir.path().join("d.h"), "// new leaf\n").unwrap();
        let search = IncludeSearchPaths::default();
        let cache = RecursiveScanCache::new();

        let first = scan_recursive_cached(&main, &search, &cache);
        assert_eq!(first.resolved.len(), 2);

        // Different length guarantees a stat mismatch even on coarse-mtime
        // filesystems.
        std::fs::write(
            dir.path().join("b.h"),
            "// leaf, now with an extra include\n#include \"d.h\"\n",
        )
        .unwrap();

        let second = scan_recursive_cached(&main, &search, &cache);
        let d = normalize(&dir.path().join("d.h"));
        assert!(
            second.resolved.contains(&d),
            "d.h missing after b.h modification: {:?}",
            second.resolved
        );
        assert_eq!(second.resolved.len(), 3);
    }

    #[test]
    fn recursive_scan_prunes_rejected_header_subtrees() {
        let dir = TempDir::new().unwrap();
        let user_dir = dir.path().join("user");
        let system_dir = dir.path().join("system");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(
            dir.path().join("main.c"),
            "#include <user.h>\n#include <system.h>\n",
        )
        .unwrap();
        std::fs::write(user_dir.join("user.h"), "// user\n").unwrap();
        std::fs::write(system_dir.join("system.h"), "#include SYSTEM_COMPUTED\n").unwrap();
        let search = IncludeSearchPaths {
            user: vec![user_dir.clone().into()],
            system: vec![system_dir.clone().into()],
            ..Default::default()
        }
        .canonicalized();
        let user_dir = std::fs::canonicalize(user_dir).unwrap();
        let cache = RecursiveScanCache::default();

        let result = scan_recursive_cached_pruned(
            &dir.path().join("main.c"),
            &search,
            &cache,
            &|_, _, path| path.starts_with(&user_dir),
        );

        assert_eq!(result.resolved, vec![normalize(&user_dir.join("user.h"))]);
        assert!(!result.has_computed);
        assert_eq!(cache.len(), 2);
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
}
