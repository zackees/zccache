//! Snapshot regression coverage for equivalent worktree variants.

use std::path::Path;

use tempfile::TempDir;
use zccache_core::NormalizedPath;
use zccache_hash::{hash_bytes, ContentHash};

use super::super::super::context::{CompileContext, ContextKey};
use super::super::super::graph::{CacheVerdict, DepGraph};
use super::super::super::scanner::ScanResult;
use super::super::super::search_paths::IncludeSearchPaths;
use super::super::super::snapshot::{load_from_file, save_to_file};
use super::{always_fresh, test_path};

fn context(root: &str) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(format!("{root}/src/lib.rs").as_str()),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: zccache_hash::hash_bytes(b"test-fixture"),
    }
}

fn scan(root: &str) -> ScanResult {
    ScanResult {
        resolved: vec![NormalizedPath::from(
            format!("{root}/include/shared.h").as_str(),
        )],
        unresolved: Vec::new(),
        has_computed: false,
    }
}

fn equal_hash(path: &Path) -> Option<ContentHash> {
    Some(hash_bytes(
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .as_bytes(),
    ))
}

#[test]
fn equivalent_worktree_variants_survive_snapshot_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    let a = graph.register_with_root_result(
        context("/snapshot-a"),
        Some(NormalizedPath::from("/snapshot-a")),
    );
    let artifact_a = graph
        .update(&a.map_key, scan("/snapshot-a"), equal_hash)
        .unwrap();
    let b = graph.register_with_root_result(
        context("/snapshot-b"),
        Some(NormalizedPath::from("/snapshot-b")),
    );
    let changed_b = |path: &Path| {
        let bytes: &[u8] = if path.ends_with("lib.rs") {
            b"changed B"
        } else {
            b"header"
        };
        Some(hash_bytes(bytes))
    };
    let artifact_b = graph
        .update(&b.map_key, scan("/snapshot-b"), changed_b)
        .unwrap();
    assert_ne!(artifact_a, artifact_b);

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();
    assert_eq!(loaded.stats().context_count, 2);
    assert!(matches!(
        loaded.check(&a.map_key, always_fresh, equal_hash),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
    ));
    assert!(matches!(
        loaded.check(&b.map_key, always_fresh, changed_b),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_b
    ));
}

#[test]
fn new_equivalent_worktree_reuses_warm_context_after_snapshot_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    let a = graph.register_with_root_result(
        context("/snapshot-a"),
        Some(NormalizedPath::from("/snapshot-a")),
    );
    let artifact_a = graph
        .update(&a.map_key, scan("/snapshot-a"), equal_hash)
        .unwrap();

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();
    let b = loaded.register_with_root_result(
        context("/snapshot-b"),
        Some(NormalizedPath::from("/snapshot-b")),
    );

    assert!(
        b.rebased_from_equivalent_root,
        "the restored logical-key index must expose the warm context to a new worktree"
    );
    assert!(matches!(
        loaded.check(&b.map_key, always_fresh, equal_hash),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
    ));
}

#[test]
fn rebased_rustc_env_dependencies_survive_snapshot_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = test_path(&dir);
    let graph = DepGraph::new();
    let logical_key = ContextKey::from_raw([0x42; 32]);
    let env_names = vec!["DYLINT_METADATA".to_string()];
    let env_value = |_: &str| Some("same-metadata".to_string());
    let a = graph.register_rustc_with_key_and_root_result(
        logical_key,
        context("/snapshot-env-a"),
        Some(NormalizedPath::from("/snapshot-env-a")),
        Vec::new(),
        None,
    );
    let artifact_a = graph
        .update_with_env(
            &a.map_key,
            scan("/snapshot-env-a"),
            equal_hash,
            &env_names,
            env_value,
        )
        .unwrap();

    save_to_file(&graph, &path).unwrap();
    let loaded = load_from_file(&path).unwrap();
    let b = loaded.register_rustc_with_key_and_root_result(
        logical_key,
        context("/snapshot-env-b"),
        Some(NormalizedPath::from("/snapshot-env-b")),
        Vec::new(),
        None,
    );

    assert!(b.rebased_from_equivalent_root);
    assert_eq!(
        loaded.get_rustc_env_deps(&b.map_key),
        loaded.get_rustc_env_deps(&a.map_key),
    );
    assert!(matches!(
        loaded.check_with_env(&b.map_key, always_fresh, equal_hash, env_value),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
    ));
}
