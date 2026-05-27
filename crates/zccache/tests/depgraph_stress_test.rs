//! Integration, stress, and adversarial tests for zccache-depgraph.
//!
//! All tests are `#[ignore]` — run with `uv run test --full` or
//! `soldr cargo test -p zccache-depgraph --test stress_test -- --ignored`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::thread;
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
// INTEGRATION TESTS: Full build simulation
// ---------------------------------------------------------------------------

/// Simulate a complete build pipeline: ingest compile_commands, cold-path scan,
/// warm-path hit, source edit, header edit, rescan, and verify all fast paths.
#[test]
#[ignore]
fn integration_full_build_pipeline() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // -- Set up a "project" on disk --
    // Note: create include dir first so we can canonicalize it.
    std::fs::create_dir_all(root.join("include")).unwrap();
    let inc = canon(&root.join("include"));

    // Headers: common.h includes types.h, types.h is a leaf.
    let common_h = create_file(
        root,
        "include/common.h",
        "#pragma once\n#include \"types.h\"\nvoid common_func();\n",
    );
    let types_h = create_file(
        root,
        "include/types.h",
        "#pragma once\ntypedef int MyInt;\n",
    );

    // Source files.
    let main_cpp = create_file(
        root,
        "src/main.cpp",
        "#include \"common.h\"\nint main() { return 0; }\n",
    );
    let util_cpp = create_file(
        root,
        "src/util.cpp",
        "#include \"types.h\"\nMyInt util() { return 42; }\n",
    );

    // -- Build the graph --
    let graph = DepGraph::new();

    let search = IncludeSearchPaths {
        user: vec![inc.clone()],
        ..Default::default()
    };

    let ctx_main = CompileContext {
        source_file: main_cpp.clone(),
        include_search: search.clone(),
        defines: vec!["NDEBUG".into()],
        flags: vec!["-std=c++17".into()],
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let ctx_util = CompileContext {
        source_file: util_cpp.clone(),
        include_search: search.clone(),
        defines: vec!["NDEBUG".into()],
        flags: vec!["-std=c++17".into()],
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };

    let key_main = graph.register(ctx_main);
    let key_util = graph.register(ctx_util);

    // -- Phase 1: Cold path --
    let verdict = graph.check(&key_main, |_| true, disk_hash_oracle());
    assert!(
        matches!(verdict, CacheVerdict::Cold),
        "first check should be Cold"
    );

    let verdict = graph.check(&key_util, |_| true, disk_hash_oracle());
    assert!(matches!(verdict, CacheVerdict::Cold));

    // Scan main.cpp recursively.
    let scan_main = scanner::scan_recursive(&main_cpp, &search);
    assert!(
        !scan_main.resolved.is_empty(),
        "main.cpp should include headers"
    );
    assert!(
        contains_path(&scan_main.resolved, &common_h),
        "main.cpp -> common.h"
    );
    assert!(
        contains_path(&scan_main.resolved, &types_h),
        "main.cpp -> common.h -> types.h"
    );
    assert!(!scan_main.has_computed, "no computed includes");

    let ak_main = graph.update(&key_main, scan_main.clone(), disk_hash_oracle());
    assert!(ak_main.is_some(), "should produce artifact key");

    // Scan util.cpp.
    let scan_util = scanner::scan_recursive(&util_cpp, &search);
    assert!(contains_path(&scan_util.resolved, &types_h));
    assert!(
        !contains_path(&scan_util.resolved, &common_h),
        "util.cpp doesn't include common.h"
    );

    let ak_util = graph.update(&key_util, scan_util.clone(), disk_hash_oracle());
    assert!(ak_util.is_some());

    // Artifact keys should differ (different source files).
    assert_ne!(ak_main.unwrap(), ak_util.unwrap());

    // -- Phase 2: Warm path — everything fresh --
    let no_stale = HashSet::new();
    let verdict = graph.check(&key_main, freshness_oracle(&no_stale), disk_hash_oracle());
    match &verdict {
        CacheVerdict::Hit { artifact_key } => {
            assert_eq!(*artifact_key, ak_main.unwrap());
        }
        other => panic!("expected Hit, got {other:?}"),
    }

    let verdict = graph.check(&key_util, freshness_oracle(&no_stale), disk_hash_oracle());
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));

    // -- Phase 3: Source file changes --
    std::fs::write(
        &main_cpp,
        "#include \"common.h\"\nint main() { return 1; }\n",
    )
    .unwrap();

    let stale_main: HashSet<String> = [normalize_for_key(main_cpp.as_path())].into();
    let verdict = graph.check(&key_main, freshness_oracle(&stale_main), disk_hash_oracle());
    match &verdict {
        CacheVerdict::SourceChanged { artifact_key } => {
            // New artifact key because content changed.
            assert_ne!(*artifact_key, ak_main.unwrap());
        }
        other => panic!("expected SourceChanged, got {other:?}"),
    }

    // util.cpp is unaffected.
    let verdict = graph.check(&key_util, freshness_oracle(&no_stale), disk_hash_oracle());
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));

    // -- Phase 4: Header changes --
    std::fs::write(&types_h, "#pragma once\ntypedef long MyInt;\n").unwrap();

    // Both contexts should detect the header change.
    let stale_types: HashSet<String> = [normalize_for_key(types_h.as_path())].into();
    let verdict = graph.check(
        &key_main,
        freshness_oracle(&stale_types),
        disk_hash_oracle(),
    );
    match &verdict {
        CacheVerdict::HeadersChanged { changed } => {
            assert!(contains_path(changed, &types_h));
        }
        other => panic!("expected HeadersChanged for main, got {other:?}"),
    }

    let verdict = graph.check(
        &key_util,
        freshness_oracle(&stale_types),
        disk_hash_oracle(),
    );
    assert!(
        matches!(verdict, CacheVerdict::HeadersChanged { .. }),
        "util should also detect types.h change"
    );

    // -- Phase 5: Rescan after header change --
    // Rescan main.cpp (header content changed but include list didn't).
    let rescan = scanner::scan_recursive(&main_cpp, &search);
    assert_eq!(
        rescan.resolved.len(),
        scan_main.resolved.len(),
        "include list should be unchanged"
    );
    let ak_main_v2 = graph.update(&key_main, rescan, disk_hash_oracle());
    assert_ne!(
        ak_main_v2.unwrap(),
        ak_main.unwrap(),
        "artifact key should change after header edit"
    );

    // Now check again — should be a hit with the new key.
    let verdict = graph.check(&key_main, freshness_oracle(&no_stale), disk_hash_oracle());
    match &verdict {
        CacheVerdict::Hit { artifact_key } => {
            assert_eq!(*artifact_key, ak_main_v2.unwrap());
        }
        other => panic!("expected Hit after rescan, got {other:?}"),
    }

    // -- Phase 6: Stats --
    let stats = graph.stats();
    assert_eq!(stats.context_count, 2);
    assert!(stats.checks > 0);
    assert!(stats.hits > 0);
    assert!(stats.misses > 0);
}

