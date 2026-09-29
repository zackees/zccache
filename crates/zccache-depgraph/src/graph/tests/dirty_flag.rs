//! zccache#1775: mutation-tracking so a shutdown/periodic snapshot can skip
//! re-serializing and re-fsyncing a graph that hasn't changed since the last
//! save. Every mutating entry point that touches persisted state must mark
//! the graph dirty; pure reads (fast-hit lookups) must not.

use std::path::Path;

use zccache_core::NormalizedPath;
use zccache_hash::ContentHash;

use super::super::context::CompileContext;
use super::super::scanner::ScanResult;
use super::super::search_paths::IncludeSearchPaths;
use super::{CacheVerdict, DepGraph};

fn context(source: &str) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(source),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: zccache_hash::hash_bytes(b"test-fixture"),
    }
}

fn scan() -> ScanResult {
    ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    }
}

fn dummy_hash(path: &Path) -> Option<ContentHash> {
    Some(zccache_hash::hash_bytes(path.to_string_lossy().as_bytes()))
}

fn always_fresh(_: &Path) -> bool {
    true
}

#[test]
fn fresh_graph_starts_clean() {
    let graph = DepGraph::new();
    assert!(!graph.is_dirty());
    assert!(!graph.take_dirty(), "nothing to clear on a fresh graph");
}

#[test]
fn reconstructed_from_maps_starts_clean() {
    // Mirrors `snapshot::from_snapshot`'s load path: pre-built maps, no
    // mutating call. A daemon that just loaded its depgraph.bin has
    // nothing new to write back until something actually changes.
    let graph = DepGraph::from_maps_with_rustc_externs(
        dashmap::DashMap::new(),
        dashmap::DashMap::new(),
        dashmap::DashMap::new(),
    );
    assert!(!graph.is_dirty());
}

#[test]
fn register_marks_dirty_until_taken() {
    let graph = DepGraph::new();
    graph.register(context("/src/a.c"));
    assert!(graph.is_dirty());
    assert!(graph.take_dirty());
    assert!(!graph.is_dirty(), "take_dirty must clear the flag");
}

#[test]
fn successful_update_marks_dirty() {
    let graph = DepGraph::new();
    let key = graph.register(context("/src/a.c"));
    graph.take_dirty();

    let artifact_key = graph.update(&key, scan(), dummy_hash);
    assert!(
        artifact_key.is_some(),
        "update must succeed with no headers"
    );
    assert!(graph.is_dirty(), "a warm transition must be persisted");
}

#[test]
fn check_hit_marks_dirty_via_last_accessed_touch() {
    let graph = DepGraph::new();
    let key = graph.register(context("/src/a.c"));
    graph.update(&key, scan(), dummy_hash);
    graph.take_dirty();

    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));
    assert!(
        graph.is_dirty(),
        "check() touches last_accessed_unix_ms, which is part of the \
         persisted snapshot"
    );
}

#[test]
fn try_fast_hit_does_not_mark_dirty() {
    // The ultra-fast hit path is read-only (`contexts.get`, not
    // `contexts.get_mut`) and must not force a resave on a daemon that is
    // otherwise idle apart from serving warm hits.
    let graph = DepGraph::new();
    let key = graph.register(context("/src/a.c"));
    graph.update(&key, scan(), dummy_hash);
    graph.take_dirty();

    let hit = graph.try_fast_hit(&key, dummy_hash);
    assert!(hit.is_some());
    assert!(
        !graph.is_dirty(),
        "a pure ultra-fast-hit lookup must not mark the graph dirty"
    );
}

#[test]
fn mark_stale_of_missing_context_does_not_mark_dirty() {
    let graph = DepGraph::new();
    // A key that was never registered on `graph` (registered instead on an
    // unrelated, throwaway graph) is guaranteed "missing" here.
    let missing = DepGraph::new().register(context("/src/never-registered.c"));
    assert!(!graph.mark_stale(&missing));
    assert!(!graph.is_dirty());
}

#[test]
fn mark_stale_of_existing_context_marks_dirty() {
    let graph = DepGraph::new();
    let key = graph.register(context("/src/a.c"));
    graph.take_dirty();

    assert!(graph.mark_stale(&key));
    assert!(graph.is_dirty());
}

#[test]
fn evict_contexts_of_nothing_does_not_mark_dirty() {
    let graph = DepGraph::new();
    graph.register(context("/src/a.c"));
    graph.take_dirty();

    let removed = graph.evict_contexts(&[]);
    assert_eq!(removed, 0);
    assert!(!graph.is_dirty());
}

#[test]
fn clear_marks_dirty() {
    let graph = DepGraph::new();
    graph.register(context("/src/a.c"));
    graph.take_dirty();

    graph.clear();
    assert!(
        graph.is_dirty(),
        "a cleared graph must be re-persisted so disk reflects the clear"
    );
}
