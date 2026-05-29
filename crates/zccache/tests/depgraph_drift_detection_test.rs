//! Regression tests for issue #449: read-side / write-side artifact_key
//! divergence when a header's content drifts but the journal still claims
//! it is fresh (Windows watcher loses events under load, editors that
//! replace-then-rename, etc.).
//!
//! The bug: `check_diagnostic` recomputes the artifact key from
//! `entry.resolved_includes` (the STALE include set captured by the
//! previous `update()`) plus the current content hashes, then returns
//! `Hit { artifact_key: K_predict }` whenever the source path is "fresh".
//! The pipeline trusts K_predict and looks it up in the artifact store —
//! a coincidentally matching older artifact (compiled when the include
//! set happened to be the same) would be served as a stale `.obj`.
//!
//! The fix: when any file's hash has drifted since the last `update()`,
//! the verdict must NOT be `Hit`. Surfacing it as `HeadersChanged` forces
//! the pipeline through the cold-miss path: recompile, re-scan, and
//! re-store under the write-side key derived from the current dependency
//! set. The next read against an unchanged state then ultra-fast-hits the
//! write-side key (single source of truth).

use std::path::Path;

use zccache::core::NormalizedPath;
use zccache::depgraph::{CacheVerdict, CompileContext, DepGraph, ScanResult};
use zccache::hash::{hash_bytes, ContentHash};

fn make_ctx(source: &str) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(source),
        include_search: Default::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
}

fn dummy_hash(path: &Path) -> Option<ContentHash> {
    Some(hash_bytes(path.to_string_lossy().as_bytes()))
}

#[test]
fn drifted_header_hash_with_fresh_journal_does_not_return_hit() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // Journal lies: it still claims b.h is fresh, but the content hash
    // has drifted from what `update()` recorded in `last_file_hashes`.
    let drifted_hash = |p: &Path| -> Option<ContentHash> {
        if p == Path::new("/inc/b.h") {
            Some(hash_bytes(b"b-drifted"))
        } else {
            dummy_hash(p)
        }
    };
    let always_fresh = |_: &Path| true;

    let (verdict, reason) = graph.check_diagnostic(&key, always_fresh, drifted_hash);
    match verdict {
        CacheVerdict::HeadersChanged { changed } => {
            assert!(
                changed
                    .iter()
                    .any(|p| p == &NormalizedPath::from("/inc/b.h")),
                "drifted header should be reported; got {changed:?}"
            );
        }
        other => panic!(
            "expected HeadersChanged when header content drifted (so the include \
             set may have shifted with the change); got {other:?} (reason: {reason})"
        ),
    }

    // The non-diagnostic fast path used by the pipeline's first cache
    // probe must reach the same conclusion — otherwise a stale artifact
    // can be served before the pipeline ever consults `check_diagnostic`.
    let verdict_fast = graph.check(&key, always_fresh, drifted_hash);
    assert!(
        matches!(verdict_fast, CacheVerdict::HeadersChanged { .. }),
        "non-diagnostic check must also force a re-scan on drift; got {verdict_fast:?}"
    );
}

#[test]
fn drifted_source_hash_with_fresh_journal_does_not_return_hit() {
    // Symmetric to the header-drift case: a watcher miss on the source
    // file itself must not paper over content drift either.
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    let drifted_source_hash = |p: &Path| -> Option<ContentHash> {
        if p == Path::new("/src/a.c") {
            Some(hash_bytes(b"source-drifted"))
        } else {
            dummy_hash(p)
        }
    };
    let always_fresh = |_: &Path| true;

    let (verdict, reason) = graph.check_diagnostic(&key, always_fresh, drifted_source_hash);
    assert!(
        !matches!(verdict, CacheVerdict::Hit { .. }),
        "drifted source must not produce a Hit; got {verdict:?} (reason: {reason})"
    );
}

#[test]
fn warm_no_drift_still_returns_hit() {
    // Regression guard: the drift-detection fix must not break the common
    // ultra-fast-hit path where nothing has changed.
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    let always_fresh = |_: &Path| true;
    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(
        matches!(verdict, CacheVerdict::Hit { .. }),
        "unchanged warm context should still hit; got {verdict:?}"
    );
}