/// Simulate adding a new header that shadows an existing one.
#[test]
#[ignore]
fn integration_shadow_detection_real_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create dirs first so we can canonicalize.
    std::fs::create_dir_all(root.join("low")).unwrap();
    std::fs::create_dir_all(root.join("high")).unwrap();
    let low_dir = canon(&root.join("low"));
    let high_dir = canon(&root.join("high"));

    // foo.h exists only in /low initially.
    let foo_h_low = create_file(root, "low/foo.h", "// low priority foo\n");
    let main_cpp = create_file(root, "src/main.cpp", "#include \"foo.h\"\n");

    let graph = DepGraph::new();
    let search = IncludeSearchPaths {
        user: vec![high_dir.clone(), low_dir.clone()],
        ..Default::default()
    };

    let ctx = CompileContext {
        source_file: main_cpp.clone(),
        include_search: search.clone(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    // Cold scan — foo.h resolves from /low.
    let scan = scanner::scan_recursive(&main_cpp, &search);
    assert!(contains_path(&scan.resolved, &foo_h_low));
    graph.update(&key, scan, disk_hash_oracle());

    // Verify warm hit.
    let verdict = graph.check(&key, |_| true, disk_hash_oracle());
    assert!(matches!(verdict, CacheVerdict::Hit { .. }));

    // Now create foo.h in /high — this should shadow /low/foo.h.
    let foo_h_high = create_file(root, "high/foo.h", "// high priority foo\n");
    // Mark stale and rescan.
    graph.mark_stale(&key);
    let rescan = scanner::scan_recursive(&main_cpp, &search);
    assert!(
        contains_path(&rescan.resolved, &foo_h_high),
        "after rescan, foo.h should resolve from /high"
    );
    assert!(
        !contains_path(&rescan.resolved, &foo_h_low),
        "low/foo.h should no longer be in the include list"
    );
}

/// Simulate a new file resolving a previously unresolved include.
#[test]
#[ignore]
fn integration_new_resolve_real_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let main_cpp = create_file(
        root,
        "src/main.cpp",
        "#include \"exists.h\"\n#include \"missing.h\"\n",
    );
    let _exists_h = create_file(root, "include/exists.h", "// exists\n");
    let inc_dir = canon(&root.join("include"));

    let graph = DepGraph::new();
    let search = IncludeSearchPaths {
        user: vec![inc_dir.clone()],
        ..Default::default()
    };

    let ctx = CompileContext {
        source_file: main_cpp.clone(),
        include_search: search.clone(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);

    // Cold scan — missing.h is unresolved.
    let scan = scanner::scan_recursive(&main_cpp, &search);
    assert!(
        scan.unresolved.iter().any(|u| u == "missing.h"),
        "missing.h should be unresolved"
    );
    graph.update(&key, scan, disk_hash_oracle());

    // Create missing.h.
    let missing_h = create_file(root, "include/missing.h", "// now it exists\n");
    let affected = graph.check_new_resolve(&missing_h);
    assert!(
        affected.contains(&key),
        "should detect that missing.h is now resolvable"
    );
}

/// Full session lifecycle: create, compile, check, end, cleanup.
#[test]
#[ignore]
fn integration_session_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let _h = create_file(root, "include/api.h", "#pragma once\nint api();\n");
    let inc = canon(&root.join("include"));
    let src = create_file(
        root,
        "src/lib.cpp",
        "#include \"api.h\"\nint api() { return 0; }\n",
    );

    // Set up session manager.
    let mgr = SessionManager::new(Duration::from_secs(900));
    let graph = DepGraph::new();

    // Discover system includes (simulated).
    let fake_compiler_output = r#"
#include "..." search starts here:
#include <...> search starts here:
 /usr/include
 /usr/local/include
End of search list.
"#;
    let sys_includes = parse_system_include_output(fake_compiler_output);
    assert_eq!(sys_includes.len(), 2);

    // Create session.
    let session_id = mgr.create(SessionConfig {
        client_pid: std::process::id(),
        working_dir: root.to_path_buf().into(),
        log_file: None,
        track_stats: false,
        journal_path: None,
        profile: false,
        private_env: Vec::new(),
        owner_pids: Vec::new(),
    });
    assert!(mgr.exists(&session_id));

    // Register compilation context.
    let search = IncludeSearchPaths {
        user: vec![inc.clone()],
        system: sys_includes,
        ..Default::default()
    };
    let ctx = CompileContext {
        source_file: src.clone(),
        include_search: search.clone(),
        defines: Vec::new(),
        flags: vec!["-O2".into()],
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };
    let key = graph.register(ctx);
    mgr.add_context(&session_id, key);
    mgr.touch(&session_id);

    assert_eq!(mgr.context_count(&session_id), Some(1));

    // Cold → scan → warm.
    assert!(matches!(
        graph.check(&key, |_| true, disk_hash_oracle()),
        CacheVerdict::Cold
    ));
    let scan = scanner::scan_recursive(&src, &search);
    graph.update(&key, scan, disk_hash_oracle());
    assert!(matches!(
        graph.check(&key, |_| true, disk_hash_oracle()),
        CacheVerdict::Hit { .. }
    ));

    // End session.
    let ended = mgr.end(&session_id);
    assert!(ended.is_some());
    assert!(!mgr.exists(&session_id));

    // Graph survives across sessions.
    assert_eq!(graph.stats().context_count, 1);
    assert!(matches!(
        graph.check(&key, |_| true, disk_hash_oracle()),
        CacheVerdict::Hit { .. }
    ));
}

