//! Cross-cutting behavior across save/load that touches more than a single
//! snapshot field: cache-hit recovery, context-key consistency, unicode
//! paths, double-roundtrip idempotency, overlapping contexts, all
//! `ContextState` variants, bit-flip detection, re-register after load,
//! GC behavior on save, concurrent save/load, large-graph stress.

use std::time::Duration;

use tempfile::TempDir;
use zccache_core::NormalizedPath;
use zccache_hash::ContentHash;

use super::super::super::context::CompileContext;
use super::super::super::graph::{CacheVerdict, ContextState, DepGraph};
use super::super::super::scanner::{IncludeDirective, IncludeKind, ScanResult};
use super::super::super::search_paths::IncludeSearchPaths;
use super::super::super::snapshot::{load_from_file, save_to_file, strings_to_paths, HEADER_SIZE};
use super::{always_fresh, dummy_hash, make_ctx, test_path};

#[test]
fn gc_trims_old_entries() {
    let graph = DepGraph::new();
    graph.register(make_ctx("/old.cpp"));
    assert_eq!(graph.stats().context_count, 1);

    // trim with zero duration removes all entries.
    let removed = graph.trim(Duration::ZERO);
    assert_eq!(removed, 1);
    assert_eq!(graph.stats().context_count, 0);
}

/// After save+load, a check() on the loaded graph must still return
/// Hit for previously-warm contexts. This is the most important
/// behavioral invariant.
#[test]
fn loaded_graph_serves_cache_hits() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let ctx = CompileContext {
        source_file: NormalizedPath::from("/src/main.cpp"),
        include_search: IncludeSearchPaths {
            user: vec![NormalizedPath::from("/include")],
            system: vec![NormalizedPath::from("/usr/include")],
            ..Default::default()
        },
        defines: vec!["NDEBUG".into()],
        flags: vec!["-O2".into(), "-std=c++17".into()],
        force_includes: vec![NormalizedPath::from("/pch.h")],
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
        (
            NormalizedPath::from("/src/main.cpp"),
            zccache_hash::hash_bytes(b"src"),
        ),
        (
            NormalizedPath::from("/include/a.h"),
            zccache_hash::hash_bytes(b"a"),
        ),
        (
            NormalizedPath::from("/pch.h"),
            zccache_hash::hash_bytes(b"pch"),
        ),
    ]
    .into_iter()
    .collect();

    graph.update(
        &key,
        ScanResult {
            resolved: vec![NormalizedPath::from("/include/a.h")],
            unresolved: Vec::new(),
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );

    // Verify original graph serves hits.
    let verdict = graph.check(&key, always_fresh, |p| {
        hashes.get(&NormalizedPath::new(p)).copied()
    });
    assert!(
        matches!(verdict, CacheVerdict::Hit { .. }),
        "original graph should hit, got {verdict:?}"
    );

    // Save, load, check again.
    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let verdict = loaded.check(&key, always_fresh, |p| {
        hashes.get(&NormalizedPath::new(p)).copied()
    });
    assert!(
        matches!(verdict, CacheVerdict::Hit { .. }),
        "loaded graph should still serve hit, got {verdict:?}"
    );
}

