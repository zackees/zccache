//! Concurrent stress tests for zccache-depgraph.
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

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;
use zccache::core::NormalizedPath;
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

/// Hash a file's content with blake3 via zccache-hash.
fn hash_file(path: &Path) -> Option<zccache::hash::ContentHash> {
    let data = std::fs::read(path).ok()?;
    Some(zccache::hash::hash_bytes(&data))
}

/// Build a hash oracle that reads files from disk.
fn disk_hash_oracle() -> impl Fn(&Path) -> Option<zccache::hash::ContentHash> {
    |p: &Path| hash_file(p)
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
