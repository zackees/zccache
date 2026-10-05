//! Link-count-aware eviction and retired sibling-store bytes for the
//! disk-maintenance pass (issues #1659, #1673, #1687).
//!
//! Split out of `disk_maintenance_unit.rs` so that file keeps headroom under
//! the repo's LOC ceiling. Every fixture it needs (`artifact`, `bytes_policy`,
//! `FixedEnvironment`, `DAY`) stays owned by the parent and arrives through
//! `use super::*`.

use super::*;

/// Plan ordering: an older entry whose files are hard-linked elsewhere frees
/// nothing, so a newer `nlink == 1` entry is selected first.
#[test]
fn issue_1659_pressure_prefers_entries_that_actually_free_space() {
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    let mut shared = artifact("shared", 30, now, 2 * DAY);
    shared.reclaimable_bytes = 0;
    let entries = vec![shared, artifact("unshared", 80, now, DAY)];
    let plan = plan_maintenance(
        bytes_policy(100),
        MaintenanceKind::Pressure,
        now,
        FilesystemSpace {
            capacity_bytes: 1000 * GIB,
            free_bytes: 500 * GIB,
        },
        &entries,
        0,
    );
    assert_eq!(plan.pressure, MaintenancePressure::Hard);
    assert_eq!(plan.selected, vec!["unshared"]);
}