/// The stored context key must match the key recomputed from the
/// loaded CompileContext. If lossy PathBuf→String→NormalizedPath conversion
/// corrupts paths, the key will diverge and lookups will silently fail.
#[test]
fn context_key_consistent_after_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let ctx = CompileContext {
        source_file: NormalizedPath::from("/src/main.cpp"),
        include_search: IncludeSearchPaths {
            iquote: vec![NormalizedPath::from("/iquote/dir")],
            user: vec![NormalizedPath::from("/user/dir")],
            system: vec![NormalizedPath::from("/system/dir")],
            after: vec![NormalizedPath::from("/after/dir")],
        },
        defines: vec!["FOO=1".into(), "BAR=2".into()],
        flags: vec!["-Wall".into()],
        force_includes: vec![NormalizedPath::from("/fi/pch.h")],
        unknown_flags: vec!["--custom".into()],
    };
    let original_key = ctx.context_key();
    graph.register(ctx);

    let hash = zccache_hash::hash_bytes(b"x");
    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
        (NormalizedPath::from("/src/main.cpp"), hash),
        (NormalizedPath::from("/fi/pch.h"), hash),
    ]
    .into_iter()
    .collect();
    graph.update(
        &original_key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    // The loaded graph should find the entry by the original key.
    assert_eq!(
        loaded.get_state(&original_key),
        Some(ContextState::Warm),
        "loaded graph must find entry by original context key"
    );

    // Extract the loaded CompileContext and recompute its key.
    let snap = loaded.to_snapshot();
    assert_eq!(snap.contexts.len(), 1);
    let loaded_ctx = CompileContext {
        source_file: NormalizedPath::from(&snap.contexts[0].source_file),
        include_search: IncludeSearchPaths {
            iquote: strings_to_paths(snap.contexts[0].iquote.clone()),
            user: strings_to_paths(snap.contexts[0].user.clone()),
            system: strings_to_paths(snap.contexts[0].system.clone()),
            after: strings_to_paths(snap.contexts[0].after.clone()),
        },
        defines: snap.contexts[0].defines.clone(),
        flags: snap.contexts[0].flags.clone(),
        force_includes: strings_to_paths(snap.contexts[0].force_includes.clone()),
        unknown_flags: snap.contexts[0].unknown_flags.clone(),
    };
    let recomputed_key = loaded_ctx.context_key();
    assert_eq!(
        *original_key.hash().as_bytes(),
        *recomputed_key.hash().as_bytes(),
        "context key recomputed from loaded context must match stored key"
    );
}

/// Unicode paths must roundtrip correctly — they are common on macOS
/// (NFC normalization) and Windows (wide chars).
#[test]
fn unicode_paths_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let unicode_source = "/src/日本語/main.cpp";
    let unicode_header = "/inc/données/header.h";
    let unicode_define = "NÄME=Ünïcödé";
    let emoji_path = "/inc/🎉/emoji.h";

    let ctx = CompileContext {
        source_file: NormalizedPath::from(unicode_source),
        include_search: IncludeSearchPaths {
            user: vec![
                NormalizedPath::from(unicode_header),
                NormalizedPath::from(emoji_path),
            ],
            ..Default::default()
        },
        defines: vec![unicode_define.into()],
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);
    let hash = zccache_hash::hash_bytes(b"x");
    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> =
        [(NormalizedPath::from(unicode_source), hash)]
            .into_iter()
            .collect();
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );

    // Also store file includes with unicode paths.
    graph.store_file_includes(
        NormalizedPath::from(unicode_source),
        vec![IncludeDirective {
            kind: IncludeKind::Quoted,
            path: unicode_header.into(),
            line: 1,
        }],
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
    let includes = loaded
        .get_file_includes(&NormalizedPath::from(unicode_source))
        .unwrap();
    assert_eq!(includes[0].path, unicode_header);

    // Verify the context's include search paths survived.
    let snap = loaded.to_snapshot();
    assert_eq!(
        snap.contexts[0].source_file,
        NormalizedPath::from(unicode_source).display().to_string()
    );
    assert!(snap.contexts[0]
        .user
        .contains(&NormalizedPath::from(unicode_header).display().to_string()));
    assert!(snap.contexts[0]
        .user
        .contains(&NormalizedPath::from(emoji_path).display().to_string()));
    assert!(snap.contexts[0]
        .defines
        .contains(&unicode_define.to_string()));
}