/// compile_commands.json ingest with real files and subsequent scanning.
#[test]
#[ignore]
fn integration_ingest_and_scan() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    create_file(
        root,
        "include/config.h",
        "#pragma once\n#define VERSION 1\n",
    );
    create_file(
        root,
        "src/a.cpp",
        "#include \"config.h\"\nint a() { return VERSION; }\n",
    );
    create_file(
        root,
        "src/b.cpp",
        "#include \"config.h\"\nint b() { return VERSION + 1; }\n",
    );

    let inc = canon(&root.join("include"));
    let src_a = canon(&root.join("src/a.cpp"));
    let src_b = canon(&root.join("src/b.cpp"));

    // Build compile_commands JSON.
    // Use forward slashes to avoid JSON escape issues on Windows.
    let json_path = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let json = format!(
        r#"[
            {{
                "directory": "{}",
                "command": "g++ -I{} -DNDEBUG -std=c++17 -c {} -o a.o",
                "file": "{}"
            }},
            {{
                "directory": "{}",
                "command": "g++ -I{} -DNDEBUG -std=c++17 -c {} -o b.o",
                "file": "{}"
            }}
        ]"#,
        json_path(root),
        json_path(&inc),
        json_path(&src_a),
        json_path(&src_a),
        json_path(root),
        json_path(&inc),
        json_path(&src_b),
        json_path(&src_b),
    );

    let commands = parse_compile_commands_json(&json).unwrap();
    assert_eq!(commands.len(), 2);

    let graph = DepGraph::new();
    let keys = graph.ingest_compile_commands(&commands, &[]);
    assert_eq!(keys.len(), 2);
    assert_eq!(graph.stats().context_count, 2);

    // All contexts start Cold.
    for key in &keys {
        assert!(matches!(
            graph.check(key, |_| true, disk_hash_oracle()),
            CacheVerdict::Cold
        ));
    }

    // Scan each and update.
    for (i, key) in keys.iter().enumerate() {
        let source = if i == 0 { &src_a } else { &src_b };
        let search = IncludeSearchPaths {
            user: vec![inc.clone()],
            ..Default::default()
        };
        let scan = scanner::scan_recursive(source, &search);
        assert!(!scan.resolved.is_empty());
        graph.update(key, scan, disk_hash_oracle());
    }

    // All should be warm now.
    for key in &keys {
        assert!(matches!(
            graph.check(key, |_| true, disk_hash_oracle()),
            CacheVerdict::Hit { .. }
        ));
    }

    // Watch set should have directories for includes and sources.
    let ws = graph.watch_set();
    assert!(
        ws.dir_count() >= 2,
        "watch set should have dirs for includes and sources, got {}",
        ws.dir_count()
    );
}

