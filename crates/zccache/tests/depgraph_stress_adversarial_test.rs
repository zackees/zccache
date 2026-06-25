//! Adversarial tests for zccache-depgraph: edge cases and error conditions.
//!
//! All tests are `#[ignore]` — run with `uv run test --full` or
//! `soldr cargo test -p zccache-depgraph --test stress_test -- --ignored`.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use zccache::core::{normalize_for_key, NormalizedPath};
use zccache::depgraph::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Canonicalize a path (resolves symlinks, UNC prefix on Windows).
/// On Windows, TempDir paths differ from canonicalized scanner output.
fn canon(p: &Path) -> NormalizedPath {
    NormalizedPath::new(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
}

/// Create a file with the given content. Creates parent dirs if needed.
/// Returns the canonicalized path so it matches scanner output.
fn create_file(base: &Path, rel: &str, content: &str) -> NormalizedPath {
    let path = base.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    canon(&path)
}

fn np(path: impl AsRef<Path>) -> NormalizedPath {
    NormalizedPath::new(path)
}

fn contains_path<T: AsRef<Path>>(paths: &[T], path: &NormalizedPath) -> bool {
    let expected = normalize_for_key(path.as_path());
    paths
        .iter()
        .any(|p| normalize_for_key(p.as_ref()) == expected)
}

/// Hash a file's content with blake3 via zccache-hash.
fn hash_file(path: &Path) -> Option<zccache::hash::ContentHash> {
    let data = std::fs::read(path).ok()?;
    Some(zccache::hash::hash_bytes(&data))
}

/// Build a freshness oracle from a set of "stale" paths.
fn freshness_oracle(stale: &HashSet<String>) -> impl Fn(&Path) -> bool + '_ {
    move |p: &Path| !stale.contains(&normalize_for_key(p))
}

/// Build a hash oracle that reads files from disk.
fn disk_hash_oracle() -> impl Fn(&Path) -> Option<zccache::hash::ContentHash> {
    |p: &Path| hash_file(p)
}

// ---------------------------------------------------------------------------
// ADVERSARIAL TESTS: Edge cases and error conditions
// ---------------------------------------------------------------------------

/// Circular includes: a.h -> b.h -> c.h -> a.h. Must not infinite-loop.
#[test]
#[ignore]
fn adversarial_circular_includes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    create_file(root, "include/a.h", "#include \"b.h\"\nint a();\n");
    create_file(root, "include/b.h", "#include \"c.h\"\nint b();\n");
    create_file(root, "include/c.h", "#include \"a.h\"\nint c();\n");
    let main_cpp = create_file(root, "src/main.cpp", "#include \"a.h\"\nint main() {}\n");
    let inc = canon(&root.join("include"));

    let search = IncludeSearchPaths {
        user: vec![inc],
        ..Default::default()
    };

    // Must complete without hanging.
    let start = Instant::now();
    let scan = scanner::scan_recursive(&main_cpp, &search);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "scan took too long: {elapsed:?}"
    );
    assert_eq!(scan.resolved.len(), 3, "should find all 3 headers");
    assert!(scan.unresolved.is_empty());
    assert!(!scan.has_computed);

    // Graph should handle this fine.
    let graph = DepGraph::new();
    let ctx = CompileContext {
        source_file: main_cpp.clone(),
        include_search: search,
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);
    graph.update(&key, scan, disk_hash_oracle());

    let verdict = graph.check(&key, |_| true, disk_hash_oracle());
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));
}

/// Deeply nested includes: a.h -> b.h -> ... -> z.h (26 levels).
#[test]
#[ignore]
fn adversarial_deep_include_chain() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create a chain: a.h includes b.h, b.h includes c.h, etc.
    for i in 0..26u8 {
        let name = (b'a' + i) as char;
        let next = if i < 25 {
            format!("#include \"{}.h\"\n", (b'a' + i + 1) as char)
        } else {
            String::new()
        };
        create_file(
            root,
            &format!("include/{name}.h"),
            &format!("{next}int {name}();\n"),
        );
    }
    let main_cpp = create_file(root, "src/main.cpp", "#include \"a.h\"\nint main() {}\n");
    let inc = canon(&root.join("include"));

    let search = IncludeSearchPaths {
        user: vec![inc],
        ..Default::default()
    };

    let scan = scanner::scan_recursive(&main_cpp, &search);
    assert_eq!(scan.resolved.len(), 26, "should find all 26 headers");
}