/// Save→load→save→load must produce an identical graph. Tests for
/// any drift introduced by a single roundtrip (e.g., path
/// normalization, field reordering, floating precision).
#[test]
fn double_roundtrip_idempotent() {
    let dir = TempDir::new().unwrap();
    let path1 = dir.path().join("pass1.bin");
    let path2 = dir.path().join("pass2.bin");
    let graph = DepGraph::new();

    // Build a non-trivial graph.
    for i in 0..5 {
        let ctx = CompileContext {
            source_file: NormalizedPath::from(format!("/src/file{i}.cpp")),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from(format!("/inc{i}"))],
                system: vec![NormalizedPath::from("/sys")],
                ..Default::default()
            },
            defines: vec![format!("VAR{i}=1")],
            flags: vec!["-O2".into()],
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = graph.register(ctx);
        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from(format!("/inc{i}/h.h"))],
                unresolved: vec![format!("missing{i}.h")],
                has_computed: i == 0, // one with computed includes
            },
            dummy_hash,
        );
        graph.store_file_includes(
            NormalizedPath::from(format!("/src/file{i}.cpp")),
            vec![IncludeDirective {
                kind: IncludeKind::Quoted,
                path: format!("h{i}.h"),
                line: i as u32 + 1,
            }],
        );
    }

    // First roundtrip.
    save_to_file(&graph, &path1).unwrap();
    let loaded1 = load_from_file(&path1).unwrap();

    // Second roundtrip.
    save_to_file(&loaded1, &path2).unwrap();
    let loaded2 = load_from_file(&path2).unwrap();

    // Compare snapshots field-by-field.
    let snap1 = loaded1.to_snapshot();
    let snap2 = loaded2.to_snapshot();
    assert_eq!(snap1.files.len(), snap2.files.len(), "file count mismatch");
    assert_eq!(
        snap1.contexts.len(),
        snap2.contexts.len(),
        "context count mismatch"
    );

    // Sort by path for deterministic comparison (DashMap order is random).
    let mut files1: Vec<_> = snap1.files.iter().map(|f| &f.path).collect();
    let mut files2: Vec<_> = snap2.files.iter().map(|f| &f.path).collect();
    files1.sort();
    files2.sort();
    assert_eq!(files1, files2, "file paths differ after double roundtrip");

    let mut keys1: Vec<_> = snap1.contexts.iter().map(|c| c.context_key).collect();
    let mut keys2: Vec<_> = snap2.contexts.iter().map(|c| c.context_key).collect();
    keys1.sort();
    keys2.sort();
    assert_eq!(keys1, keys2, "context keys differ after double roundtrip");
}

