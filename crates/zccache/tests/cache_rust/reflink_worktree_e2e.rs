//! `ZCCACHE_MODE` end to end on a real reflink volume (#1683, #1691).
//!
//! A parent worktree compiles a crate cold; a sibling (child) worktree of
//! the same repo then gets a real cache hit through `ZCCACHE_PATH_REMAP`.
//! Each mode's delivery is checked on the child's `.rlib`: REFLINK/AUTO
//! clone (shared extents, private inode), COPY owns its blocks, LINK shares
//! the cache inode. Edits in the child never reach the parent or the cache,
//! and a reflink-shared cache file is not counted as reclaimable (#1687).
//!
//! Runs only where `ZCCACHE_REFLINK_E2E_ROOT` names a directory on a
//! reflink-capable filesystem (the `reflink-e2e` CI job mounts btrfs).
//! `ZCCACHE_REFLINK_E2E_OTHER_ROOT` (another filesystem) adds the
//! cross-device case, where REFLINK must fall back to an independent copy.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use kernal_api::platform::fs::{extent_sharing, ExtentSharing};
use zccache::embedded::{
    AuditConfig, AuditContext, CompileRequest, HostIdentity, MaterializationMode, RuntimeHooks,
    ServiceLimits, ShutdownMode, ZccacheConfig, ZccacheService,
};

const SOURCE: &str = "pub fn answer() -> u32 { 42 }\n";
const RLIB: &str = "target/libe2ecrate.rlib";

fn e2e_root(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

fn worktree(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("src/lib.rs"), SOURCE).unwrap();
    root
}

async fn start(cache_root: &Path) -> ZccacheService {
    let mut audit = AuditConfig::default();
    audit.mode = zccache::audit::AuditMode::Off;
    ZccacheService::start(ZccacheConfig {
        host: HostIdentity {
            product: "reflink-e2e".into(),
            instance_id: "reflink-e2e".into(),
            workspace_id: "reflink-e2e".into(),
        },
        cache_root: cache_root.into(),
        audit,
        limits: ServiceLimits::default(),
        runtime: RuntimeHooks::default(),
        cancellation: None,
    })
    .await
    .expect("embedded service starts")
}

fn request(
    compiler: &zccache::core::NormalizedPath,
    worktree: &Path,
    mode: MaterializationMode,
) -> CompileRequest {
    CompileRequest {
        audit: AuditContext::new(
            zccache::audit::AuditId::new("reflink-e2e").unwrap(),
            zccache::audit::AuditId::new("reflink-e2e-trace").unwrap(),
        ),
        compiler: compiler.clone(),
        args: [
            "--crate-name",
            "e2ecrate",
            "--crate-type=rlib",
            "--emit=metadata,link",
            "--out-dir",
            "target",
            "src/lib.rs",
        ]
        .map(String::from)
        .to_vec(),
        cwd: worktree.into(),
        env: vec![
            ("ZCCACHE_PATH_REMAP".into(), "auto".into()),
            (
                "ZCCACHE_WORKTREE_ROOT".into(),
                worktree.to_string_lossy().into_owned(),
            ),
            ("ZCCACHE_MODE".into(), mode.as_str().into()),
        ],
        stdin: Vec::new(),
    }
}

/// The parent compiles cold and the service restarts (durable store); the
/// child then must hit. Returns the child's delivered `.rlib`.
fn parent_then_child_hit(
    compiler: &zccache::core::NormalizedPath,
    cache_root: &Path,
    parent: &Path,
    child: &Path,
    mode: MaterializationMode,
) -> PathBuf {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let cold = start(cache_root).await;
        let miss = cold.compile(request(compiler, parent, mode)).await.unwrap();
        assert_eq!(miss.exit_code, 0, "{mode}: parent compile failed: {miss:?}");
        cold.shutdown(ShutdownMode::Graceful).await.unwrap();

        let warm = start(cache_root).await;
        let hit = warm.compile(request(compiler, child, mode)).await.unwrap();
        assert_eq!(hit.exit_code, 0, "{mode}: child compile failed: {hit:?}");
        assert!(
            hit.cached,
            "{mode}: the child worktree must hit the parent's cache entry"
        );
        warm.shutdown(ShutdownMode::Graceful).await.unwrap();
    });
    child.join(RLIB)
}

fn links(path: &Path) -> u64 {
    zccache::core::config::file_link_count(path).expect("link count")
}

/// Cache files whose bytes equal `bytes` (the stored blob for an output).
fn cache_copies_of(cache_root: &Path, bytes: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![cache_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64)
                && std::fs::read(&path).is_ok_and(|content| content == bytes)
            {
                found.push(path);
            }
        }
    }
    found
}

