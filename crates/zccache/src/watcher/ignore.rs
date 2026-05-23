//! Directory ignore filter for file watching.
//!
//! Filters out paths containing directories that should never be watched
//! (build output, VCS internals, dependency caches). Applied on the notify
//! callback thread to prevent these events from ever entering the channel.

use std::path::Path;

/// Filters filesystem paths against a set of ignored directory names.
///
/// Matches exact path components, not substrings. For example, the pattern
/// ".git" ignores `repo/.git/config` but NOT `repo/.github/workflows/ci.yml`.
#[derive(Debug, Clone)]
pub struct IgnoreFilter {
    patterns: Vec<String>,
}

impl IgnoreFilter {
    /// Create a filter with the given directory name patterns.
    #[must_use]
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }

    /// The default set of ignored directory names.
    #[must_use]
    pub fn default_patterns() -> Vec<String> {
        [
            ".git",
            "target",
            "node_modules",
            ".hg",
            "__pycache__",
            ".mypy_cache",
            "build",
            ".cache",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    /// Returns `true` if the path should be ignored.
    ///
    /// A path is ignored if any of its components exactly matches
    /// one of the configured patterns.
    #[must_use]
    pub fn should_ignore(&self, path: &Path) -> bool {
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name_str) = name.to_str() {
                    if self.patterns.iter().any(|p| p == name_str) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl Default for IgnoreFilter {
    fn default() -> Self {
        Self::new(Self::default_patterns())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_patterns_include_common_dirs() {
        let patterns = IgnoreFilter::default_patterns();
        assert!(patterns.contains(&".git".to_string()));
        assert!(patterns.contains(&"target".to_string()));
        assert!(patterns.contains(&"node_modules".to_string()));
    }

    #[test]
    fn ignores_path_with_git_component() {
        let filter = IgnoreFilter::default();
        assert!(filter.should_ignore(Path::new("repo/.git/config")));
        assert!(filter.should_ignore(Path::new("repo/.git/objects/abc")));
    }

    #[test]
    fn does_not_ignore_similar_names() {
        let filter = IgnoreFilter::default();
        // .github is NOT .git
        assert!(!filter.should_ignore(Path::new("repo/.github/workflows/ci.yml")));
        // target_util is NOT target
        assert!(!filter.should_ignore(Path::new("repo/target_util/main.c")));
    }

    #[test]
    fn does_not_ignore_normal_source_files() {
        let filter = IgnoreFilter::default();
        assert!(!filter.should_ignore(Path::new("src/main.rs")));
        assert!(!filter.should_ignore(Path::new("include/header.h")));
        assert!(!filter.should_ignore(Path::new("lib/utils.c")));
    }

    #[test]
    fn ignores_target_dir() {
        let filter = IgnoreFilter::default();
        assert!(filter.should_ignore(Path::new("project/target/debug/binary")));
        assert!(filter.should_ignore(Path::new("target/release/libfoo.so")));
    }

    #[test]
    fn ignores_node_modules() {
        let filter = IgnoreFilter::default();
        assert!(filter.should_ignore(Path::new("app/node_modules/pkg/index.js")));
    }

    #[test]
    fn custom_patterns_work() {
        let filter = IgnoreFilter::new(vec!["vendor".to_string(), "dist".to_string()]);
        assert!(filter.should_ignore(Path::new("project/vendor/lib.c")));
        assert!(filter.should_ignore(Path::new("project/dist/bundle.js")));
        assert!(!filter.should_ignore(Path::new("project/src/main.c")));
    }

    #[test]
    fn empty_filter_ignores_nothing() {
        let filter = IgnoreFilter::new(vec![]);
        assert!(!filter.should_ignore(Path::new(".git/config")));
        assert!(!filter.should_ignore(Path::new("target/debug/bin")));
    }

    #[test]
    fn root_path_not_ignored() {
        let filter = IgnoreFilter::default();
        assert!(!filter.should_ignore(Path::new("main.c")));
        assert!(!filter.should_ignore(Path::new(".")));
    }
}