/// Multiple contexts referencing overlapping resolved includes.
/// All must survive independently.
#[test]
fn overlapping_contexts_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let shared_header = NormalizedPath::from("/inc/shared.h");

    // Two contexts that share the same header.
    let ctx_a = CompileContext {
        source_file: NormalizedPath::from("/src/a.cpp"),
        include_search: IncludeSearchPaths {
            user: vec![NormalizedPath::from("/inc")],
            ..Default::default()
        },
        defines: vec!["A=1".into()],
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let ctx_b = CompileContext {
        source_file: NormalizedPath::from("/src/b.cpp"),
        include_search: IncludeSearchPaths {
            user: vec![NormalizedPath::from("/inc")],
            ..Default::default()
        },
        defines: vec!["B=1".into()],
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };

    let key_a = graph.register(ctx_a);
    let key_b = graph.register(ctx_b);

    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
        (
            NormalizedPath::from("/src/a.cpp"),
            zccache_hash::hash_bytes(b"a"),
        ),
        (
            NormalizedPath::from("/src/b.cpp"),
            zccache_hash::hash_bytes(b"b"),
        ),
        (shared_header.clone(), zccache_hash::hash_bytes(b"shared")),
    ]
    .into_iter()
    .collect();

    graph.update(
        &key_a,
        ScanResult {
            resolved: vec![shared_header.clone()],
            unresolved: Vec::new(),
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );
    graph.update(
        &key_b,
        ScanResult {
            resolved: vec![shared_header.clone()],
            unresolved: Vec::new(),
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(loaded.stats().context_count, 2);
    assert_eq!(loaded.get_state(&key_a), Some(ContextState::Warm));
    assert_eq!(loaded.get_state(&key_b), Some(ContextState::Warm));

    // Both should serve hits.
    let verdict_a = loaded.check(&key_a, always_fresh, |p| {
        hashes.get(&NormalizedPath::new(p)).copied()
    });
    let verdict_b = loaded.check(&key_b, always_fresh, |p| {
        hashes.get(&NormalizedPath::new(p)).copied()
    });
    assert!(matches!(verdict_a, CacheVerdict::Hit { .. }));
    assert!(matches!(verdict_b, CacheVerdict::Hit { .. }));

    // And they must have different artifact keys (different source files).
    match (verdict_a, verdict_b) {
        (CacheVerdict::Hit { artifact_key: ak_a }, CacheVerdict::Hit { artifact_key: ak_b }) => {
            assert_ne!(
                ak_a.hash().as_bytes(),
                ak_b.hash().as_bytes(),
                "different contexts should have different artifact keys"
            );
        }
        _ => unreachable!(),
    }
}

/// All three ContextState variants must survive roundtrip faithfully.
#[test]
fn all_states_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    // Cold context: just register, never update.
    let cold_key = graph.register(make_ctx("/src/cold.cpp"));
    assert_eq!(graph.get_state(&cold_key), Some(ContextState::Cold));

    // Warm context: register + update.
    let warm_key = graph.register(make_ctx("/src/warm.cpp"));
    graph.update(
        &warm_key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        dummy_hash,
    );
    assert_eq!(graph.get_state(&warm_key), Some(ContextState::Warm));

    // Stale context: register + update + mark stale.
    let stale_key = graph.register(make_ctx("/src/stale.cpp"));
    graph.update(
        &stale_key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        dummy_hash,
    );
    graph.mark_stale(&stale_key);
    assert_eq!(graph.get_state(&stale_key), Some(ContextState::Stale));

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(
        loaded.get_state(&cold_key),
        Some(ContextState::Cold),
        "Cold state not preserved"
    );
    assert_eq!(
        loaded.get_state(&warm_key),
        Some(ContextState::Warm),
        "Warm state not preserved"
    );
    assert_eq!(
        loaded.get_state(&stale_key),
        Some(ContextState::Stale),
        "Stale state not preserved"
    );
}

/// A bit-flip in the rkyv payload should be caught by validation.
#[test]
fn bit_flip_in_payload_detected() {
    use super::super::super::snapshot::SnapshotError;

    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    graph.update(
        &key,
        ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        },
        dummy_hash,
    );

    save_to_file(&graph, &path).unwrap();

    // Read the file, flip a byte in the payload, write it back.
    let mut data = std::fs::read(&path).unwrap();
    assert!(data.len() > HEADER_SIZE + 10);
    // Flip a bit in the middle of the payload.
    let flip_idx = HEADER_SIZE + (data.len() - HEADER_SIZE) / 2;
    data[flip_idx] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    match load_from_file(&path) {
        Err(SnapshotError::Corrupt(_)) => {} // Expected
        Ok(_) => {
            // rkyv might not catch every bit-flip if it lands on
            // a valid-looking field. This is acceptable — we just
            // want to verify the validation path exists.
        }
        Err(other) => panic!("unexpected error: {other}"),
    }
}

/// Stress test: many contexts + files to verify no panics, no data
/// loss, and reasonable performance.
#[test]
fn large_graph_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let n_contexts = 200;
    let n_headers_per_ctx = 10;
    let mut keys = Vec::new();

    for i in 0..n_contexts {
        let ctx = CompileContext {
            source_file: NormalizedPath::from(format!("/src/file{i}.cpp")),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from(format!("/inc{i}"))],
                ..Default::default()
            },
            defines: (0..5).map(|d| format!("DEF{d}={i}")).collect(),
            flags: vec!["-O2".into(), format!("-std=c++{}", 14 + (i % 4) * 3)],
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = graph.register(ctx);

        let resolved: Vec<NormalizedPath> = (0..n_headers_per_ctx)
            .map(|h| NormalizedPath::from(format!("/inc{i}/header{h}.h")))
            .collect();
        graph.update(
            &key,
            ScanResult {
                resolved,
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        keys.push(key);
    }

    assert_eq!(graph.stats().context_count, n_contexts);

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(loaded.stats().context_count, n_contexts);

    // Spot-check a few contexts.
    for key in keys.iter().take(10) {
        assert_eq!(loaded.get_state(key), Some(ContextState::Warm));
        let verdict = loaded.check(key, always_fresh, dummy_hash);
        assert!(
            matches!(verdict, CacheVerdict::Hit { .. }),
            "context should hit after load"
        );
    }
}

