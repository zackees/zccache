//! Worktree-instance isolation regressions for the dependency graph.

use std::path::Path;
use std::sync::{Arc, Barrier};

use zccache_core::NormalizedPath;
use zccache_hash::{hash_bytes, ContentHash};

use super::super::context::{CompileContext, ContextKey};
use super::super::scanner::ScanResult;
use super::super::search_paths::IncludeSearchPaths;
use super::{CacheVerdict, ContextState, DepGraph};

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

fn equivalent_hash(path: &Path) -> Option<ContentHash> {
    Some(hash_bytes(
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .as_bytes(),
    ))
}

#[test]
fn equivalent_worktree_b_update_preserves_a_artifact() {
    let graph = DepGraph::new();
    let root_a = NormalizedPath::from("/worktree-a");
    let root_b = NormalizedPath::from("/worktree-b");
    let a = graph.register_with_root_result(context("/worktree-a"), Some(root_a));
    assert_eq!(a.state, ContextState::Cold);
    let artifact_a = graph
        .update(&a.map_key, scan("/worktree-a"), equivalent_hash)
        .expect("A must become warm");

    let b = graph.register_with_root_result(context("/worktree-b"), Some(root_b));
    assert!(b.rebased_from_equivalent_root);
    assert_eq!(
        b.state,
        ContextState::Warm,
        "registration reports the state cloned from the equivalent root"
    );
    assert_eq!(
        a.key, b.key,
        "equivalent roots retain one artifact identity"
    );
    assert_ne!(
        a.map_key, b.map_key,
        "mutable state must be checkout-specific"
    );
    assert!(matches!(
        graph.check(&b.map_key, |_| true, equivalent_hash),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
    ));

    let changed_b = |path: &Path| {
        let bytes: &[u8] = if path.ends_with("lib.rs") {
            b"B changed"
        } else {
            b"same header"
        };
        Some(hash_bytes(bytes))
    };
    let artifact_b = graph
        .update(&b.map_key, scan("/worktree-b"), changed_b)
        .expect("B update must succeed");
    assert_ne!(artifact_a, artifact_b, "edited B needs a distinct artifact");

    assert!(matches!(
        graph.check(&a.map_key, |_| true, equivalent_hash),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
    ));
    assert_eq!(graph.stats().context_count, 2);
}

#[test]
fn equivalent_rustc_worktree_rebases_env_dependencies() {
    let graph = DepGraph::new();
    let logical_key = ContextKey::from_raw([0x31; 32]);
    let env_names = vec!["DYLINT_METADATA".to_string()];
    let env_value = |_: &str| Some("same-metadata".to_string());

    let a = graph.register_rustc_with_key_and_root_result(
        logical_key,
        context("/rustc-env-a"),
        Some(NormalizedPath::from("/rustc-env-a")),
        Vec::new(),
        None,
    );
    let artifact_a = graph
        .update_with_env(
            &a.map_key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            equivalent_hash,
            &env_names,
            env_value,
        )
        .expect("A must become warm");

    let b = graph.register_rustc_with_key_and_root_result(
        logical_key,
        context("/rustc-env-b"),
        Some(NormalizedPath::from("/rustc-env-b")),
        Vec::new(),
        None,
    );
    assert!(b.rebased_from_equivalent_root);
    assert_eq!(b.state, ContextState::Warm);
    assert_eq!(
        graph.get_rustc_env_deps(&b.map_key),
        graph.get_rustc_env_deps(&a.map_key),
        "the rebased artifact key and its env inputs must stay together"
    );
    assert!(matches!(
        graph.check_with_env(&b.map_key, |_| true, equivalent_hash, env_value),
        CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
    ));
    assert!(
        matches!(
            graph.check_with_env(
                &b.map_key,
                |_| true,
                equivalent_hash,
                |_| Some("changed-metadata".to_string()),
            ),
            CacheVerdict::Cold
        ),
        "a changed compile-time env input must still miss safely"
    );
}

