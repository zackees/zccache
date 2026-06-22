//! Focused snapshot ↔ load roundtrip tests, one per field/concern.
//!
//! Each test here exercises a single attribute of the snapshot format
//! (a specific field, a specific `IncludeKind` variant, empty values,
//! exact byte equality) so a regression points directly at the broken
//! field. Cross-cutting behavior across save/load lives in `behavioral.rs`.

use crate::core::NormalizedPath;
use crate::hash::ContentHash;
use tempfile::TempDir;

use super::super::super::context::CompileContext;
use super::super::super::graph::{ContextState, DepGraph};
use super::super::super::scanner::{IncludeDirective, IncludeKind, ScanResult};
use super::super::super::search_paths::IncludeSearchPaths;
use super::super::super::snapshot::{load_from_file, save_to_file};
use super::{make_ctx, test_path};

#[test]
fn empty_graph_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let stats = loaded.stats();
    assert_eq!(stats.file_count, 0);
    assert_eq!(stats.context_count, 0);
}

#[test]
fn populated_graph_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    // Add file entries with all IncludeKind variants.
    graph.store_file_includes(
        NormalizedPath::from("/src/main.cpp"),
        vec![
            IncludeDirective {
                kind: IncludeKind::Quoted,
                path: "header.h".into(),
                line: 1,
            },
            IncludeDirective {
                kind: IncludeKind::AngleBracket,
                path: "vector".into(),
                line: 2,
            },
            IncludeDirective {
                kind: IncludeKind::Computed("PLATFORM_HEADER".into()),
                path: "PLATFORM_HEADER".into(),
                line: 3,
            },
        ],
    );

    // Add a context entry with all fields populated.
    let ctx = CompileContext {
        source_file: NormalizedPath::from("/src/main.cpp"),
        include_search: IncludeSearchPaths {
            iquote: vec![NormalizedPath::from("/src")],
            user: vec![NormalizedPath::from("/include")],
            system: vec![NormalizedPath::from("/usr/include")],
            after: vec![NormalizedPath::from("/after")],
        },
        defines: vec!["DEBUG=1".into()],
        flags: vec!["-std=c++17".into()],
        force_includes: vec![NormalizedPath::from("/pch.h")],
        unknown_flags: vec!["--custom".into()],
    };
    let key = graph.register(ctx);

    // Update with resolved includes and file hashes.
    let source_hash = crate::hash::hash_bytes(b"source content");
    let header_hash = crate::hash::hash_bytes(b"header content");
    let pch_hash = crate::hash::hash_bytes(b"pch content");
    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
        (NormalizedPath::from("/src/main.cpp"), source_hash),
        (NormalizedPath::from("/include/header.h"), header_hash),
        (NormalizedPath::from("/pch.h"), pch_hash),
    ]
    .into_iter()
    .collect();

    graph.update(
        &key,
        ScanResult {
            resolved: vec![NormalizedPath::from("/include/header.h")],
            unresolved: vec!["missing.h".into()],
            has_computed: true,
        },
        |path| hashes.get(&NormalizedPath::new(path)).copied(),
    );

    // Save and load.
    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let stats = loaded.stats();
    assert_eq!(stats.file_count, 1);
    assert_eq!(stats.context_count, 1);

    // Verify file entry.
    let includes = loaded
        .get_file_includes(&NormalizedPath::from("/src/main.cpp"))
        .unwrap();
    assert_eq!(includes.len(), 3);
    assert_eq!(includes[0].kind, IncludeKind::Quoted);
    assert_eq!(includes[1].kind, IncludeKind::AngleBracket);
    assert!(matches!(includes[2].kind, IncludeKind::Computed(_)));

    // Verify context state survived.
    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
    let resolved = loaded.get_includes(&key).unwrap();
    assert_eq!(resolved, vec![NormalizedPath::from("/include/header.h")]);
}

#[test]
fn last_file_hashes_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    let hash1 = crate::hash::hash_bytes(b"content1");
    let hash2 = crate::hash::hash_bytes(b"content2");
    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
        (NormalizedPath::from("/src/a.cpp"), hash1),
        (NormalizedPath::from("/inc/b.h"), hash2),
    ]
    .into_iter()
    .collect();

    graph.update(
        &key,
        ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        },
        |path| hashes.get(&NormalizedPath::new(path)).copied(),
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    // Verify context survived with file hashes.
    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));

    // Verify hashes via snapshot inspection.
    let snap = loaded.to_snapshot();
    assert_eq!(snap.contexts[0].last_file_hashes.len(), 2);
}