/// Computed include (#include MACRO) should trigger NeedsPreprocessor.
#[test]
#[ignore]
fn adversarial_computed_include_propagates() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    create_file(root, "include/normal.h", "int normal();\n");
    create_file(
        root,
        "include/computed.h",
        "#include GENERATED_HEADER\nint computed();\n",
    );
    let main_cpp = create_file(
        root,
        "src/main.cpp",
        "#include \"normal.h\"\n#include \"computed.h\"\n",
    );
    let inc = canon(&root.join("include"));

    let search = IncludeSearchPaths {
        user: vec![inc],
        ..Default::default()
    };

    let scan = scanner::scan_recursive(&main_cpp, &search);
    assert!(scan.has_computed, "should detect computed include");

    let graph = DepGraph::new();
    let ctx = CompileContext {
        source_file: main_cpp,
        include_search: search,
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);
    graph.update(&key, scan, disk_hash_oracle());

    let verdict = graph.check(&key, |_| true, disk_hash_oracle());
    assert!(
        matches!(verdict, CacheVerdict::NeedsPreprocessor),
        "computed include should force preprocessor: {verdict:?}"
    );
}

/// Same header shared across many contexts — verify it's scanned once but
/// the graph tracks it correctly for each context.
#[test]
#[ignore]
fn adversarial_shared_header_many_contexts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let shared_h = create_file(root, "include/shared.h", "#pragma once\nint shared();\n");
    let inc = canon(&root.join("include"));

    let graph = DepGraph::new();
    let search = IncludeSearchPaths {
        user: vec![inc],
        ..Default::default()
    };

    let mut keys = Vec::new();

    for i in 0..100 {
        let src = create_file(root, &format!("src/f{i}.cpp"), "#include \"shared.h\"\n");

        let ctx = CompileContext {
            source_file: src.clone(),
            include_search: search.clone(),
            defines: vec![format!("FILE_ID={i}")],
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = graph.register(ctx);

        let scan = scanner::scan_recursive(&src, &search);
        assert!(contains_path(&scan.resolved, &shared_h));
        graph.update(&key, scan, disk_hash_oracle());
        keys.push(key);
    }

    // All 100 contexts should be warm.
    for key in &keys {
        assert!(matches!(
            graph.check(key, |_| true, disk_hash_oracle()),
            CacheVerdict::Hit { .. }
        ));
    }

    // Change shared.h — all 100 contexts should detect it.
    std::fs::write(&shared_h, "#define SHARED 2\n").unwrap();
    let stale: HashSet<String> = [normalize_for_key(shared_h.as_path())].into();
    let mut changed_count = 0;
    for key in &keys {
        let v = graph.check(key, freshness_oracle(&stale), disk_hash_oracle());
        if matches!(v, CacheVerdict::HeadersChanged { .. }) {
            changed_count += 1;
        }
    }
    assert_eq!(
        changed_count, 100,
        "all 100 contexts should detect shared.h change"
    );
}