#[test]
fn registration_reports_existing_stale_state_without_an_extra_lookup() {
    let graph = DepGraph::new();
    let root = NormalizedPath::from("/stale-worktree");
    let first = graph.register_with_root_result(context("/stale-worktree"), Some(root.clone()));
    graph
        .update(&first.map_key, scan("/stale-worktree"), equivalent_hash)
        .expect("context must become warm");
    assert!(graph.mark_stale(&first.map_key));

    let refreshed = graph.register_with_root_result(context("/stale-worktree"), Some(root));
    assert_eq!(refreshed.map_key, first.map_key);
    assert_eq!(refreshed.state, ContextState::Stale);
}

#[test]
fn concurrent_equivalent_registration_keeps_independent_instances() {
    let graph = Arc::new(DepGraph::new());
    let barrier = Arc::new(Barrier::new(2));
    let mut joins = Vec::new();
    for root in ["/concurrent-a", "/concurrent-b"] {
        let graph = Arc::clone(&graph);
        let barrier = Arc::clone(&barrier);
        joins.push(std::thread::spawn(move || {
            let registration =
                graph.register_with_root_result(context(root), Some(NormalizedPath::from(root)));
            barrier.wait();
            graph
                .update(&registration.map_key, scan(root), equivalent_hash)
                .expect("concurrent update must succeed");
            registration
        }));
    }
    let first = joins.remove(0).join().unwrap();
    let second = joins.remove(0).join().unwrap();
    assert_eq!(first.key, second.key);
    assert_ne!(first.map_key, second.map_key);
    assert_eq!(graph.stats().context_count, 2);
    assert_eq!(graph.get_state(&first.map_key), Some(ContextState::Warm));
    assert_eq!(graph.get_state(&second.map_key), Some(ContextState::Warm));
}

#[test]
fn equivalent_variant_bound_evicts_to_a_conservative_miss() {
    // zackees/soldr#2436 D11: the bound is resolver-driven (default 16,
    // ZCCACHE_MAX_EQUIVALENT_CONTEXTS override); register one past it.
    let limit = crate::graph::register::max_equivalent_contexts();
    let graph = DepGraph::new();
    let mut registrations = Vec::new();
    for index in 0..=limit {
        let root = format!("/evict-{index}");
        let registration = graph
            .register_with_root_result(context(&root), Some(NormalizedPath::from(root.as_str())));
        graph.update(&registration.map_key, scan(&root), equivalent_hash);
        registrations.push(registration);
    }
    assert_eq!(graph.stats().context_count, limit);
    assert!(matches!(
        graph.check(&registrations[0].map_key, |_| true, equivalent_hash),
        CacheVerdict::Cold
    ));
    for registration in registrations.iter().skip(1) {
        assert_eq!(
            graph.get_state(&registration.map_key),
            Some(ContextState::Warm)
        );
    }
}

#[test]
fn rustc_metadata_compatibility_aliases_are_checkout_specific() {
    let graph = DepGraph::new();
    let logical_key = ContextKey::from_raw([0x11; 32]);
    let compat_key = ContextKey::from_raw([0x22; 32]);
    let root_a = NormalizedPath::from("/compat-a");
    let root_b = NormalizedPath::from("/compat-b");

    let a = graph.register_rustc_with_key_and_root_result(
        logical_key,
        context("/compat-a"),
        Some(root_a),
        Vec::new(),
        Some(compat_key),
    );
    let b = graph.register_rustc_with_key_and_root_result(
        logical_key,
        context("/compat-b"),
        Some(root_b),
        Vec::new(),
        Some(compat_key),
    );

    let alias_a = a.metadata_compat_map_key.expect("A compat alias");
    let alias_b = b.metadata_compat_map_key.expect("B compat alias");
    assert_eq!(
        alias_a,
        DepGraph::rustc_metadata_compat_map_key(
            compat_key,
            &context("/compat-a").source_file,
            Some(&NormalizedPath::from("/compat-a")),
        )
    );
    assert_eq!(
        alias_b,
        DepGraph::rustc_metadata_compat_map_key(
            compat_key,
            &context("/compat-b").source_file,
            Some(&NormalizedPath::from("/compat-b")),
        )
    );
    assert_ne!(alias_a, alias_b);
    assert_eq!(
        graph
            .rustc_check_metadata_compat
            .get(&alias_a)
            .map(|entry| *entry),
        Some(a.map_key)
    );
    assert_eq!(
        graph
            .rustc_check_metadata_compat
            .get(&alias_b)
            .map(|entry| *entry),
        Some(b.map_key)
    );
}
