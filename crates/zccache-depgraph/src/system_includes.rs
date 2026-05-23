//! System include path discovery from compiler output.
//!
//! Parses the output of `<compiler> -v -E -x c++ /dev/null 2>&1` to extract
//! the compiler's default system include search paths. These paths are used
//! to resolve `#include <...>` directives that don't match any explicit
//! `-I`/`-isystem` paths.
//!
//! The discovery command differs by platform:
//! - Linux/macOS: `<compiler> -v -E -x c++ /dev/null 2>&1`
//! - Windows: `<compiler> -v -E -x c++ NUL 2>&1`
//!
//! The actual command execution is left to the caller (daemon). This module
//! only handles parsing the output and caching results.

use std::collections::HashMap;
use std::path::Path;

use zccache_monocrate::core::NormalizedPath;

/// Parse compiler `-v -E` output to extract system include paths.
///
/// Looks for the section between `#include <...> search starts here:`
/// and `End of search list.` in the compiler's stderr output.
///
/// Each line in that section is trimmed and treated as a directory path.
/// Lines starting with ` (framework directory)` are included but the
/// suffix is stripped.
#[must_use]
pub fn parse_system_include_output(output: &str) -> Vec<NormalizedPath> {
    let mut in_section = false;
    let mut paths = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed == "#include <...> search starts here:" {
            in_section = true;
            continue;
        }

        if trimmed == "End of search list." {
            break;
        }

        if in_section && !trimmed.is_empty() {
            // Some compilers annotate framework dirs: "/path (framework directory)"
            let path_str = if let Some(stripped) = trimmed.strip_suffix(" (framework directory)") {
                stripped
            } else {
                trimmed
            };

            if !path_str.is_empty() {
                paths.push(path_str.into());
            }
        }
    }

    paths
}

/// Build the compiler discovery command arguments.
///
/// Returns the arguments to pass to the compiler to discover system include
/// paths. The caller should execute the compiler with these args and capture
/// stderr.
#[must_use]
pub fn discovery_args() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["-v", "-E", "-x", "c++", "NUL"]
    } else {
        vec!["-v", "-E", "-x", "c++", "/dev/null"]
    }
}

/// Cache of discovered system include paths, keyed by compiler path.
///
/// Avoids re-running the compiler discovery command for the same compiler
/// across sessions.
#[derive(Debug, Default)]
pub struct SystemIncludeCache {
    cache: HashMap<NormalizedPath, Vec<NormalizedPath>>,
}

impl SystemIncludeCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up cached system include paths for a compiler.
    #[must_use]
    pub fn get(&self, compiler: &Path) -> Option<&[NormalizedPath]> {
        let compiler = NormalizedPath::new(compiler);
        self.cache.get(&compiler).map(Vec::as_slice)
    }

    /// Store discovered system include paths for a compiler.
    pub fn insert(&mut self, compiler: NormalizedPath, paths: Vec<NormalizedPath>) {
        self.cache.insert(compiler, paths);
    }

    /// Get cached paths or discover them using the provided closure.
    ///
    /// The closure receives the compiler path and should execute the
    /// discovery command and return parsed paths.
    pub fn get_or_discover<F>(&mut self, compiler: &Path, discover: F) -> &[NormalizedPath]
    where
        F: FnOnce(&Path) -> Vec<NormalizedPath>,
    {
        let compiler_key = NormalizedPath::new(compiler);
        if !self.cache.contains_key(&compiler_key) {
            let paths = discover(compiler);
            self.cache.insert(compiler_key.clone(), paths);
        }
        self.cache.get(&compiler_key).map(Vec::as_slice).unwrap()
    }

    /// Remove all cached entries.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gcc_output() {
        let output = r#"Using built-in specs.
COLLECT_GCC=g++
Target: x86_64-linux-gnu
#include "..." search starts here:
#include <...> search starts here:
 /usr/lib/gcc/x86_64-linux-gnu/11/include
 /usr/local/include
 /usr/include/x86_64-linux-gnu
 /usr/include