#[test]
#[ignore = "e2e: needs ZCCACHE_REFLINK_E2E_ROOT on a reflink filesystem and rustc"]
fn reflink_e2e_parent_child_worktrees_per_mode() {
    let Some(root) = e2e_root("ZCCACHE_REFLINK_E2E_ROOT") else {
        assert!(
            std::env::var_os("ZCCACHE_REQUIRE_REFLINK_E2E").is_none(),
            "ZCCACHE_REFLINK_E2E_ROOT must name a reflink-capable directory"
        );
        eprintln!("SKIP reflink e2e: ZCCACHE_REFLINK_E2E_ROOT is unset");
        return;
    };
    let compiler = zccache::test_support::find_rustc().expect("rustc on PATH");
    for mode in MaterializationMode::ALL {
        let run = tempfile::tempdir_in(&root).unwrap();
        let repo = run.path().join("repo");
        let parent = worktree(&repo, "parent");
        let child = worktree(&repo, "child");
        let cache_root = run.path().join("cache");
        let rlib = parent_then_child_hit(&compiler, &cache_root, &parent, &child, mode);
        let bytes = std::fs::read(&rlib).unwrap();
        let parent_bytes = std::fs::read(parent.join(RLIB)).unwrap();
        assert_eq!(
            bytes, parent_bytes,
            "{mode}: the hit restores identical bytes"
        );
        let blobs = cache_copies_of(&cache_root, &bytes);
        assert!(!blobs.is_empty(), "{mode}: the cache must hold the rlib");

        match mode {
            MaterializationMode::Reflink | MaterializationMode::Auto => {
                assert_eq!(links(&rlib), 1, "{mode}: a clone is its own inode");
                assert_eq!(
                    extent_sharing(&rlib).unwrap(),
                    ExtentSharing::Shared,
                    "{mode}: the child's rlib must share extents with the cache"
                );
                // #1687: the reflinked cache file frees nothing when evicted.
                for blob in &blobs {
                    assert!(
                        !zccache::core::config::file_may_free_space_on_removal(blob),
                        "{mode}: a reflink-shared cache file is not reclaimable: {}",
                        blob.display()
                    );
                }
            }
            MaterializationMode::Copy => {
                assert_eq!(links(&rlib), 1, "COPY output is its own inode");
                assert_eq!(
                    extent_sharing(&rlib).unwrap(),
                    ExtentSharing::Exclusive,
                    "COPY output must own its blocks"
                );
            }
            MaterializationMode::Link => {
                assert!(links(&rlib) >= 2, "LINK shares the cache inode");
            }
        }

        println!(
            "reflink e2e: {mode} child rlib links={} sharing={:?}",
            links(&rlib),
            extent_sharing(&rlib)
        );
        if mode != MaterializationMode::Link {
            // An in-place edit in the child (a later non-wrapped rustc)
            // reaches neither the parent nor the cache.
            std::fs::write(&rlib, b"child edits its own copy").unwrap();
            assert_eq!(
                std::fs::read(parent.join(RLIB)).unwrap(),
                parent_bytes,
                "{mode}"
            );
            assert!(
                blobs
                    .iter()
                    .all(|blob| std::fs::read(blob).unwrap() == parent_bytes),
                "{mode}: the child's edit reached the cache"
            );
        }
    }
}

/// Cache and child on different filesystems: nothing can clone or link, so
/// REFLINK and AUTO deliver an independent copy.
#[test]
#[ignore = "e2e: needs ZCCACHE_REFLINK_E2E_ROOT and ZCCACHE_REFLINK_E2E_OTHER_ROOT"]
fn reflink_e2e_cross_device_falls_back_to_copy() {
    let (Some(root), Some(other)) = (
        e2e_root("ZCCACHE_REFLINK_E2E_ROOT"),
        e2e_root("ZCCACHE_REFLINK_E2E_OTHER_ROOT"),
    ) else {
        assert!(
            std::env::var_os("ZCCACHE_REQUIRE_REFLINK_E2E").is_none(),
            "both e2e roots are required"
        );
        eprintln!("SKIP cross-device e2e: roots unset");
        return;
    };
    let compiler = zccache::test_support::find_rustc().expect("rustc on PATH");
    for mode in [MaterializationMode::Reflink, MaterializationMode::Auto] {
        let run = tempfile::tempdir_in(&root).unwrap();
        let other_run = tempfile::tempdir_in(&other).unwrap();
        let parent = worktree(&run.path().join("repo"), "parent");
        let child = worktree(&other_run.path().join("repo"), "child");
        let cache_root = run.path().join("cache");
        let rlib = parent_then_child_hit(&compiler, &cache_root, &parent, &child, mode);
        assert_eq!(
            links(&rlib),
            1,
            "{mode}: cross-device output is independent"
        );
        assert!(
            !matches!(extent_sharing(&rlib), Ok(ExtentSharing::Shared)),
            "{mode}: nothing can share extents across devices"
        );
        assert_eq!(
            std::fs::read(&rlib).unwrap(),
            std::fs::read(parent.join(RLIB)).unwrap()
        );
    }
}
