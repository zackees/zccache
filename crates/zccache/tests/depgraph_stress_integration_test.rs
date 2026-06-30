//! Integration tests for zccache-depgraph: full build simulations.
//!
//! All tests are `#[ignore]` — run with `uv run test --full` or
//! `soldr cargo test -p zccache-depgraph --test stress_test -- --ignored`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

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