// ---------------------------------------------------------------------------
// STRESS TESTS: Concurrent operations
// ---------------------------------------------------------------------------

/// 8 threads × 200 contexts each, all registering, scanning, and checking.
#[test]
#[ignore]
fn stress_concurrent_register_scan_check() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create a shared header.
    create_file(root, "include/shared.h", "#pragma once\nint shared();\n");
    let inc = canon(&root.join("include"));

    let graph = Arc::new(DepGraph::new());
    let root = root.to_path_buf();
    let mut handles = Vec::new();

    for t in 0..8 {
        let graph = Arc::clone(&graph);
        let root = root.clone();
        let inc = inc.clone();

        handles.push(thread::spawn(move || {
            let search = IncludeSearchPaths {
                user: vec![inc.clone()],
                ..Default::default()
            };

            for i in 0..200 {
                // Create a unique source file for this thread+index.
                let src_path = root.join(format!("src/t{t}/f{i}.cpp"));
                if let Some(parent) = src_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(
                    &src_path,
                    format!("#include \"shared.h\"\nint f{i}() {{ return {i}; }}\n"),
                )
                .unwrap();

                let ctx = CompileContext {
                    source_file: src_path.clone().into(),
                    include_search: search.clone(),
                    defines: vec![format!("THREAD={t}")],
                    flags: vec!["-O2".into()],
                    force_includes: Vec::new(),
                    unknown_flags: Vec::new(),
                };

                let key = graph.register(ctx);

                // Cold check.
                let v = graph.check(&key, |_| true, disk_hash_oracle());
                assert!(matches!(v, CacheVerdict::Cold));

                // Scan.
                let scan = scanner::scan_recursive(&src_path, &search);
                graph.update(&key, scan, disk_hash_oracle());

                // Warm check.
                let v = graph.check(&key, |_| true, disk_hash_oracle());
                assert!(
                    matches!(v, CacheVerdict::Hit { .. }),
                    "thread {t}, file {i}: expected Hit after scan, got {v:?}"
                );
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    let stats = graph.stats();
    assert_eq!(stats.context_count, 1600); // 8 * 200
    assert_eq!(stats.checks, 3200); // 2 checks per context
    assert_eq!(stats.hits, 1600);
    assert_eq!(stats.misses, 1600); // cold checks
}

/// Concurrent register + trim — trim shouldn't corrupt the graph.
#[test]
#[ignore]
fn stress_concurrent_register_and_trim() {
    let graph = Arc::new(DepGraph::new());
    let mut handles = Vec::new();

    // Writers: register contexts.
    for t in 0..4 {
        let graph = Arc::clone(&graph);
        handles.push(thread::spawn(move || {
            for i in 0..500 {
                let ctx = CompileContext {
                    source_file: np(format!("/src/t{t}/f{i}.cpp")),
                    include_search: IncludeSearchPaths::default(),
                    defines: Vec::new(),
                    flags: Vec::new(),
                    force_includes: Vec::new(),
                    unknown_flags: Vec::new(),
                };
                let key = graph.register(ctx);

                let scan = ScanResult {
                    resolved: vec![np(format!("/inc/t{t}_h{i}.h"))],
                    unresolved: Vec::new(),
                    has_computed: false,
                };
                graph.update(&key, scan, |p: &Path| {
                    Some(zccache::hash::hash_bytes(p.to_string_lossy().as_bytes()))
                });
            }
        }));
    }

    // Trimmer: runs trim concurrently.
    let graph_trimmer = Arc::clone(&graph);
    handles.push(thread::spawn(move || {
        for _ in 0..100 {
            // Trim with a short duration — shouldn't crash even with concurrent writes.
            graph_trimmer.trim(Duration::from_millis(1));
            thread::sleep(Duration::from_millis(1));
        }
    }));

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Graph should be consistent — no panics, and stats should make sense.
    let stats = graph.stats();
    // Some entries may have been trimmed, but the graph shouldn't be corrupted.
    assert!(stats.context_count <= 2000);
}

/// Concurrent shadow detection while contexts are being registered.
#[test]
#[ignore]
fn stress_concurrent_shadow_detection() {
    let graph = Arc::new(DepGraph::new());
    let mut handles = Vec::new();

    // Writers: register contexts with includes in /low.
    for t in 0..4 {
        let graph = Arc::clone(&graph);
        handles.push(thread::spawn(move || {
            for i in 0..100 {
                let search = IncludeSearchPaths {
                    user: vec![np("/high"), np("/low")],
                    ..Default::default()
                };
                let ctx = CompileContext {
                    source_file: np(format!("/src/t{t}/f{i}.cpp")),
                    include_search: search,
                    defines: Vec::new(),
                    flags: Vec::new(),
                    force_includes: Vec::new(),
                    unknown_flags: Vec::new(),
                };
                let key = graph.register(ctx);

                let scan = ScanResult {
                    resolved: vec![np(format!("/low/h{i}.h"))],
                    unresolved: Vec::new(),
                    has_computed: false,
                };
                graph.update(&key, scan, |p: &Path| {
                    Some(zccache::hash::hash_bytes(p.to_string_lossy().as_bytes()))
                });
            }
        }));
    }

    // Shadow checker: runs concurrently.
    let graph_checker = Arc::clone(&graph);
    handles.push(thread::spawn(move || {
        for i in 0..100 {
            // Check if a new file in /high shadows /low.
            let shadows = graph_checker.check_shadow(Path::new(&format!("/high/h{i}.h")));
            // Don't assert specific counts — contexts may or may not be
            // registered yet. Just verify no panics.
            let _ = shadows.len();
        }
    }));

    for h in handles {
        h.join().expect("thread panicked");
    }
}

/// Concurrent session create/end/cleanup.
#[test]
#[ignore]
fn stress_concurrent_session_operations() {
    let mgr = Arc::new(SessionManager::new(Duration::from_millis(100)));
    let mut handles = Vec::new();

    // Creators.
    for t in 0..4 {
        let mgr = Arc::clone(&mgr);
        handles.push(thread::spawn(move || {
            let mut ids = Vec::new();
            for i in 0..100 {
                let id = mgr.create(SessionConfig {
                    client_pid: (t * 1000 + i) as u32,
                    working_dir: np(format!("/project/{t}")),
                    log_file: None,
                    track_stats: false,
                    journal_path: None,
                    profile: false,
                    private_env: Vec::new(),
                    owner_pids: Vec::new(),
                });
                ids.push(id);
                mgr.touch(&id);
            }
            // End half of them.
            for id in ids.iter().take(50) {
                mgr.end(id);
            }
        }));
    }

    // Cleaner.
    let mgr_cleaner = Arc::clone(&mgr);
    handles.push(thread::spawn(move || {
        for _ in 0..50 {
            mgr_cleaner.cleanup_expired();
            thread::sleep(Duration::from_millis(2));
        }
    }));

    for h in handles {
        h.join().expect("thread panicked");
    }

    // After all threads finish, remaining sessions should be consistent.
    let count = mgr.active_count();
    let ids = mgr.active_ids();
    assert_eq!(count, ids.len(), "active_count and active_ids should agree");
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