End of search list.
# 1 "/dev/null"
"#;

        let paths = parse_system_include_output(output);
        assert_eq!(
            paths,
            vec![
                NormalizedPath::from("/usr/lib/gcc/x86_64-linux-gnu/11/include"),
                NormalizedPath::from("/usr/local/include"),
                NormalizedPath::from("/usr/include/x86_64-linux-gnu"),
                NormalizedPath::from("/usr/include"),
            ]
        );
    }

    #[test]
    fn parse_clang_output() {
        let output = r#"clang version 14.0.0
Target: x86_64-pc-linux-gnu
#include "..." search starts here:
#include <...> search starts here:
 /usr/lib/clang/14.0.0/include
 /usr/local/include
 /usr/include
End of search list.
"#;

        let paths = parse_system_include_output(output);
        assert_eq!(
            paths,
            vec![
                NormalizedPath::from("/usr/lib/clang/14.0.0/include"),
                NormalizedPath::from("/usr/local/include"),
                NormalizedPath::from("/usr/include"),
            ]
        );
    }

    #[test]
    fn parse_macos_with_framework_dirs() {
        let output = r#"Apple clang version 14.0.0
#include "..." search starts here:
#include <...> search starts here:
 /usr/local/include
 /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include
 /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks (framework directory)
End of search list.
"#;

        let paths = parse_system_include_output(output);
        assert_eq!(
            paths,
            vec![
                NormalizedPath::from("/usr/local/include"),
                NormalizedPath::from(
                    "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include",
                ),
                NormalizedPath::from(
                    "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks",
                ),
            ]
        );
    }

    #[test]
    fn parse_empty_output() {
        let paths = parse_system_include_output("");
        assert!(paths.is_empty());
    }

    #[test]
    fn parse_no_section_marker() {
        let output = "some random compiler output\nwithout the expected markers\n";
        let paths = parse_system_include_output(output);
        assert!(paths.is_empty());
    }

    #[test]
    fn parse_quoted_section_ignored() {
        // Paths in the "..." section should NOT be included.
        let output = r#"#include "..." search starts here:
 /project/include
#include <...> search starts here:
 /usr/include
End of search list.
"#;
        let paths = parse_system_include_output(output);
        assert_eq!(paths, vec![NormalizedPath::from("/usr/include")]);
    }

    #[test]
    fn cache_get_returns_none_for_unknown() {
        let cache = SystemIncludeCache::new();
        assert!(cache.get(Path::new("/usr/bin/gcc")).is_none());
    }

    #[test]
    fn cache_insert_and_get() {
        let mut cache = SystemIncludeCache::new();
        cache.insert(
            "/usr/bin/gcc".into(),
            vec![NormalizedPath::from("/usr/include")],
        );
        let paths = cache.get(Path::new("/usr/bin/gcc")).unwrap();
        assert_eq!(paths, &[NormalizedPath::from("/usr/include")]);
    }

    #[test]
    fn cache_get_or_discover_caches() {
        let mut cache = SystemIncludeCache::new();
        let mut call_count = 0u32;

        // First call should invoke the closure.
        let paths = cache.get_or_discover(Path::new("/usr/bin/g++"), |_| {
            call_count += 1;
            vec![NormalizedPath::from("/usr/include")]
        });
        assert_eq!(paths, &[NormalizedPath::from("/usr/include")]);
        assert_eq!(call_count, 1);

        // Second call should use cache â€” but we can't capture the same
        // mutable reference, so verify via len.
        assert_eq!(cache.len(), 1);
        assert!(cache.get(Path::new("/usr/bin/g++")).is_some());
    }

    #[test]
    fn cache_different_compilers() {
        let mut cache = SystemIncludeCache::new();
        cache.insert(
            "/usr/bin/gcc".into(),
            vec![NormalizedPath::from("/gcc/include")],
        );
        cache.insert(
            "/usr/bin/clang".into(),
            vec![NormalizedPath::from("/clang/include")],
        );
        assert_eq!(cache.len(), 2);
        assert_ne!(
            cache.get(Path::new("/usr/bin/gcc")),
            cache.get(Path::new("/usr/bin/clang"))
        );
    }

    #[test]
    fn discovery_args_returns_nonempty() {
        let args = discovery_args();
        assert!(args.len() >= 4);
        assert!(args.contains(&"-v"));
        assert!(args.contains(&"-E"));
    }
}
