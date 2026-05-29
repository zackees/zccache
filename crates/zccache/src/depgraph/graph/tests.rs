//! `#[cfg(test)]` unit tests for `graph`.
//!
//! Carved out of `mod.rs` so the main file stays under the LOC guard.

use super::*;
use crate::core::NormalizedPath;
use std::path::Path;

use super::super::search_paths::IncludeSearchPaths;

fn make_ctx(source: &str) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(source),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
}

fn always_fresh(_: &Path) -> bool {
    true
}

fn never_fresh(_: &Path) -> bool {
    false
}

fn dummy_hash(path: &Path) -> Option<ContentHash> {
    Some(crate::hash::hash_bytes(path.to_string_lossy().as_bytes()))
}

#[test]
fn register_returns_consistent_key() {
    let graph = DepGraph::new();
    let ctx = make_ctx("/src/a.c");
    let k1 = graph.register(ctx.clone());
    let k2 = graph.register(ctx);
    assert_eq!(k1, k2);
}

#[test]
fn cold_context_returns_cold() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));
    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::Cold));
}

#[test]
fn unregistered_key_returns_cold() {
    let graph = DepGraph::new();
    let ctx = make_ctx("/src/a.c");
    let key = ctx.context_key();
    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::Cold));
}

#[test]
fn warm_context_all_fresh_returns_hit() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));
}

#[test]
fn rustc_extern_artifact_key_ignores_target_dir_path_shape() {
    let graph = DepGraph::new();
    let ctx = make_ctx("/src/app.rs");
    let key = ctx.context_key();
    let source_hash = crate::hash::hash_bytes(b"app");
    let extern_hash = crate::hash::hash_bytes(b"dep-v1");
    let extern_a = NormalizedPath::from("/target-main/libdep.rlib");
    let extern_b = NormalizedPath::from("/target-subagent/libdep.rlib");

    graph.register_rustc_with_key_and_root_result(
        key,
        ctx.clone(),
        None,
        vec![("dep".to_string(), extern_a.clone())],
    );
    let first_key = graph
        .update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            |path| {
                if path == Path::new("/src/app.rs") {
                    Some(source_hash)
                } else if path == extern_a.as_path() {
                    Some(extern_hash)
                } else {
                    None
                }
            },
        )
        .expect("rustc artifact key should be computed");

    graph.register_rustc_with_key_and_root_result(
        key,
        ctx,
        None,
        vec![("dep".to_string(), extern_b.clone())],
    );
    let verdict = graph.check(&key, always_fresh, |path| {
        if path == Path::new("/src/app.rs") {
            Some(source_hash)
        } else if path == extern_b.as_path() {
            Some(extern_hash)
        } else {
            None
        }
    });

    match verdict {
        CacheVerdict::Hit { artifact_key } => assert_eq!(artifact_key, first_key),
        other => panic!("expected rustc extern path-shape hit, got {other:?}"),
    }
}

#[test]
fn warm_context_source_changed_returns_source_changed() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // Source is stale-by-watcher AND its content hash now differs from
    // the stored hash (post-fallback semantics: a header/source is
    // only "changed" if journal says stale AND the content hash also
    // moved).
    let is_fresh = |p: &Path| p != Path::new("/src/a.c");
    let changed_source_hash = |p: &Path| -> Option<ContentHash> {
        if p == Path::new("/src/a.c") {
            Some(crate::hash::hash_bytes(b"source-modified"))
        } else {
            dummy_hash(p)
        }
    };
    let verdict = graph.check(&key, is_fresh, changed_source_hash);
    assert!(matches!(verdict, CacheVerdict::SourceChanged { .. }));
}

#[test]
fn warm_context_header_changed_returns_headers_changed() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![
            NormalizedPath::from("/inc/b.h"),
            NormalizedPath::from("/inc/c.h"),
        ],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // b.h is stale-by-watcher AND its current content hash differs
    // from the stored hash (so the hash-fallback also flags it).
    let is_fresh = |p: &Path| p != Path::new("/inc/b.h");
    let changed_b_hash = |p: &Path| -> Option<ContentHash> {
        if p == Path::new("/inc/b.h") {
            Some(crate::hash::hash_bytes(b"b-modified"))
        } else {
            dummy_hash(p)
        }
    };
    let verdict = graph.check(&key, is_fresh, changed_b_hash);
    match verdict {
        CacheVerdict::HeadersChanged { changed } => {
            assert_eq!(changed, vec![NormalizedPath::from("/inc/b.h")]);
        }
        other => panic!("expected HeadersChanged, got {other:?}"),
    }
}