/// Acceptance 5: retired sibling-store bytes push usage over budget; the pass
/// reclaims the retired store and evicts no live entry.
#[test]
fn issue_1659_retired_store_bytes_are_reclaimed_before_live_entries() {
    let root = tempfile::tempdir().unwrap();
    let top_level = root.path().to_path_buf();
    let current = crate::core::config::versioned_subdir();
    let artifact_dir = top_level.join(&current).join("artifacts");
    let retired = top_level.join("v0.0.1");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(retired.join("artifacts")).unwrap();
    let live = artifact_dir.join("key.meta");
    std::fs::write(&live, vec![0_u8; 4096]).unwrap();
    let old = kernal_api::platform::fs::FileTime::from_system_time(SystemTime::now() - 10 * DAY);
    for i in 0..10 {
        let path = retired.join("artifacts").join(format!("old-{i}.meta"));
        std::fs::write(&path, vec![0_u8; 4096]).unwrap();
        kernal_api::platform::fs::set_file_mtime(&path, old).unwrap();
    }
    assert!(retired_store_bytes(&top_level, &current) > 20_000);

    let artifacts = DashMap::new();
    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    let dep_graph = DepGraph::new();
    let report = maintain_disk_artifacts(MaintenancePass {
        artifact_dir: &artifact_dir,
        artifacts: &artifacts,
        artifact_store: &store,
        index_writer_tx: None,
        dep_graph: &dep_graph,
        pending_write_bytes: 0,
        policy: bytes_policy(20_000),
        kind: MaintenanceKind::Pressure,
        environment: &FixedEnvironment {
            now: SystemTime::now(),
            space: FilesystemSpace {
                capacity_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
        },
        retired_top_level: Some((
            crate::core::NormalizedPath::from(top_level.clone()),
            current.clone(),
        )),
    })
    .unwrap();

    assert_eq!(report.artifacts_removed, 0, "no live entry may be evicted");
    assert!(live.exists());
    for i in 0..10 {
        assert!(!retired
            .join("artifacts")
            .join(format!("old-{i}.meta"))
            .exists());
    }
    // #1673: reclaimed bytes are credited only where the volume proves the
    // blocks exclusive; a volume reporting unknown sharing credits none,
    // though the files are still removed and no live entry is evicted.
    if volume_proves_exclusive(root.path()) {
        assert!(report.retired_bytes_reclaimed > 0);
    }
    assert!(report.bytes_reclaimed >= report.retired_bytes_reclaimed);
    assert_eq!(retired_store_bytes(&top_level, &current), 0);
}

/// Whether a fresh file in `dir` is reported `Exclusive`, i.e. whether this
/// volume can prove that removing an `nlink == 1` file frees its space.
fn volume_proves_exclusive(dir: &std::path::Path) -> bool {
    let probe = dir.join("exclusive-probe.bin");
    std::fs::write(&probe, b"probe").unwrap();
    let exclusive = matches!(
        kernal_api::platform::fs::extent_sharing(&probe),
        Ok(kernal_api::platform::fs::ExtentSharing::Exclusive)
    );
    std::fs::remove_file(&probe).unwrap();
    exclusive
}

/// Acceptance 6: evicting an entry whose file is also hard-linked into a
/// build tree frees nothing, so it is not reported as reclaimed, and the
/// build tree's link survives.
#[test]
fn issue_1659_evicting_a_hard_linked_entry_reports_no_reclaimed_bytes() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("artifacts");
    let target = root.path().join("target");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    let cached = artifact_dir.join("key.meta");
    std::fs::write(&cached, vec![0_u8; 4096]).unwrap();
    std::fs::hard_link(&cached, target.join("out.o")).unwrap();

    let artifacts = DashMap::new();
    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    let dep_graph = DepGraph::new();
    let report = maintain_disk_artifacts(MaintenancePass {
        artifact_dir: &artifact_dir,
        artifacts: &artifacts,
        artifact_store: &store,
        index_writer_tx: None,
        dep_graph: &dep_graph,
        pending_write_bytes: 0,
        policy: bytes_policy(1),
        kind: MaintenanceKind::Pressure,
        environment: &FixedEnvironment {
            now: SystemTime::UNIX_EPOCH + 100 * DAY,
            space: FilesystemSpace {
                capacity_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
        },
        retired_top_level: None,
    })
    .unwrap();

    assert_eq!(report.pressure, MaintenancePressure::Hard);
    assert_eq!(report.artifacts_removed, 1);
    assert_eq!(
        report.bytes_reclaimed, 0,
        "the target/ link still holds the blocks"
    );
    assert!(!cached.exists());
    assert_eq!(std::fs::read(target.join("out.o")).unwrap().len(), 4096);
}

/// #1673: a *newer* sibling store belongs to a newer daemon and never counts
/// as reclaimable; only older-version siblings do.
#[test]
fn issue_1673_retired_store_bytes_ignore_newer_sibling_stores() {
    let root = tempfile::tempdir().unwrap();
    let top_level = root.path().to_path_buf();
    std::fs::create_dir_all(top_level.join("v1.0.0")).unwrap();
    let newer = top_level.join("v2.0.0").join("artifacts");
    std::fs::create_dir_all(&newer).unwrap();
    std::fs::write(newer.join("new.meta"), vec![0_u8; 4096]).unwrap();
    assert_eq!(retired_store_bytes(&top_level, "v1.0.0"), 0);

    let older = top_level.join("v0.9.0").join("artifacts");
    std::fs::create_dir_all(&older).unwrap();
    std::fs::write(older.join("old.meta"), vec![0_u8; 4096]).unwrap();
    assert!(retired_store_bytes(&top_level, "v1.0.0") > 0);
}

/// #1687: a cache file reflinked into a build tree keeps `nlink == 1` while
/// sharing every block, so evicting it frees (almost) nothing. The live
/// planner must not credit it as reclaimable; it still counts toward
/// accounted usage (`allocated_bytes`). Runs wherever the temp volume can
/// reflink and prove sharing (APFS on macOS CI, btrfs/XFS, ReFS); loud skip
/// elsewhere.
#[test]
fn issue_1687_reflink_shared_cache_file_is_not_reclaimable() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("owned");
    let target = root.path().join("target");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    let cached = artifact_dir.join("key_0");
    std::fs::write(&cached, vec![7_u8; 256 * 1024]).unwrap();
    let restored = target.join("libkey.rlib");
    if kernal_api::platform::fs::reflink_file(&cached, &restored).is_err()
        || !matches!(
            kernal_api::platform::fs::extent_sharing(&cached),
            Ok(kernal_api::platform::fs::ExtentSharing::Shared)
        )
    {
        eprintln!(
            "SKIP issue_1687_reflink_shared_cache_file_is_not_reclaimable: {} cannot prove reflink extent sharing",
            root.path().display()
        );
        return;
    }
    let scanned = scan_artifacts(&artifact_dir).unwrap();
    assert_eq!(scanned.len(), 1);
    assert!(scanned[0].allocated_bytes > 0);
    assert_eq!(
        scanned[0].reclaimable_bytes, 0,
        "a reflink-shared cache file frees no space when evicted"
    );
    assert_eq!(std::fs::read(&restored).unwrap().len(), 256 * 1024);
}