#[test]
fn artifact_key_some_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/c.cpp"));
    let hash = crate::hash::hash_bytes(b"source");
    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> =
        [(NormalizedPath::from("/src/c.cpp"), hash)]
            .into_iter()
            .collect();

    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        |path| hashes.get(&NormalizedPath::new(path)).copied(),
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let snap = loaded.to_snapshot();
    assert!(
        snap.contexts[0].artifact_key.is_some(),
        "artifact_key should survive roundtrip"
    );
}

#[test]
fn rustc_externs_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let ctx = make_ctx("/src/app.rs");
    let key = ctx.context_key();
    let externs = vec![
        (
            "dep".to_string(),
            NormalizedPath::from("/target/debug/deps/libdep.rlib"),
        ),
        (
            "cc".to_string(),
            NormalizedPath::from("/target/debug/deps/libcc.rlib"),
        ),
    ];

    graph.register_rustc_with_key_and_root_result(key, ctx, None, externs.clone(), None);
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        super::dummy_hash,
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(loaded.get_rustc_externs(&key), Some(externs));
}

/// Artifact key=None (Cold context) must roundtrip as None.
#[test]
fn artifact_key_none_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    // Register but don't update — artifact_key stays None.
    graph.register(make_ctx("/src/cold.cpp"));

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let snap = loaded.to_snapshot();
    assert_eq!(snap.contexts.len(), 1);
    assert!(
        snap.contexts[0].artifact_key.is_none(),
        "Cold context should have artifact_key=None"
    );
}

/// Unresolved includes (strings, not paths) must roundtrip.
#[test]
fn unresolved_includes_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: vec!["missing1.h".into(), "subdir/missing2.h".into(), "".into()],
            has_computed: false,
        },
        super::dummy_hash,
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let snap = loaded.to_snapshot();
    assert_eq!(
        snap.contexts[0].unresolved_includes,
        vec!["missing1.h", "subdir/missing2.h", ""]
    );
}

/// has_computed_includes flag must roundtrip for both true and false.
#[test]
fn has_computed_includes_roundtrip() {
    use super::super::super::graph::CacheVerdict;

    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key_with = graph.register(make_ctx("/src/with_computed.cpp"));
    graph.update(
        &key_with,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: true,
        },
        super::dummy_hash,
    );

    let key_without = graph.register(make_ctx("/src/without_computed.cpp"));
    graph.update(
        &key_without,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        super::dummy_hash,
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let snap = loaded.to_snapshot();
    let with_computed = NormalizedPath::new("/src/with_computed.cpp")
        .display()
        .to_string();
    let without_computed = NormalizedPath::new("/src/without_computed.cpp")
        .display()
        .to_string();
    let ctx_with = snap
        .contexts
        .iter()
        .find(|c| c.source_file == with_computed)
        .unwrap();
    let ctx_without = snap
        .contexts
        .iter()
        .find(|c| c.source_file == without_computed)
        .unwrap();
    assert!(ctx_with.has_computed_includes);
    assert!(!ctx_without.has_computed_includes);

    // Warm context with has_computed must return NeedsPreprocessor on check.
    let verdict = loaded.check(&key_with, super::always_fresh, super::dummy_hash);
    assert!(
        matches!(verdict, CacheVerdict::NeedsPreprocessor),
        "computed includes should force preprocessor, got {verdict:?}"
    );
}