#[test]
fn warm_context_header_stale_by_watcher_but_hash_unchanged_returns_hit() {
    // Regression guard for the journal-cold-after-restart fix:
    // an empty in-memory journal post-restart makes `is_fresh` return
    // false for every path, but if the content hash still matches the
    // stored one we must treat the file as fresh-by-content. Before
    // the fix, every cached header was reported as HeadersChanged and
    // every Warm context degraded to a miss on the warm side of the
    // cold-tar-untar-warm perf scenario.
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // Journal claims b.h has changed (it's never been seen), but
    // dummy_hash returns the same hash for the same path — so the
    // content didn't actually change.
    let verdict = graph.check(&key, never_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));
}

#[test]
fn computed_includes_returns_needs_preprocessor() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: true,
    };
    graph.update(&key, scan, dummy_hash);

    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::NeedsPreprocessor));
}

#[test]
fn show_includes_enables_cache_hit_after_computed() {
    // Simulates the MSVC /showIncludes optimization:
    // 1. First update from scanner: has_computed=true → NeedsPreprocessor
    // 2. Second update from /showIncludes: has_computed=false → Hit
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    // Scanner found #include MACRO → has_computed=true
    let scanner_scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/known.h")],
        unresolved: Vec::new(),
        has_computed: true,
    };
    graph.update(&key, scanner_scan, dummy_hash);

    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(matches!(verdict, CacheVerdict::NeedsPreprocessor));

    // /showIncludes resolved all includes → has_computed=false
    let depfile_scan = ScanResult {
        resolved: vec![
            NormalizedPath::from("/inc/known.h"),
            NormalizedPath::from("/inc/macro_resolved.h"),
        ],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, depfile_scan, dummy_hash);

    // Now should be a hit.
    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(
        matches!(verdict, CacheVerdict::Hit { .. }),
        "expected Hit after /showIncludes update, got {verdict:?}"
    );
}

#[test]
fn update_sets_warm_state() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));
    assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

    let scan = ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);
    assert_eq!(graph.get_state(&key), Some(ContextState::Warm));
}

#[test]
fn header_change_sets_stale_state() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/h.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);
    assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

    // Both the watcher AND the content hash say h.h changed — the
    // hash-fallback can't rescue this one, so the verdict is
    // HeadersChanged and the entry flips to Stale.
    let changed_h_hash = |p: &Path| -> Option<ContentHash> {
        if p == Path::new("/h.h") {
            Some(crate::hash::hash_bytes(b"h-modified"))
        } else {
            dummy_hash(p)
        }
    };
    graph.check(&key, never_fresh, changed_h_hash);
    assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
}

#[test]
fn trim_removes_old_entries() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // Sleep briefly so the entry's last_accessed is older than Duration::ZERO.
    std::thread::sleep(Duration::from_millis(5));

    // Trim with max_age=0: everything not accessed this exact instant is removed.
    let removed = graph.trim(Duration::ZERO);
    assert_eq!(removed, 1);
    assert_eq!(graph.stats().context_count, 0);
}

#[test]
fn trim_keeps_recent_entries() {
    let graph = DepGraph::new();
    graph.register(make_ctx("/src/a.c"));
    let removed = graph.trim(Duration::from_secs(60));
    assert_eq!(removed, 0);
    assert_eq!(graph.stats().context_count, 1);
}

#[test]
fn stats_track_checks_and_hits() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    graph.check(&key, always_fresh, dummy_hash);
    graph.check(&key, always_fresh, dummy_hash);

    let stats = graph.stats();
    assert_eq!(stats.checks, 2);
    assert_eq!(stats.hits, 2);
    assert_eq!(stats.misses, 0);
    assert_eq!(stats.context_count, 1);
}

#[test]
fn artifact_key_changes_when_hash_changes() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    };

    let hash_v1 = |_: &Path| Some(crate::hash::hash_bytes(b"v1"));
    let ak1 = graph.update(&key, scan.clone(), hash_v1).unwrap();

    let hash_v2 = |_: &Path| Some(crate::hash::hash_bytes(b"v2"));
    let ak2 = graph.update(&key, scan, hash_v2).unwrap();

    assert_ne!(ak1, ak2);
}

#[test]
fn store_and_get_file_includes() {
    let graph = DepGraph::new();
    let path = NormalizedPath::from("/src/foo.h");
    let includes = vec![super::super::IncludeDirective {
        kind: super::super::IncludeKind::Quoted,
        path: "bar.h".to_string(),
        line: 1,
    }];

    graph.store_file_includes(path.clone(), includes.clone());
    let retrieved = graph.get_file_includes(&path).unwrap();
    assert_eq!(retrieved.len(), 1);
    assert_eq!(retrieved[0].path, "bar.h");
}

