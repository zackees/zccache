//! Merge independently collected dependency scans.

use std::collections::HashSet;

use zccache_core::NormalizedPath;

use crate::scanner::ScanResult;

/// Union the exact user-header depfile with a conservative static include scan.
#[must_use]
pub fn merge_scan_results(mut depfile: ScanResult, static_scan: ScanResult) -> ScanResult {
    let mut seen: HashSet<NormalizedPath> = depfile.resolved.iter().cloned().collect();
    depfile.resolved.extend(
        static_scan
            .resolved
            .into_iter()
            .filter(|path| seen.insert(path.clone())),
    );
    let mut unresolved: HashSet<String> = depfile.unresolved.iter().cloned().collect();
    depfile.unresolved.extend(
        static_scan
            .unresolved
            .into_iter()
            .filter(|path| unresolved.insert(path.clone())),
    );
    depfile.has_computed |= static_scan.has_computed;
    depfile
}

/// Merge a compiler manifest with a static scan without trusting incomplete
/// static resolution for direct-mode hits.
#[must_use]
pub fn merge_scan_results_conservative(
    depfile: ScanResult,
    mut static_scan: ScanResult,
) -> ScanResult {
    static_scan.has_computed |= !static_scan.unresolved.is_empty();
    merge_scan_results(depfile, static_scan)
}

#[cfg(test)]
mod tests {
    use super::{merge_scan_results, merge_scan_results_conservative};
    use crate::scanner::ScanResult;
    use zccache_core::NormalizedPath;

    #[test]
    fn merges_mmd_user_headers_with_static_system_headers_without_duplicates() {
        let source = NormalizedPath::from("/src/main.c");
        let user = NormalizedPath::from("/src/user.h");
        let system = NormalizedPath::from("/usr/include/stdio.h");
        let merged = merge_scan_results(
            ScanResult {
                resolved: vec![source, user.clone()],
                unresolved: vec!["missing.h".into()],
                has_computed: false,
            },
            ScanResult {
                resolved: vec![user, system.clone()],
                unresolved: Vec::new(),
                has_computed: false,
            },
        );

        assert_eq!(merged.resolved.len(), 3);
        assert!(merged.resolved.contains(&system));
        assert_eq!(merged.unresolved, vec!["missing.h"]);
        assert!(!merged.has_computed);
    }

    #[test]
    fn conservative_merge_disables_direct_mode_for_unresolved_static_include() {
        let merged = merge_scan_results_conservative(
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            ScanResult {
                resolved: Vec::new(),
                unresolved: vec!["compiler-only-system-header.h".into()],
                has_computed: false,
            },
        );

        assert!(merged.has_computed);
    }
}