/// Rapid state transitions: cold → warm → stale → warm → stale → ...
/// Verifies the state machine doesn't get stuck or confused.
#[test]
#[ignore]
fn adversarial_rapid_state_transitions() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let src = create_file(root, "src/main.cpp", "#include \"api.h\"\n");
    let api_h = create_file(root, "include/api.h", "int api();\n");
    let inc = canon(&root.join("include"));

    let graph = DepGraph::new();
    let search = IncludeSearchPaths {
        user: vec![inc],
        ..Default::default()
    };

    let ctx = CompileContext {
        source_file: src.clone(),
        include_search: search.clone(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    for cycle in 0..50 {
        // Cold or stale → scan → warm.
        let scan = scanner::scan_recursive(&src, &search);
        graph.update(&key, scan, disk_hash_oracle());
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

        // Check → hit.
        let v = graph.check(&key, |_| true, disk_hash_oracle());
        assert!(
            matches!(v, CacheVerdict::Hit { .. }),
            "cycle {cycle}: expected Hit"
        );

        // Edit header → check detects change → stale.
        std::fs::write(&api_h, format!("int api_v{cycle}();\n")).unwrap();
        let stale: HashSet<String> = [normalize_for_key(api_h.as_path())].into();
        let v = graph.check(&key, freshness_oracle(&stale), disk_hash_oracle());
        assert!(
            matches!(v, CacheVerdict::HeadersChanged { .. }),
            "cycle {cycle}: expected HeadersChanged"
        );
        assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
    }
}

/// Empty compile_commands.json should not crash or produce invalid state.
#[test]
#[ignore]
fn adversarial_empty_compile_commands() {
    let graph = DepGraph::new();
    let commands = parse_compile_commands_json("[]").unwrap();
    let keys = graph.ingest_compile_commands(&commands, &[]);
    assert!(keys.is_empty());
    assert_eq!(graph.stats().context_count, 0);
}

/// Source file with no includes — should still work correctly.
#[test]
#[ignore]
fn adversarial_no_includes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let src = create_file(root, "src/standalone.cpp", "int main() { return 0; }\n");

    let graph = DepGraph::new();
    let search = IncludeSearchPaths::default();
    let ctx = CompileContext {
        source_file: src.clone(),
        include_search: search.clone(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    let scan = scanner::scan_recursive(&src, &search);
    assert!(scan.resolved.is_empty());
    assert!(scan.unresolved.is_empty());
    assert!(!scan.has_computed);

    graph.update(&key, scan, disk_hash_oracle());

    let v = graph.check(&key, |_| true, disk_hash_oracle());
    assert!(matches!(v, CacheVerdict::Hit { .. }));

    // Verify artifact key is stable.
    if let CacheVerdict::Hit { artifact_key: ak1 } = v {
        let v2 = graph.check(&key, |_| true, disk_hash_oracle());
        if let CacheVerdict::Hit { artifact_key: ak2 } = v2 {
            assert_eq!(ak1, ak2, "artifact key should be stable for unchanged file");
        }
    }
}

/// File with only unresolved includes (e.g., all behind #ifdef guards that
/// reference headers not in any search path).
#[test]
#[ignore]
fn adversarial_all_includes_unresolved() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let src = create_file(
        root,
        "src/main.cpp",
        "#include \"nonexistent1.h\"\n#include \"nonexistent2.h\"\n#include <nowhere.h>\n",
    );

    let graph = DepGraph::new();
    let search = IncludeSearchPaths::default(); // no search dirs at all
    let ctx = CompileContext {
        source_file: src.clone(),
        include_search: search.clone(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    let scan = scanner::scan_recursive(&src, &search);
    assert!(scan.resolved.is_empty());
    assert_eq!(scan.unresolved.len(), 3);

    graph.update(&key, scan, disk_hash_oracle());

    // Should still be warm and produce a hit (no headers to check).
    let v = graph.check(&key, |_| true, disk_hash_oracle());
    assert!(matches!(v, CacheVerdict::Hit { .. }));
}

/// Watch set correctness under bulk load.
#[test]
#[ignore]
fn adversarial_watch_set_large_graph() {
    let graph = DepGraph::new();

    // Register 500 contexts with diverse include paths.
    for i in 0..500 {
        let group = i / 50;
        let search = IncludeSearchPaths {
            user: vec![
                np(format!("/project/group{group}/include")),
                np("/shared/include"),
            ],
            system: vec![np("/usr/include")],
            ..Default::default()
        };

        let ctx = CompileContext {
            source_file: np(format!("/project/group{group}/src/f{i}.cpp")),
            include_search: search,
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = graph.register(ctx);

        // Simulate includes.
        let scan = ScanResult {
            resolved: vec![
                np(format!("/project/group{group}/include/h{i}.h")),
                np("/shared/include/common.h"),
            ],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, |p: &Path| {
            Some(zccache::hash::hash_bytes(p.to_string_lossy().as_bytes()))
        });
    }

    let ws = graph.watch_set();

    // Should have deduped directories.
    // 10 groups × 2 dirs (src, include) + /shared/include + /usr/include = at most ~32
    let dir_count = ws.dir_count();
    assert!(dir_count < 50, "expected < 50 unique dirs, got {dir_count}");

    // /shared/include should be watched.
    assert!(ws.is_watched(Path::new("/shared/include")));
    assert!(ws.is_watched(Path::new("/usr/include")));

    // common.h should be tracked.
    assert!(ws.is_tracked(Path::new("/shared/include/common.h")));
}

/// Trim should correctly clean up file entries that are no longer referenced.
#[test]
#[ignore]
fn adversarial_trim_cleans_orphaned_files() {
    let graph = DepGraph::new();

    // Register a context.
    let ctx = CompileContext {
        source_file: np("/src/a.cpp"),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    // Store some file includes.
    graph.store_file_includes(
        np("/inc/old.h"),
        vec![IncludeDirective {
            kind: IncludeKind::Quoted,
            path: "nested.h".into(),
            line: 1,
        }],
    );

    // Update context with resolved includes pointing to different files.
    let scan = ScanResult {
        resolved: vec![np("/inc/new.h")],
        unresolved: Vec::new(),
        has_computed: false,
    };
    graph.update(&key, scan, |p: &Path| {
        Some(zccache::hash::hash_bytes(p.to_string_lossy().as_bytes()))
    });

    // Trim everything (Duration::ZERO means all entries are older than cutoff).
    let removed = graph.trim(Duration::ZERO);
    assert_eq!(removed, 1);

    // File entries should also be cleaned up (no contexts reference them).
    let stats = graph.stats();
    assert_eq!(
        stats.file_count, 0,
        "orphaned file entries should be cleaned"
    );
    assert_eq!(stats.context_count, 0);
}

/// Deterministic artifact keys across repeated identical builds.
#[test]
#[ignore]
fn adversarial_deterministic_artifact_keys() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    create_file(root, "include/a.h", "#pragma once\nint a();\n");
    create_file(
        root,
        "include/b.h",
        "#pragma once\n#include \"a.h\"\nint b();\n",
    );
    let src = create_file(root, "src/main.cpp", "#include \"b.h\"\nint main() {}\n");
    let inc = canon(&root.join("include"));

    let search = IncludeSearchPaths {
        user: vec![inc],
        ..Default::default()
    };

    // Build 1.
    let graph1 = DepGraph::new();
    let ctx1 = CompileContext {
        source_file: src.clone(),
        include_search: search.clone(),
        defines: vec!["NDEBUG".into()],
        flags: vec!["-O2".into()],
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key1 = graph1.register(ctx1);
    let scan1 = scanner::scan_recursive(&src, &search);
    let ak1 = graph1.update(&key1, scan1, disk_hash_oracle()).unwrap();

    // Build 2 (completely independent graph).
    let graph2 = DepGraph::new();
    let ctx2 = CompileContext {
        source_file: src.clone(),
        include_search: search.clone(),
        defines: vec!["NDEBUG".into()],
        flags: vec!["-O2".into()],
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key2 = graph2.register(ctx2);
    let scan2 = scanner::scan_recursive(&src, &search);
    let ak2 = graph2.update(&key2, scan2, disk_hash_oracle()).unwrap();

    // Same inputs → same context key and artifact key.
    assert_eq!(key1, key2, "context keys should be deterministic");
    assert_eq!(ak1, ak2, "artifact keys should be deterministic");
}