#[test]
fn concurrent_register_and_check() {
    use std::sync::Arc;
    use std::thread;

    let graph = Arc::new(DepGraph::new());
    let mut handles = Vec::new();

    // 4 threads registering and checking.
    for t in 0..4 {
        let graph = Arc::clone(&graph);
        handles.push(thread::spawn(move || {
            for i in 0..50 {
                let ctx = make_ctx(&format!("/src/t{t}_f{i}.c"));
                let key = graph.register(ctx);

                let scan = ScanResult {
                    resolved: vec![NormalizedPath::from(format!("/inc/t{t}_h{i}.h"))],
                    unresolved: Vec::new(),
                    has_computed: false,
                };
                graph.update(&key, scan, dummy_hash);
                graph.check(&key, always_fresh, dummy_hash);
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    let stats = graph.stats();
    assert_eq!(stats.context_count, 200); // 4 * 50
    assert_eq!(stats.checks, 200);
}

#[test]
fn ingest_compile_commands_registers_contexts() {
    let json = r#"[
        {
            "directory": "/build",
            "command": "g++ -I/project/include -DNDEBUG -std=c++17 -c /project/src/main.cpp -o main.o",
            "file": "/project/src/main.cpp"
        },
        {
            "directory": "/build",
            "command": "g++ -I/project/include -DNDEBUG -std=c++17 -c /project/src/util.cpp -o util.o",
            "file": "/project/src/util.cpp"
        }
    ]"#;

    let commands = super::super::compile_commands::parse_compile_commands_json(json).unwrap();
    let graph = DepGraph::new();
    let system_includes = vec![NormalizedPath::from("/usr/include")];
    let keys = graph.ingest_compile_commands(&commands, &system_includes);

    assert_eq!(keys.len(), 2);
    assert_eq!(graph.stats().context_count, 2);

    // All contexts should be Cold (not yet scanned).
    for key in &keys {
        assert_eq!(graph.get_state(key), Some(ContextState::Cold));
    }
}

#[test]
fn ingest_merges_system_includes() {
    let json = r#"[
        {
            "directory": "/build",
            "command": "g++ -isystem /explicit/system -c /src/main.cpp",
            "file": "/src/main.cpp"
        }
    ]"#;

    let commands = super::super::compile_commands::parse_compile_commands_json(json).unwrap();
    let graph = DepGraph::new();
    let system_includes = vec![NormalizedPath::from("/usr/include")];
    let keys = graph.ingest_compile_commands(&commands, &system_includes);

    assert_eq!(keys.len(), 1);

    // The context should have both the explicit and system includes.
    // We can verify by checking the context key differs with/without system includes.
    let keys_no_sys = graph.ingest_compile_commands(&commands, &[]);

    // Same source + different system includes = different context keys.
    assert_ne!(keys[0], keys_no_sys[0]);
}

#[test]
fn ingest_deduplicates_system_includes() {
    let json = r#"[
        {
            "directory": "/build",
            "command": "g++ -isystem /usr/include -c /src/main.cpp",
            "file": "/src/main.cpp"
        }
    ]"#;

    let commands = super::super::compile_commands::parse_compile_commands_json(json).unwrap();
    let graph = DepGraph::new();
    // /usr/include is already in -isystem, should not be added twice.
    let system_includes = vec![NormalizedPath::from("/usr/include")];
    let keys = graph.ingest_compile_commands(&commands, &system_includes);
    assert_eq!(keys.len(), 1);
}

#[test]
fn clear_resets_everything() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);
    graph.check(&key, always_fresh, dummy_hash);

    let stats_before = graph.stats();
    assert!(stats_before.context_count > 0);
    assert!(stats_before.checks > 0);
    assert!(stats_before.hits > 0);

    graph.clear();

    let stats_after = graph.stats();
    assert_eq!(stats_after.context_count, 0);
    assert_eq!(stats_after.file_count, 0);
    assert_eq!(stats_after.checks, 0);
    assert_eq!(stats_after.hits, 0);
    assert_eq!(stats_after.misses, 0);
}

#[test]
fn mark_stale_changes_state() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);
    assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

    assert!(graph.mark_stale(&key));
    assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
}

// ── update() atomicity tests ──────────────────────────────────────────

#[test]
fn update_with_hash_failure_stays_cold() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));
    assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    // Source hash fails → update returns None, state must stay Cold.
    let no_hash = |_: &Path| -> Option<ContentHash> { None };
    let result = graph.update(&key, scan, no_hash);
    assert!(result.is_none());
    assert_eq!(graph.get_state(&key), Some(ContextState::Cold));
}