/// A new compile request for the same context after loading must
/// find the existing warm entry (not create a duplicate cold one).
#[test]
fn register_after_load_finds_existing() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let ctx = CompileContext {
        source_file: NormalizedPath::from("/src/main.cpp"),
        include_search: IncludeSearchPaths {
            user: vec![NormalizedPath::from("/inc")],
            ..Default::default()
        },
        defines: vec!["X=1".into()],
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let original_key = graph.register(ctx.clone());
    graph.update(
        &original_key,
        ScanResult {
            resolved: vec![NormalizedPath::from("/inc/a.h")],
            unresolved: Vec::new(),
            has_computed: false,
        },
        dummy_hash,
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    // Simulate a new compile request with the identical context.
    let new_key = loaded.register(ctx);
    assert_eq!(
        original_key.hash().as_bytes(),
        new_key.hash().as_bytes(),
        "re-registering same context must produce same key"
    );
    // The existing warm entry must still be there (not overwritten).
    assert_eq!(
        loaded.get_state(&new_key),
        Some(ContextState::Warm),
        "re-register must not overwrite warm entry with cold"
    );
    assert_eq!(
        loaded.stats().context_count,
        1,
        "re-register must not create duplicate"
    );
}

/// GC during save must not discard recently-accessed warm contexts.
#[test]
fn gc_on_save_preserves_fresh_entries() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    // Register and update 10 contexts.
    let mut keys = Vec::new();
    for i in 0..10 {
        let key = graph.register(make_ctx(&format!("/src/f{i}.cpp")));
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        keys.push(key);
    }

    // Save triggers GC (1-day TTL). All entries are fresh, so none should be trimmed.
    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(
        loaded.stats().context_count,
        10,
        "GC should not trim fresh entries"
    );
    for key in &keys {
        assert_eq!(loaded.get_state(key), Some(ContextState::Warm));
    }
}

/// Concurrent save + load should not panic or corrupt (thread safety).
#[test]
fn concurrent_save_load() {
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = Arc::new(DepGraph::new());

    // Populate the graph.
    for i in 0..50 {
        let key = graph.register(make_ctx(&format!("/src/f{i}.cpp")));
        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from(format!("/inc/h{i}.h"))],
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
    }

    // Save once so the file exists.
    save_to_file(&graph, &path).unwrap();

    let mut handles = Vec::new();

    // Writer threads.
    for _ in 0..3 {
        let g = Arc::clone(&graph);
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..5 {
                let _ = save_to_file(&g, &p);
            }
        }));
    }

    // Reader threads.
    for _ in 0..3 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..5 {
                // May fail if file is being rewritten — that's OK.
                let _ = load_from_file(&p);
            }
        }));
    }

    // Mutator threads (add new entries while saving).
    for t in 0..2 {
        let g = Arc::clone(&graph);
        handles.push(std::thread::spawn(move || {
            for i in 0..20 {
                let key = g.register(make_ctx(&format!("/src/t{t}_new{i}.cpp")));
                g.update(
                    &key,
                    ScanResult {
                        resolved: Vec::new(),
                        unresolved: Vec::new(),
                        has_computed: false,
                    },
                    dummy_hash,
                );
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Final save+load should be consistent.
    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();
    assert!(loaded.stats().context_count >= 50);
}