/// All three IncludeKind variants in file entries must roundtrip,
/// including the inner string of Computed.
#[test]
fn include_kind_computed_inner_string_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let macro_name = "MY_PLATFORM_HEADER";
    graph.store_file_includes(
        NormalizedPath::from("/src/test.cpp"),
        vec![
            IncludeDirective {
                kind: IncludeKind::Quoted,
                path: "local.h".into(),
                line: 1,
            },
            IncludeDirective {
                kind: IncludeKind::AngleBracket,
                path: "system.h".into(),
                line: 2,
            },
            IncludeDirective {
                kind: IncludeKind::Computed(macro_name.into()),
                path: macro_name.into(),
                line: 3,
            },
        ],
    );

    // Need a context that references this file so trim doesn't remove it.
    let key = graph.register(make_ctx("/src/test.cpp"));
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: true,
        },
        super::dummy_hash,
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let includes = loaded
        .get_file_includes(&NormalizedPath::from("/src/test.cpp"))
        .unwrap();
    assert_eq!(includes.len(), 3);
    assert_eq!(includes[0].kind, IncludeKind::Quoted);
    assert_eq!(includes[0].path, "local.h");
    assert_eq!(includes[1].kind, IncludeKind::AngleBracket);
    assert_eq!(includes[1].path, "system.h");
    match &includes[2].kind {
        IncludeKind::Computed(inner) => {
            assert_eq!(
                inner, macro_name,
                "Computed inner string must survive roundtrip"
            );
        }
        other => panic!("expected Computed, got {other:?}"),
    }
    assert_eq!(includes[2].line, 3);
}

/// Empty strings in all fields must not cause panics or data loss.
#[test]
fn empty_strings_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let ctx = CompileContext {
        source_file: NormalizedPath::from(""),
        include_search: IncludeSearchPaths {
            iquote: vec![NormalizedPath::from("")],
            user: vec![NormalizedPath::from("")],
            system: vec![NormalizedPath::from("")],
            after: vec![NormalizedPath::from("")],
        },
        defines: vec![String::new()],
        flags: vec![String::new()],
        force_includes: vec![NormalizedPath::from("")],
        unknown_flags: vec![String::new()],
    };
    let key = graph.register(ctx);

    // Empty path hash.
    let hash = crate::hash::hash_bytes(b"");
    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> =
        [(NormalizedPath::from(""), hash)].into_iter().collect();
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: vec![String::new()],
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));

    let snap = loaded.to_snapshot();
    assert_eq!(snap.contexts[0].source_file, "");
    assert_eq!(snap.contexts[0].defines, vec![""]);
    assert_eq!(snap.contexts[0].unresolved_includes, vec![""]);
}

/// File hashes must roundtrip with exact byte equality.
#[test]
fn file_hash_bytes_exact_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    let source_hash = crate::hash::hash_bytes(b"specific source content 12345");
    let header_hash = crate::hash::hash_bytes(b"specific header content 67890");

    let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
        (NormalizedPath::from("/src/a.cpp"), source_hash),
        (NormalizedPath::from("/inc/b.h"), header_hash),
    ]
    .into_iter()
    .collect();

    graph.update(
        &key,
        ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        },
        |p| hashes.get(&NormalizedPath::new(p)).copied(),
    );

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let snap = loaded.to_snapshot();
    let ctx = &snap.contexts[0];

    // Verify each hash byte-for-byte.
    for (snap_path, snap_hash) in &ctx.last_file_hashes {
        let expected = hashes.get(&NormalizedPath::from(snap_path)).unwrap();
        assert_eq!(
            snap_hash,
            expected.as_bytes(),
            "hash mismatch for {snap_path}"
        );
    }
}

/// Artifact key bytes must be identical after roundtrip.
#[test]
fn artifact_key_bytes_exact_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    let artifact = graph
        .update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            super::dummy_hash,
        )
        .unwrap();

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let snap = loaded.to_snapshot();
    let loaded_artifact_bytes = snap.contexts[0].artifact_key.unwrap();
    assert_eq!(
        &loaded_artifact_bytes,
        artifact.hash().as_bytes(),
        "artifact key bytes must be identical after roundtrip"
    );
}

/// Stats counters must reset to zero after load (not carry forward
/// stale hit/miss data).
#[test]
fn stats_reset_after_load() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();

    let key = graph.register(make_ctx("/src/a.cpp"));
    graph.update(
        &key,
        ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        super::dummy_hash,
    );
    // Generate some stats.
    graph.check(&key, super::always_fresh, super::dummy_hash);
    graph.check(&key, super::always_fresh, super::dummy_hash);
    assert_eq!(graph.stats().checks, 2);
    assert_eq!(graph.stats().hits, 2);

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();

    let stats = loaded.stats();
    assert_eq!(stats.checks, 0, "checks must reset on load");
    assert_eq!(stats.hits, 0, "hits must reset on load");
    assert_eq!(stats.misses, 0, "misses must reset on load");
}
