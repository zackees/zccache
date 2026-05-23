//! Watcher integration support for the dependency graph.
//!
//! Provides:
//! - `WatchSet` â€” tracks which directories should be watched and which
//!   filenames within them are relevant.
//! - Shadow detection â€” identifies when a newly created file in a
//!   higher-priority include directory would shadow an existing resolved
//!   include.
//! - Unresolved include resolution â€” identifies when a newly created file
//!   matches a previously unresolved `#include`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::context::ContextKey;
use super::graph::DepGraph;
use zccache_monocrate::core::NormalizedPath;

/// Set of directories that should be watched, with tracked filenames per directory.
///
/// The watcher layer uses this to decide which directories to register with
/// the OS file-watcher (non-recursive) and which events to filter for.
#[derive(Debug, Clone, Default)]
pub struct WatchSet {
    /// Maps directory path â†’ set of tracked file names within it.
    dirs: HashMap<NormalizedPath, HashSet<String>>,
}

fn normalize_watch_filename(name: &std::ffi::OsStr) -> String {
    #[cfg(windows)]
    {
        name.to_string_lossy().to_ascii_lowercase()
    }

    #[cfg(not(windows))]
    {
        name.to_string_lossy().into_owned()
    }
}

impl WatchSet {
    /// Create an empty watch set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a watch set from a list of absolute file paths.
    /// Each file's parent directory is added, with the filename tracked.
    #[must_use]
    pub fn from_paths(paths: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        let mut dirs: HashMap<NormalizedPath, HashSet<String>> = HashMap::new();
        for path in paths {
            let path = path.as_ref();
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                dirs.entry(parent.to_path_buf().into())
                    .or_default()
                    .insert(normalize_watch_filename(name));
            }
        }
        Self { dirs }
    }

    /// Add a directory to watch (even if it has no tracked files yet).
    /// Used for include search directories where new files might appear.
    pub fn add_dir(&mut self, dir: NormalizedPath) {
        self.dirs.entry(dir).or_default();
    }

    /// Add a specific file path to the watch set.
    pub fn add_path(&mut self, path: &Path) {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            self.dirs
                .entry(parent.to_path_buf().into())
                .or_default()
                .insert(normalize_watch_filename(name));
        }
    }

    /// Get all directories that need to be watched.
    pub fn dirs(&self) -> impl Iterator<Item = &NormalizedPath> {
        self.dirs.keys()
    }

    /// Check if a path is in the tracked file set.
    #[must_use]
    pub fn is_tracked(&self, path: &Path) -> bool {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            let parent = NormalizedPath::new(parent);
            self.dirs
                .get(&parent)
                .is_some_and(|names| names.contains(&normalize_watch_filename(name)))
        } else {
            false
        }
    }

    /// Check if a directory is in the watch set.
    #[must_use]
    pub fn is_watched(&self, dir: &Path) -> bool {
        self.dirs.contains_key(&NormalizedPath::new(dir))
    }

    /// Number of watched directories.
    #[must_use]
    pub fn dir_count(&self) -> usize {
        self.dirs.len()
    }

    /// Total number of tracked files across all directories.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.dirs.values().map(HashSet::len).sum()
    }

    /// Directories in `self` that are not in `previous`.
    /// These are newly added directories that need watch registration.
    #[must_use]
    pub fn new_dirs_vs(&self, previous: &WatchSet) -> Vec<NormalizedPath> {
        self.dirs
            .keys()
            .filter(|d| !previous.dirs.contains_key(*d))
            .cloned()
            .collect()
    }

    /// Directories in `previous` that are not in `self`.
    /// These are removed directories whose watches can be dropped.
    #[must_use]
    pub fn removed_dirs_vs(&self, previous: &WatchSet) -> Vec<NormalizedPath> {
        previous
            .dirs
            .keys()
            .filter(|d| !self.dirs.contains_key(*d))
            .cloned()
            .collect()
    }
}

