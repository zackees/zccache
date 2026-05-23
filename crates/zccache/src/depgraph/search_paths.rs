//! Include search path types and resolution order.

use std::path::Path;

use crate::core::NormalizedPath;

/// Ordered include search paths, preserving -I/-isystem/-iquote/-idirafter.
///
/// Resolution order for `#include "foo.h"` (quoted):
///   1. Directory of the including file
///   2. `-iquote` dirs
///   3. `-I` dirs
///   4. `-isystem` dirs + compiler defaults
///   5. `-idirafter` dirs
///
/// Resolution order for `#include <foo.h>` (angle bracket):
///   1. `-I` dirs
///   2. `-isystem` dirs + compiler defaults
///   3. `-idirafter` dirs
#[derive(Debug, Clone, Default)]
pub struct IncludeSearchPaths {
    /// `-iquote` paths â€” searched only for quoted includes, before `-I`.
    pub iquote: Vec<NormalizedPath>,
    /// `-I` paths â€” user include paths (order matters!).
    pub user: Vec<NormalizedPath>,
    /// `-isystem` paths + compiler default system dirs.
    pub system: Vec<NormalizedPath>,
    /// `-idirafter` paths â€” searched last.
    pub after: Vec<NormalizedPath>,
}

impl IncludeSearchPaths {
    /// Iterate search dirs for a quoted include (`#include "foo.h"`),
    /// starting after the including file's own directory.
    pub fn quoted_search_dirs(&self) -> impl Iterator<Item = &Path> {
        self.iquote
            .iter()
            .chain(self.user.iter())
            .chain(self.system.iter())
            .chain(self.after.iter())
            .map(|p| p.as_path())
    }

    /// Iterate search dirs for an angle-bracket include (`#include <foo.h>`).
    pub fn angle_search_dirs(&self) -> impl Iterator<Item = &Path> {
        self.user
            .iter()
            .chain(self.system.iter())
            .chain(self.after.iter())
            .map(|p| p.as_path())
    }

    /// Iterate all search dirs in priority order (iquote â†’ user â†’ system â†’ after).
    /// This is the superset of both quoted and angle-bracket search orders.
    pub fn all_search_dirs(&self) -> impl Iterator<Item = &Path> {
        self.iquote
            .iter()
            .chain(self.user.iter())
            .chain(self.system.iter())
            .chain(self.after.iter())
            .map(|p| p.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_search_includes_iquote_first() {
        let paths = IncludeSearchPaths {
            iquote: vec!["/q".into()],
            user: vec!["/u".into()],
            system: vec!["/s".into()],
            after: vec!["/a".into()],
        };
        let dirs: Vec<&Path> = paths.quoted_search_dirs().collect();
        assert_eq!(
            dirs,
            vec![
                Path::new("/q"),
                Path::new("/u"),
                Path::new("/s"),
                Path::new("/a"),
            ]
        );
    }

    #[test]
    fn angle_search_skips_iquote() {
        let paths = IncludeSearchPaths {
            iquote: vec!["/q".into()],
            user: vec!["/u".into()],
            system: vec!["/s".into()],
            after: vec!["/a".into()],
        };
        let dirs: Vec<&Path> = paths.angle_search_dirs().collect();
        assert_eq!(
            dirs,
            vec![Path::new("/u"), Path::new("/s"), Path::new("/a"),]
        );
    }

    #[test]
    fn empty_paths_produce_empty_iterators() {
        let paths = IncludeSearchPaths::default();
        assert_eq!(paths.quoted_search_dirs().count(), 0);
        assert_eq!(paths.angle_search_dirs().count(), 0);
    }

    #[test]
    fn user_dir_order_preserved() {
        let paths = IncludeSearchPaths {
            user: vec!["/first".into(), "/second".into(), "/third".into()],
            ..Default::default()
        };
        let dirs: Vec<&Path> = paths.angle_search_dirs().collect();
        assert_eq!(
            dirs,
            vec![
                Path::new("/first"),
                Path::new("/second"),
                Path::new("/third")
            ]
        );
    }
}