#[test]
fn update_partial_hash_failure_stays_cold() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));

    let scan = ScanResult {
        resolved: vec![
            NormalizedPath::from("/inc/a.h"),
            NormalizedPath::from("/inc/b.h"),
            NormalizedPath::from("/inc/c.h"),
        ],
        unresolved: Vec::new(),
        has_computed: false,
    };
    // 2nd header hash fails → state must stay Cold.
    let partial_hash = |p: &Path| -> Option<ContentHash> {
        if p == Path::new("/inc/b.h") {
            None
        } else {
            Some(crate::hash::hash_bytes(p.to_string_lossy().as_bytes()))
        }
    };
    let result = graph.update(&key, scan, partial_hash);
    assert!(result.is_none());
    assert_eq!(graph.get_state(&key), Some(ContextState::Cold));
}

#[test]
fn update_success_transitions_to_warm() {
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/a.c"));
    assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    let result = graph.update(&key, scan, dummy_hash);
    assert!(result.is_some());
    assert_eq!(graph.get_state(&key), Some(ContextState::Warm));
}

#[test]
fn pch_gen_context_hit_after_update() {
    // Register a PCH-generation context (no force_includes — it IS the PCH).
    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/pch.h"));

    let scan = ScanResult {
        resolved: vec![
            NormalizedPath::from("/inc/a.h"),
            NormalizedPath::from("/inc/b.h"),
        ],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // check() should return Hit, not Cold.
    let verdict = graph.check(&key, always_fresh, dummy_hash);
    assert!(
        matches!(verdict, CacheVerdict::Hit { .. }),
        "expected Hit after update, got {verdict:?}"
    );
}

#[test]
fn warm_context_with_no_artifact_returns_cold_on_check() {
    // Simulate the bug scenario: state=Warm but artifact_key=None.
    // With the fix, this can't happen via update() — but if someone
    // manually sets state=Warm, check_diagnostic should handle it.
    let graph = DepGraph::new();
    let ctx = make_ctx("/src/a.c");
    let key = ctx.context_key();

    // Manually insert a Warm entry with no artifact key.
    graph.contexts.insert(
        key,
        ContextEntry {
            context: ctx,
            key_root: None,
            resolved_includes: vec![NormalizedPath::from("/inc/b.h")],
            unresolved_includes: Vec::new(),
            has_computed_includes: false,
            artifact_key: None,
            last_file_hashes: Vec::new(),
            last_accessed: Instant::now(),
            state: ContextState::Warm,
        },
    );

    // check_diagnostic should still produce a valid verdict (not panic).
    // With all fresh, it should compute an artifact key and return Hit.
    let (verdict, _reason) = graph.check_diagnostic(&key, always_fresh, dummy_hash);
    assert!(
        matches!(
            verdict,
            CacheVerdict::Hit { .. } | CacheVerdict::SourceChanged { .. }
        ),
        "warm context with all hashes available should hit, got {verdict:?}"
    );
}

#[test]
fn trim_preserves_force_include_files() {
    let graph = DepGraph::new();

    // Create a context with a force-include (PCH file).
    let mut ctx = make_ctx("/src/a.c");
    ctx.force_includes = vec![NormalizedPath::from("/pch/precompiled.h")];
    let key = graph.register(ctx);

    let scan = ScanResult {
        resolved: vec![NormalizedPath::from("/inc/b.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, dummy_hash);

    // Populate the files map for both the force-include and resolved include.
    let empty_includes = vec![super::super::IncludeDirective {
        kind: super::super::IncludeKind::Quoted,
        path: "stdafx.h".to_string(),
        line: 1,
    }];
    graph.store_file_includes(
        NormalizedPath::from("/pch/precompiled.h"),
        empty_includes.clone(),
    );
    graph.store_file_includes(NormalizedPath::from("/inc/b.h"), empty_includes);

    // Also add an unreferenced file that should be evicted.
    graph.store_file_includes(
        NormalizedPath::from("/stale/old.h"),
        vec![super::super::IncludeDirective {
            kind: super::super::IncludeKind::Quoted,
            path: "gone.h".to_string(),
            line: 1,
        }],
    );

    assert_eq!(graph.stats().file_count, 3);

    // Trim with a long max_age — no contexts should be removed.
    let removed = graph.trim(Duration::from_secs(3600));
    assert_eq!(removed, 0);

    // The force-included PCH file must still be in the files map.
    assert!(
        graph
            .get_file_includes(&NormalizedPath::from("/pch/precompiled.h"))
            .is_some(),
        "force-included PCH file should not be evicted by trim"
    );
    // Regular includes should also be preserved.
    assert!(
        graph
            .get_file_includes(&NormalizedPath::from("/inc/b.h"))
            .is_some(),
        "resolved include should not be evicted by trim"
    );
    // Unreferenced file should be evicted.
    assert!(
        graph
            .get_file_includes(&NormalizedPath::from("/stale/old.h"))
            .is_none(),
        "unreferenced file should be evicted by trim"
    );
    assert_eq!(graph.stats().file_count, 2);
}