/// Check if `dir_a` appears before `dir_b` in the given search path order.
///
/// Returns `true` if `dir_a` has higher priority (appears earlier) than `dir_b`.
/// Returns `false` if either directory is not in the search paths or they are equal.
fn is_higher_priority(
    dir_a: &Path,
    dir_b: &Path,
    search: &crate::search_paths::IncludeSearchPaths,
) -> bool {
    let all_dirs: Vec<&Path> = search.all_search_dirs().collect();

    let pos_a = all_dirs.iter().position(|d| *d == dir_a);
    let pos_b = all_dirs.iter().position(|d| *d == dir_b);

    match (pos_a, pos_b) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

impl DepGraph {
    /// Compute the set of directories that should be watched.
    ///
    /// Includes:
    /// - Parent directories of all resolved include paths (to detect modifications)
    /// - Parent directories of source files (to detect source changes)
    /// - All include search directories from all contexts (to detect new files)
    #[must_use]
    pub fn watch_set(&self) -> WatchSet {
        let mut ws = WatchSet::new();

        for entry in self.contexts_iter() {
            let ctx_entry = entry.value();

            // Source file parent dir.
            ws.add_path(&ctx_entry.context.source_file);

            // All resolved include parent dirs.
            for inc in &ctx_entry.resolved_includes {
                ws.add_path(inc);
            }

            // All include search dirs (for new-file detection).
            for dir in ctx_entry.context.include_search.all_search_dirs() {
                ws.add_dir(dir.into());
            }
        }

        ws
    }

    /// Check if a newly created file shadows any existing resolved include
    /// in any context. Returns context keys that should be marked stale.
    ///
    /// A shadow occurs when `new_file` has the same filename as an existing
    /// resolved include, and `new_file`'s directory appears earlier (higher
    /// priority) in that context's include search path.
    #[must_use]
    pub fn check_shadow(&self, new_file: &Path) -> Vec<ContextKey> {
        let new_name = match new_file.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return Vec::new(),
        };
        let new_dir = match new_file.parent() {
            Some(d) => d,
            None => return Vec::new(),
        };

        let mut affected = Vec::new();

        for entry in self.contexts_iter() {
            let ctx_entry = entry.value();
            let search = &ctx_entry.context.include_search;

            for resolved_path in &ctx_entry.resolved_includes {
                let resolved_name = match resolved_path.file_name() {
                    Some(n) => n.to_string_lossy(),
                    None => continue,
                };

                if *resolved_name != new_name {
                    continue;
                }

                let resolved_dir = match resolved_path.parent() {
                    Some(d) => d,
                    None => continue,
                };

                // Same directory â€” not a shadow, just a replacement (handled
                // by the watcher's Modified event).
                if resolved_dir == new_dir {
                    continue;
                }

                if is_higher_priority(new_dir, resolved_dir, search) {
                    affected.push(*entry.key());
                    break; // Context already affected, move to next.
                }
            }
        }

        affected
    }

    /// Check if a newly created file resolves any previously unresolved
    /// `#include` in any context. Returns affected context keys.
    #[must_use]
    pub fn check_new_resolve(&self, new_file: &Path) -> Vec<ContextKey> {
        let new_name = match new_file.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return Vec::new(),
        };

        let mut affected = Vec::new();

        for entry in self.contexts_iter() {
            let ctx_entry = entry.value();

            for unresolved in &ctx_entry.unresolved_includes {
                // Unresolved includes may be bare names ("foo.h") or paths
                // ("path/to/foo.h"). Compare against the filename.
                let unresolved_name = Path::new(unresolved)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();

                if unresolved_name == new_name {
                    affected.push(*entry.key());
                    break;
                }
            }
        }

        affected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zccache_monocrate::core::NormalizedPath;

    use super::context::CompileContext;
    use super::scanner::ScanResult;
    use super::search_paths::IncludeSearchPaths;
    use zccache_monocrate::hash::ContentHash;

    fn dummy_hash(path: &Path) -> Option<ContentHash> {
        Some(zccache_monocrate::hash::hash_bytes(path.to_string_lossy().as_bytes()))
    }

    fn make_ctx_with_search(source: &str, search: IncludeSearchPaths) -> CompileContext {
        CompileContext {
            source_file: NormalizedPath::from(source),
            include_search: search,
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    // --- WatchSet tests ---

    #[test]
    fn watch_set_from_paths_groups_by_dir() {
        let ws = WatchSet::from_paths([
            NormalizedPath::from("/inc/a.h"),
            NormalizedPath::from("/inc/b.h"),
            NormalizedPath::from("/src/main.c"),
        ]);
        assert_eq!(ws.dir_count(), 2);
        assert_eq!(ws.file_count(), 3);
        assert!(ws.is_watched(Path::new("/inc")));
        assert!(ws.is_watched(Path::new("/src")));
    }

    #[test]
    fn watch_set_deduplication() {
        let ws = WatchSet::from_paths([
            NormalizedPath::from("/inc/a.h"),
            NormalizedPath::from("/inc/a.h"), // duplicate
        ]);
        assert_eq!(ws.dir_count(), 1);
        assert_eq!(ws.file_count(), 1);
    }

    #[test]
    fn watch_set_is_tracked() {
        let ws = WatchSet::from_paths([NormalizedPath::from("/inc/a.h")]);
        assert!(ws.is_tracked(Path::new("/inc/a.h")));
        assert!(!ws.is_tracked(Path::new("/inc/b.h")));
        assert!(!ws.is_tracked(Path::new("/other/a.h")));
    }

    #[cfg(windows)]
    #[test]
    fn watch_set_is_tracked_ignores_filename_case_on_windows() {
        let ws = WatchSet::from_paths([NormalizedPath::from(r"C:\inc\Config.h")]);
        assert!(ws.is_tracked(Path::new(r"C:\inc\config.h")));
        assert!(ws.is_tracked(Path::new(r"C:\inc\CONFIG.H")));
    }

    #[test]
    fn watch_set_add_dir_empty() {
        let mut ws = WatchSet::new();
        ws.add_dir(NormalizedPath::from("/usr/include"));
        assert!(ws.is_watched(Path::new("/usr/include")));
        assert_eq!(ws.file_count(), 0);
        assert_eq!(ws.dir_count(), 1);
    }

    #[test]
    fn watch_set_add_path() {
        let mut ws = WatchSet::new();
        ws.add_path(Path::new("/inc/foo.h"));
        assert!(ws.is_tracked(Path::new("/inc/foo.h")));
        assert!(ws.is_watched(Path::new("/inc")));
    }

    #[test]
    fn watch_set_new_dirs_vs() {
        let old = WatchSet::from_paths([NormalizedPath::from("/inc/a.h")]);
        let new = WatchSet::from_paths([
            NormalizedPath::from("/inc/a.h"),
            NormalizedPath::from("/new/b.h"),
        ]);
        let added = new.new_dirs_vs(&old);
        assert_eq!(added, vec![NormalizedPath::from("/new")]);
    }

    #[test]
    fn watch_set_removed_dirs_vs() {
        let old = WatchSet::from_paths([
            NormalizedPath::from("/inc/a.h"),
            NormalizedPath::from("/old/b.h"),
        ]);
        let new = WatchSet::from_paths([NormalizedPath::from("/inc/a.h")]);
        let removed = new.removed_dirs_vs(&old);
        assert_eq!(removed, vec![NormalizedPath::from("/old")]);
    }

    // --- DepGraph::watch_set() tests ---

    #[test]
    fn watch_set_includes_source_and_headers() {
        let graph = DepGraph::new();
        let ctx = make_ctx_with_search("/src/main.c", IncludeSearchPaths::default());
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: vec![
                NormalizedPath::from("/inc/a.h"),
                NormalizedPath::from("/inc/b.h"),
            ],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        let ws = graph.watch_set();
        assert!(ws.is_tracked(Path::new("/src/main.c")));
        assert!(ws.is_tracked(Path::new("/inc/a.h")));
        assert!(ws.is_tracked(Path::new("/inc/b.h")));
    }

    #[test]
    fn watch_set_includes_search_dirs() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/project/include")],
            system: vec![NormalizedPath::from("/usr/include")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        graph.register(ctx);

        let ws = graph.watch_set();
        // Search dirs are watched even if no files resolve there yet.
        assert!(ws.is_watched(Path::new("/project/include")));
        assert!(ws.is_watched(Path::new("/usr/include")));
    }

    #[test]
    fn watch_set_dedupes_across_contexts() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/inc")],
            ..Default::default()
        };

        let ctx1 = make_ctx_with_search("/src/a.c", search.clone());
        let key1 = graph.register(ctx1);
        let ctx2 = make_ctx_with_search("/src/b.c", search);
        let key2 = graph.register(ctx2);

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/common.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key1, scan.clone(), dummy_hash);
        graph.update(&key2, scan, dummy_hash);

        let ws = graph.watch_set();
        // /inc should appear once, not twice.
        let inc_count = ws
            .dirs()
            .filter(|d| d.as_path() == Path::new("/inc"))
            .count();
        assert_eq!(inc_count, 1);
    }

    // --- Shadow detection tests ---

    #[test]
    fn check_shadow_detects_higher_priority() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/high"), NormalizedPath::from("/low")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        let key = graph.register(ctx);

        // foo.h currently resolves from /low.
        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/low/foo.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // New foo.h appears in /high (higher priority).
        let affected = graph.check_shadow(Path::new("/high/foo.h"));
        assert_eq!(affected, vec![key]);
    }

    #[test]
    fn check_shadow_no_false_positive_lower_priority() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/high"), NormalizedPath::from("/low")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        let key = graph.register(ctx);

        // foo.h already resolves from /high.
        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/high/foo.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // New foo.h appears in /low (lower priority) â€” NOT a shadow.
        let affected = graph.check_shadow(Path::new("/low/foo.h"));
        assert!(affected.is_empty());
    }

    #[test]
    fn check_shadow_different_filename_no_match() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/high"), NormalizedPath::from("/low")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/low/foo.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // bar.h in /high â€” different name, not a shadow.
        let affected = graph.check_shadow(Path::new("/high/bar.h"));
        assert!(affected.is_empty());
    }

    #[test]
    fn check_shadow_same_dir_not_shadow() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/inc")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/foo.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Same dir â€” this is a modify/replace, not a shadow.
        let affected = graph.check_shadow(Path::new("/inc/foo.h"));
        assert!(affected.is_empty());
    }

    #[test]
    fn check_shadow_iquote_over_user() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            iquote: vec![NormalizedPath::from("/iquote")],
            user: vec![NormalizedPath::from("/user")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        let key = graph.register(ctx);

        // foo.h resolves from -I dir.
        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/user/foo.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // New foo.h in -iquote dir (higher priority).
        let affected = graph.check_shadow(Path::new("/iquote/foo.h"));
        assert_eq!(affected, vec![key]);
    }

    #[test]
    fn check_shadow_cold_context_not_affected() {
        let graph = DepGraph::new();
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/high"), NormalizedPath::from("/low")],
            ..Default::default()
        };
        let ctx = make_ctx_with_search("/src/main.c", search);
        graph.register(ctx);

        // Cold context has no resolved includes â€” nothing to shadow.
        let affected = graph.check_shadow(Path::new("/high/foo.h"));
        assert!(affected.is_empty());
    }

    // --- New resolve detection tests ---

    #[test]
    fn check_new_resolve_matches_unresolved() {
        let graph = DepGraph::new();
        let ctx = make_ctx_with_search("/src/main.c", IncludeSearchPaths::default());
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: vec!["missing.h".to_string()],
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        let affected = graph.check_new_resolve(Path::new("/inc/missing.h"));
        assert_eq!(affected, vec![key]);
    }

    #[test]
    fn check_new_resolve_no_match() {
        let graph = DepGraph::new();
        let ctx = make_ctx_with_search("/src/main.c", IncludeSearchPaths::default());
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: vec!["missing.h".to_string()],
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        let affected = graph.check_new_resolve(Path::new("/inc/other.h"));
        assert!(affected.is_empty());
    }

    #[test]
    fn check_new_resolve_path_include() {
        let graph = DepGraph::new();
        let ctx = make_ctx_with_search("/src/main.c", IncludeSearchPaths::default());
        let key = graph.register(ctx);

        // Unresolved include with a path component.
        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: vec!["sub/missing.h".to_string()],
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // New file with matching filename.
        let affected = graph.check_new_resolve(Path::new("/inc/sub/missing.h"));
        assert_eq!(affected, vec![key]);
    }

    // --- mark_stale tests ---

    #[test]
    fn mark_stale_changes_state() {
        let graph = DepGraph::new();
        let ctx = make_ctx_with_search("/src/main.c", IncludeSearchPaths::default());
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(
            graph.get_state(&key),
            Some(super::graph::ContextState::Warm)
        );

        assert!(graph.mark_stale(&key));
        assert_eq!(
            graph.get_state(&key),
            Some(super::graph::ContextState::Stale)
        );
    }

    #[test]
    fn mark_stale_nonexistent_returns_false() {
        let graph = DepGraph::new();
        let ctx = make_ctx_with_search("/src/main.c", IncludeSearchPaths::default());
        let key = ctx.context_key();
        assert!(!graph.mark_stale(&key));
    }

    // --- is_higher_priority tests ---

    #[test]
    fn priority_iquote_before_user() {
        let search = IncludeSearchPaths {
            iquote: vec![NormalizedPath::from("/q")],
            user: vec![NormalizedPath::from("/u")],
            ..Default::default()
        };
        assert!(is_higher_priority(
            Path::new("/q"),
            Path::new("/u"),
            &search
        ));
        assert!(!is_higher_priority(
            Path::new("/u"),
            Path::new("/q"),
            &search
        ));
    }

    #[test]
    fn priority_user_before_system() {
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/u")],
            system: vec![NormalizedPath::from("/s")],
            ..Default::default()
        };
        assert!(is_higher_priority(
            Path::new("/u"),
            Path::new("/s"),
            &search
        ));
    }

    #[test]
    fn priority_unknown_dir_returns_false() {
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/u")],
            ..Default::default()
        };
        assert!(!is_higher_priority(
            Path::new("/unknown"),
            Path::new("/u"),
            &search
        ));
    }

    #[test]
    fn priority_same_dir_returns_false() {
        let search = IncludeSearchPaths {
            user: vec![NormalizedPath::from("/u")],
            ..Default::default()
        };
        assert!(!is_higher_priority(
            Path::new("/u"),
            Path::new("/u"),
            &search
        ));
    }
}
