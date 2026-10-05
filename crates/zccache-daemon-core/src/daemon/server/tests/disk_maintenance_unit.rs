//! Unit tests for daemon-owned bounded retention (issue #1148).

use super::*;

// Link-count-aware eviction and retired sibling-store bytes (#1659, #1673,
// #1687) live in `disk_maintenance_retired_stores.rs`, split out to keep this
// file under the repo's LOC ceiling. `#[path]` because this file is wired in
// as a `#[path]`-loaded `mod tests`, not as a `tests/mod.rs`.
#[path = "disk_maintenance_retired_stores.rs"]
mod retired_stores;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn artifact(key: &str, bytes: u64, now: SystemTime, age: Duration) -> DiskArtifact {
    DiskArtifact {
        key: key.to_string(),
        allocated_bytes: bytes,
        reclaimable_bytes: bytes,
        last_access: now - age,
        recently_published: false,
        legacy_files: Vec::new(),
        staged: false,
        staged_generation: None,
    }
}

fn bytes_policy(bytes: u64) -> MaintenancePolicy {
    MaintenancePolicy {
        budget: BudgetSpec::Bytes(bytes),
    }
}

struct FixedEnvironment {
    now: SystemTime,
    space: FilesystemSpace,
}

#[tokio::test]
async fn shutdown_wait_observes_atomic_request_without_notify_edge() {
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let waiter_flag = Arc::clone(&shutdown_requested);
    let waiter = tokio::spawn(async move {
        wait_for_next_pass_or_shutdown(
            &waiter_flag,
            Duration::from_secs(5 * 60),
            Duration::from_millis(1),
        )
        .await
    });

    tokio::task::yield_now().await;
    shutdown_requested.store(true, Ordering::Release);

    assert!(tokio::time::timeout(Duration::from_millis(100), waiter)
        .await
        .expect("shutdown waiter should not sleep until the maintenance interval")
        .expect("shutdown waiter task should complete"));
}

impl MaintenanceEnvironment for FixedEnvironment {
    fn now(&self) -> SystemTime {
        self.now
    }

    fn filesystem_space(&self, _root: &Path) -> io::Result<FilesystemSpace> {
        Ok(self.space)
    }
}

struct GatedScanEnvironment {
    now: SystemTime,
    calls: std::sync::atomic::AtomicUsize,
    scan_entered: std::sync::Barrier,
    release_scan: std::sync::Barrier,
}

impl MaintenanceEnvironment for GatedScanEnvironment {
    fn now(&self) -> SystemTime {
        self.now
    }

    fn filesystem_space(&self, _root: &Path) -> io::Result<FilesystemSpace> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
            self.scan_entered.wait();
            self.release_scan.wait();
        }
        Ok(FilesystemSpace {
            capacity_bytes: 1000 * GIB,
            free_bytes: 500 * GIB,
        })
    }
}

#[test]
fn issue_1148_default_budget_is_five_percent_clamped_to_40_200_gib() {
    let policy = MaintenancePolicy::default();
    assert_eq!(policy.budget_bytes(30 * GIB), 15 * GIB);
    assert_eq!(policy.budget_bytes(40 * GIB), 20 * GIB);
    assert_eq!(policy.budget_bytes(100 * GIB), 40 * GIB);
    assert_eq!(policy.budget_bytes(1024 * GIB), 51 * GIB + 214_748_364);
    assert_eq!(policy.budget_bytes(10_000 * GIB), 200 * GIB);
}

#[test]
fn issue_1148_override_parser_rejects_ambiguity_and_invalid_values() {
    assert!(MaintenancePolicy::from_values(Some("1"), Some("5")).is_err());
    assert!(MaintenancePolicy::from_values(Some("0"), None).is_err());
    assert!(MaintenancePolicy::from_values(None, Some("0")).is_err());
    assert!(MaintenancePolicy::from_values(None, Some("101")).is_err());
    assert_eq!(
        MaintenancePolicy::from_values(Some("42949672960"), None)
            .unwrap()
            .budget_bytes(1024 * GIB),
        40 * GIB
    );
    assert!(MaintenancePolicy::from_limits(Some(1), Some(5)).is_err());
    assert!(MaintenancePolicy::from_limits(Some(0), None).is_err());
    assert!(MaintenancePolicy::from_limits(None, Some(101)).is_err());
    assert_eq!(
        MaintenancePolicy::from_values(None, Some("10"))
            .unwrap()
            .budget_bytes(1024 * GIB),
        102 * GIB + 429_496_729
    );
}

#[test]
fn issue_1148_full_marker_makes_missed_idle_pass_due_on_restart() {
    let root = tempfile::tempdir().unwrap();
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    assert!(full_maintenance_due(root.path(), now));
    record_full_maintenance(root.path(), now).unwrap();
    assert!(!full_maintenance_due(
        root.path(),
        now + FULL_INTERVAL - Duration::from_secs(1)
    ));
    assert!(full_maintenance_due(root.path(), now + FULL_INTERVAL));
    assert!(full_maintenance_due(
        root.path(),
        now - Duration::from_secs(1)
    ));
    std::fs::write(full_marker_path(root.path()), b"corrupt\n").unwrap();
    assert!(full_maintenance_due(root.path(), now));
}

#[test]
fn issue_1148_eviction_updates_files_live_map_index_and_only_owned_root() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("owned").join("artifacts");
    let sibling = root.path().join("sibling");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(artifact_dir.join("key.meta"), vec![0_u8; 1024]).unwrap();
    std::fs::write(artifact_dir.join("key_0"), vec![0_u8; 4096]).unwrap();
    std::fs::write(sibling.join("sentinel"), b"owned by another product").unwrap();

    let meta = ArtifactIndex::new(
        vec!["output.o".to_string()],
        vec![4096],
        Vec::new(),
        Vec::new(),
        0,
    );
    let artifacts = DashMap::new();
    artifacts.insert("key".to_string(), CachedArtifact::from_index(meta.clone()));
    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    store.insert("key", &meta);
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
    assert!(report.bytes_reclaimed > 0);
    assert_eq!(report.usage_after_bytes, 0);
    assert!(!artifact_dir.join("key.meta").exists());
    assert!(!artifact_dir.join("key_0").exists());
    assert!(!artifacts.contains_key("key"));
    assert!(store.get("key").is_none());
    assert_eq!(
        std::fs::read(sibling.join("sentinel")).unwrap(),
        b"owned by another product"
    );
}

#[test]
fn read_only_maintenance_scan_does_not_exclude_cache_hit_leases() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::write(artifact_dir.join("old.meta"), vec![0_u8; 4096]).unwrap();
    let artifacts = DashMap::new();
    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    let dep_graph = DepGraph::new();
    let publication_barrier = Arc::new(kernal_api::async_engine::RwLock::new(()));
    let environment = GatedScanEnvironment {
        now: SystemTime::UNIX_EPOCH + 100 * DAY,
        calls: std::sync::atomic::AtomicUsize::new(0),
        scan_entered: std::sync::Barrier::new(2),
        release_scan: std::sync::Barrier::new(2),
    };

    std::thread::scope(|scope| {
        let maintenance = scope.spawn(|| {
            maintain_disk_artifacts_with_barrier(
                MaintenancePass {
                    artifact_dir: &artifact_dir,
                    artifacts: &artifacts,
                    artifact_store: &store,
                    index_writer_tx: None,
                    dep_graph: &dep_graph,
                    pending_write_bytes: 0,
                    policy: bytes_policy(1),
                    kind: MaintenanceKind::Pressure,
                    environment: &environment,
                    retired_top_level: None,
                },
                Some(&publication_barrier),
                None,
            )
        });

        environment.scan_entered.wait();
        let cache_hit_lease = Arc::clone(&publication_barrier)
            .try_read_owned()
            .expect("read-only scan must not queue or hold the publication writer");
        drop(cache_hit_lease);
        environment.release_scan.wait();
        maintenance.join().unwrap().unwrap();
    });
}

#[test]
fn issue_1148_live_and_persisted_access_control_full_expiry() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let meta_path = artifact_dir.join("key.meta");
    let payload_path = artifact_dir.join("key_0");
    std::fs::write(&meta_path, vec![0_u8; 1024]).unwrap();
    std::fs::write(&payload_path, vec![0_u8; 4096]).unwrap();
    let now = SystemTime::now();
    let old = now - 31 * DAY;
    let old_time = kernal_api::platform::fs::FileTime::from_system_time(old);
    kernal_api::platform::fs::set_file_mtime(&meta_path, old_time).unwrap();
    kernal_api::platform::fs::set_file_mtime(&payload_path, old_time).unwrap();

    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    let dep_graph = DepGraph::new();
    let environment = FixedEnvironment {
        now,
        space: FilesystemSpace {
            capacity_bytes: 1000 * GIB,
            free_bytes: 500 * GIB,
        },
    };
    let fresh_meta = ArtifactIndex::new(
        vec!["output.o".to_string()],
        vec![4096],
        Vec::new(),
        Vec::new(),
        0,
    );
    let artifacts = DashMap::new();
    artifacts.insert(
        "key".to_string(),
        CachedArtifact::from_index(fresh_meta.clone()),
    );
    store.insert("key", &fresh_meta);

    let protected = maintain_disk_artifacts(MaintenancePass {
        artifact_dir: &artifact_dir,
        artifacts: &artifacts,
        artifact_store: &store,
        index_writer_tx: None,
        dep_graph: &dep_graph,
        pending_write_bytes: 0,
        policy: bytes_policy(1000 * GIB),
        kind: MaintenanceKind::Full,
        environment: &environment,
        retired_top_level: None,
    })
    .unwrap();
    assert_eq!(protected.artifacts_removed, 0);
    assert!(meta_path.exists());

    artifacts.clear();
    store.clear();
    let mut stale_meta = fresh_meta;
    stale_meta.stored_at_secs = old
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    artifacts.insert(
        "key".to_string(),
        CachedArtifact::from_index(stale_meta.clone()),
    );
    store.insert("key", &stale_meta);
    let expired = maintain_disk_artifacts(MaintenancePass {
        artifact_dir: &artifact_dir,
        artifacts: &artifacts,
        artifact_store: &store,
        index_writer_tx: None,
        dep_graph: &dep_graph,
        pending_write_bytes: 0,
        policy: bytes_policy(1000 * GIB),
        kind: MaintenanceKind::Full,
        environment: &environment,
        retired_top_level: None,
    })
    .unwrap();
    assert_eq!(expired.expired_artifacts_removed, 1);
    assert!(!meta_path.exists());
    assert!(!payload_path.exists());
}

#[test]
fn issue_1148_soft_pressure_only_removes_entries_older_than_four_days() {
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    let entries = vec![
        artifact("boundary", 10, now, 4 * DAY),
        artifact("stale", 20, now, 4 * DAY + Duration::from_secs(1)),
        artifact("fresh", 60, now, DAY),
    ];
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
    assert_eq!(plan.pressure, MaintenancePressure::Soft);
    assert_eq!(plan.selected, vec!["stale"]);
}

#[test]
fn issue_1148_hard_pressure_removes_fresh_lru_to_eighty_percent() {
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    let entries = vec![
        artifact("old", 30, now, 2 * DAY),
        artifact("new", 80, now, DAY),
    ];
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
    assert_eq!(plan.selected, vec!["old"]);
}

#[test]
fn issue_1148_full_pass_expires_only_older_than_thirty_days() {
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    let entries = vec![
        artifact("boundary", 10, now, 30 * DAY),
        artifact("expired", 10, now, 30 * DAY + Duration::from_secs(1)),
    ];
    let plan = plan_maintenance(
        bytes_policy(1000),
        MaintenanceKind::Full,
        now,
        FilesystemSpace {
            capacity_bytes: 1000 * GIB,
            free_bytes: 500 * GIB,
        },
        &entries,
        0,
    );
    assert_eq!(plan.pressure, MaintenancePressure::None);
    assert_eq!(plan.selected, vec!["expired"]);
}

#[cfg(unix)]
#[test]
fn issue_1148_hardlinks_are_counted_once_and_sibling_root_is_untouched() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("owned");
    let sibling = root.path().join("sibling");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(artifact_dir.join("key.meta"), vec![0_u8; 4096]).unwrap();
    std::fs::hard_link(artifact_dir.join("key.meta"), artifact_dir.join("key_0")).unwrap();
    std::fs::write(sibling.join("sentinel"), b"do not inspect or delete").unwrap();
    let scanned = scan_artifacts(&artifact_dir).unwrap();
    assert_eq!(scanned.len(), 1);
    let meta_path = artifact_dir.join("key.meta");
    let allocated = crate::platform::fs::volume::allocated_bytes(
        &meta_path,
        &std::fs::metadata(&meta_path).unwrap(),
    );
    assert_eq!(scanned[0].allocated_bytes, allocated);
    assert_eq!(
        std::fs::read(sibling.join("sentinel")).unwrap(),
        b"do not inspect or delete"
    );
}

#[test]
fn issue_1148_under_budget_and_healthy_disk_is_a_noop() {
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    let entries = vec![artifact("fresh", 49, now, DAY)];
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
    assert_eq!(plan.pressure, MaintenancePressure::None);
    assert!(plan.selected.is_empty());
}

#[test]
fn issue_1191_hard_pressure_preserves_seconds_old_artifacts() {
    let now = SystemTime::UNIX_EPOCH + 100 * DAY;
    let mut entries = vec![artifact("fresh", 10, now, Duration::from_secs(1))];
    entries[0].recently_published = true;
    let plan = plan_maintenance(
        bytes_policy(40 * GIB),
        MaintenanceKind::Pressure,
        now,
        FilesystemSpace {
            capacity_bytes: 100 * GIB,
            free_bytes: 19 * GIB,
        },
        &entries,
        0,
    );
    assert_eq!(plan.pressure, MaintenancePressure::Hard);
    assert!(plan.selected.is_empty());
}

#[test]
fn issue_1148_private_publication_temps_are_never_cache_entries() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".abc_0.tmp-123-4"), vec![0_u8; 4096]).unwrap();
    std::fs::write(root.path().join(".cowhash-deadbeef"), b"digest").unwrap();
    assert!(scan_artifacts(root.path()).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn issue_1148_cross_artifact_hardlink_replans_until_target_is_real() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let first = "a".repeat(64);
    let second = "b".repeat(64);
    let first_path = artifact_dir.join(format!("{first}.meta"));
    let second_path = artifact_dir.join(format!("{second}.meta"));
    std::fs::write(&first_path, vec![0_u8; 4096]).unwrap();
    std::fs::hard_link(&first_path, &second_path).unwrap();

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
            now: SystemTime::now(),
            space: FilesystemSpace {
                capacity_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
        },
        retired_top_level: None,
    })
    .unwrap();

    assert_eq!(report.artifacts_removed, 2);
    assert_eq!(report.usage_after_bytes, 0);
    assert!(!first_path.exists());
    assert!(!second_path.exists());
}

#[test]
fn issue_1148_mixed_legacy_pack_and_staged_layouts_are_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let legacy = "a".repeat(64);
    let packed = "b".repeat(64);
    let staged = "c".repeat(64);
    std::fs::write(
        artifact_dir.join(format!("{legacy}.meta")),
        vec![0_u8; 4096],
    )
    .unwrap();
    std::fs::write(
        artifact_dir.join(format!("{packed}.pack")),
        vec![0_u8; 4096],
    )
    .unwrap();
    let source = root.path().join("staged-source");
    std::fs::write(&source, vec![0_u8; 4096]).unwrap();
    persist_staged_artifact_paths(&artifact_dir, &staged, &[source.into()]).unwrap();

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
            now: SystemTime::now(),
            space: FilesystemSpace {
                capacity_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
        },
        retired_top_level: None,
    })
    .unwrap();

    assert_eq!(report.artifacts_removed, 3);
    assert_eq!(report.usage_after_bytes, 0);
}

#[cfg(unix)]
#[test]
fn issue_1148_linked_artifact_root_cannot_escape_product_ownership() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let sibling = root.path().join("another-product");
    let linked = root.path().join("artifacts");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("sentinel"), b"survives").unwrap();
    symlink(&sibling, &linked).unwrap();

    let artifacts = DashMap::new();
    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    let dep_graph = DepGraph::new();
    let error = maintain_disk_artifacts(MaintenancePass {
        artifact_dir: &linked,
        artifacts: &artifacts,
        artifact_store: &store,
        index_writer_tx: None,
        dep_graph: &dep_graph,
        pending_write_bytes: 0,
        policy: bytes_policy(1),
        kind: MaintenanceKind::Pressure,
        environment: &FixedEnvironment {
            now: SystemTime::now(),
            space: FilesystemSpace {
                capacity_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
        },
        retired_top_level: None,
    })
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read(sibling.join("sentinel")).unwrap(),
        b"survives"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn issue_1148_linked_staged_root_cannot_escape_product_ownership() {
    let root = tempfile::tempdir().unwrap();
    let artifact_dir = root.path().join("owned").join("artifacts");
    let sibling = root.path().join("another-product");
    let linked_staged = artifact_dir.join(".staged-v2");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("sentinel"), b"survives nested link").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sibling, &linked_staged).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&sibling, &linked_staged).unwrap();

    let artifacts = DashMap::new();
    let store = ArtifactStore::open_empty(&root.path().join("index.bin"));
    let dep_graph = DepGraph::new();
    let error = maintain_disk_artifacts(MaintenancePass {
        artifact_dir: &artifact_dir,
        artifacts: &artifacts,
        artifact_store: &store,
        index_writer_tx: None,
        dep_graph: &dep_graph,
        pending_write_bytes: 0,
        policy: bytes_policy(1),
        kind: MaintenanceKind::Pressure,
        environment: &FixedEnvironment {
            now: SystemTime::now(),
            space: FilesystemSpace {
                capacity_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
        },
        retired_top_level: None,
    })
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read(sibling.join("sentinel")).unwrap(),
        b"survives nested link"
    );
}

#[test]
fn issue_1148_future_persisted_access_is_clamped_at_restore() {
    let mut meta = ArtifactIndex::new(vec![], vec![], Vec::new(), Vec::new(), 0);
    meta.stored_at_secs = (SystemTime::now() + 365 * DAY)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let restored = CachedArtifact::from_index(meta);
    let access = restored.access_snapshot();
    assert!(access.last_used_wall <= SystemTime::now());
    assert!(!access.used_in_process);
    assert!(access.last_access_checkpoint.is_none());
}

#[tokio::test]
async fn artifact_lookup_lease_orders_access_insert_before_gc_remove() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !full_marker_path(cache_dir.as_path()).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let key = "d".repeat(64);
    let artifact_dir = daemon.state.artifact_dir.clone();
    std::fs::write(artifact_dir.join(format!("{key}.meta")), vec![0_u8; 4096]).unwrap();
    let meta = ArtifactIndex::new(vec![], vec![], Vec::new(), Vec::new(), 0);

    daemon
        .state
        .artifacts
        .insert(key.clone(), CachedArtifact::from_index(meta.clone()));
    assert!(daemon
        .state
        .index_writer_tx
        .send(IndexWriterCommand::Insert(key.clone(), meta))
        .is_ok());
    let lookup = lookup_artifact_with_disk_fallback(&daemon.state, &key)
        .expect("live artifact should acquire a publication lease");
    let maintenance_daemon = Arc::clone(&daemon);
    let mut maintenance = tokio::spawn(async move {
        maintenance_daemon
            .maintain_disk(MaintenanceKind::Pressure)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut maintenance)
            .await
            .is_err(),
        "maintenance must wait while an owned cache lookup is materializing"
    );
    record_artifact_access(&daemon.state, &key, &lookup, Instant::now());
    drop(lookup);

    let report = maintenance.await.unwrap().unwrap();
    assert_eq!(report.artifacts_removed, 1);
    assert!(!daemon.state.artifacts.contains_key(&key));
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let reopened = ArtifactStore::open(index_path.as_path()).unwrap();
    assert!(reopened.get(&key).is_none());
    let _ = daemon.shutdown().await;
}

#[tokio::test]
async fn issue_1148_shutdown_waits_for_running_maintenance() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !full_marker_path(cache_dir.as_path()).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let publication_guard = daemon.state.artifact_publication.read().await;
    let maintenance_daemon = Arc::clone(&daemon);
    let maintenance = tokio::spawn(async move {
        maintenance_daemon
            .maintain_disk(MaintenanceKind::Pressure)
            .await
    });
    tokio::task::yield_now().await;

    let shutdown_daemon = Arc::clone(&daemon);
    let mut shutdown = tokio::spawn(async move { shutdown_daemon.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut shutdown)
            .await
            .is_err(),
        "shutdown must wait for the pass holding/awaiting the publication barrier"
    );
    drop(publication_guard);

    let _ = maintenance.await.unwrap();
    let report = shutdown.await.unwrap();
    assert!(report.pending_writes_drained);
}

// ---------------------------------------------------------------------------
// #1165 Finding 1 — session reaping
// ---------------------------------------------------------------------------

/// Stand-in PID for a crashed client.
///
/// Deliberately **not** fed to the real `is_process_alive`. These tests inject
/// their own predicate, because no PID is portably dead: `is_process_alive` is
/// `kill(pid, 0) == 0` on unix and `OpenProcess` on Windows, and PID 0
/// disagrees between them -- `kill(0, 0)` signals the caller's own process
/// group and *succeeds*, while `OpenProcess(0)` fails. An earlier version of
/// this test used PID 0 against the real predicate and passed on Windows while
/// failing on Linux for exactly that reason. A large "probably free" PID would
/// trade the disagreement for a recycling race.
const DEAD_CLIENT_PID: u32 = 999_999_001;

/// Liveness predicate under test: everything is alive except the crashed
/// client. Deterministic on every platform.
fn only_dead_client_is_gone(pid: u32) -> bool {
    pid != DEAD_CLIENT_PID
}

#[tokio::test]
async fn reaping_removes_a_session_whose_client_died() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );

    let dead = daemon
        .state
        .sessions
        .create(crate::depgraph::SessionConfig {
            client_pid: DEAD_CLIENT_PID,
            working_dir: root.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });
    let live = daemon
        .state
        .sessions
        .create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: root.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });
    assert_eq!(daemon.state.sessions.active_count(), 2);

    // #1324: zero grace so these exercise reclamation itself, not the
    // idle window that protects an in-use session from its exited starter.
    reap_finished_sessions_with_grace(
        &daemon.state,
        only_dead_client_is_gone,
        std::time::Duration::ZERO,
    );

    // The crashed client is the case that leaked without bound: it never sent
    // SessionEnd, so nothing else would ever have removed it.
    assert!(
        daemon.state.sessions.context_count(&dead).is_none(),
        "a session whose client process is gone must be reaped"
    );
    assert!(
        daemon.state.sessions.context_count(&live).is_some(),
        "reaping must not touch a session whose client is still running"
    );
    assert_eq!(daemon.state.sessions.active_count(), 1);
}

#[tokio::test]
async fn reaping_is_a_noop_when_every_client_is_alive() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );
    let live = daemon
        .state
        .sessions
        .create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: root.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });

    // Repeated passes must stay idempotent: this runs every maintenance tick
    // for the life of the daemon, so a reaper that eventually evicted live
    // sessions would surface as builds mysteriously losing their session.
    for _ in 0..3 {
        // #1324: zero grace so these exercise reclamation itself, not the
        // idle window that protects an in-use session from its exited starter.
        reap_finished_sessions_with_grace(
            &daemon.state,
            only_dead_client_is_gone,
            std::time::Duration::ZERO,
        );
    }

    assert!(daemon.state.sessions.context_count(&live).is_some());
    assert_eq!(daemon.state.sessions.active_count(), 1);
}

/// #1165 Finding 1b: `ended_sessions` grew one tombstone per completed
/// session, forever.
#[tokio::test]
async fn ended_session_tombstones_expire_on_their_ttl() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );

    let stale = crate::depgraph::SessionId::new();
    let recent = crate::depgraph::SessionId::new();
    let ttl = Duration::from_secs(60 * 60);
    // Build stamps by ADDING to a base and moving the observation time
    // forward, never by subtracting from `Instant::now()`. Still no sleeping:
    // the boundary is what matters, and a test that waits an hour is not a
    // test.
    //
    // The first version did `now - ttl - 1s` and panicked on CI. Subtracting
    // an hour from a monotonic clock whose epoch is younger than an hour
    // underflows, and a freshly booted runner is exactly that.
    // `SessionManager::cleanup_expired` guards the same trap with
    // `checked_sub` ("timeout exceeds uptime; nothing can be expired") -- the
    // precedent was in the file I was already reading.
    let base = std::time::Instant::now();
    let observed_at = base + ttl + Duration::from_secs(1);
    daemon.state.ended_sessions.insert(stale, base);
    // Age at `observed_at` is 1s, comfortably inside the TTL.
    daemon.state.ended_sessions.insert(recent, base + ttl);

    let removed = reap_ended_session_tombstones_at(&daemon.state, ttl, observed_at);

    assert_eq!(removed, 1);
    assert!(
        !daemon.state.ended_sessions.contains_key(&stale),
        "a tombstone past its TTL must be reclaimed"
    );
    assert!(
        daemon.state.ended_sessions.contains_key(&recent),
        "a tombstone inside its TTL still rejects late work for that session"
    );
}

/// Poll a journal file until it holds at least `expected` lines.
///
/// Local to this file on purpose: the writer behind `CompileJournal::log` is a
/// background `std::thread` (`compile_journal/journal_thread.rs`), so an entry
/// is *not* on disk when `log` returns. Reusing the compile-journal tests'
/// `wait_for_lines` would mean reaching into a sibling test module for a
/// helper this file already has a use for; the polling contract is three lines
/// and duplicating it keeps the two test suites independent.
async fn wait_for_journal_lines(path: &Path, expected: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let lines: Vec<String> = contents.lines().map(str::to_string).collect();
            if lines.len() >= expected {
                return lines;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "journal {} never reached {expected} lines",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll a journal file until one of its lines contains `needle`, then return
/// every line. The marker form of [`wait_for_journal_lines`]: counting lines
/// cannot tell "a new file with the new entry" from "the old file with the new
/// entry appended", which is exactly the distinction #1907 turns on.
async fn wait_for_journal_line_containing(path: &Path, needle: &str) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let lines: Vec<String> = contents.lines().map(str::to_string).collect();
            if lines.iter().any(|line| line.contains(needle)) {
                return lines;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "journal {} never gained a line containing {needle}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// #1907: reaping dropped the `Session` — and therefore its `journal_path` —
/// without a matching `journal.close_session`. `SessionEnd` and
/// `handle_release_worktree_handles` both close; the reaper was the one
/// teardown path that did not, so a client that crashed (never sending
/// `SessionEnd`) left the writer thread holding an open `File` forever.
///
/// Observable without counting file descriptors: with the handle still open the
/// writer keeps appending to the *unlinked* inode, so the next entry never
/// reaches a fresh file at that path. On unix the path simply never comes back;
/// on Windows the name stays readable while the delete is pending on the open
/// handle, so the second entry lands there as line 2. The discriminator is
/// therefore "the file at this path holds the second entry and nothing else" —
/// the close-then-reopen contract `compile_journal`'s
/// `test_close_session_then_reopen` already asserts.
///
/// The daemon is started with `automatic_maintenance == false` so the
/// background disk-maintenance loop -- the only *other* caller of the reaper --
/// cannot reap the session out from under the test mid-assertion.
/// `EmbeddedDaemon::start` passes `true` and would make this racy.
#[tokio::test]
async fn reaping_a_dead_client_releases_its_journal_handle() {
    let root = tempfile::tempdir().unwrap();
    let sessions_dir = root.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let session_path = sessions_dir.join("reaped.jsonl");
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = EmbeddedDaemon::start_with_maintenance(
        crate::ipc::unique_test_endpoint(),
        cache_dir,
        None,
        None,
        bytes_policy(1),
        false,
        None,
    )
    .await
    .unwrap();

    let reaped = daemon
        .state
        .sessions
        .create(crate::depgraph::SessionConfig {
            client_pid: DEAD_CLIENT_PID,
            working_dir: root.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: Some(session_path.clone().into()),
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });

    let context = || crate::daemon::compile_journal::JournalContext {
        compiler: "clang".into(),
        args: vec![],
        cwd: root.path().to_string_lossy().into_owned(),
        env: None,
        session_id: Some("reaped".into()),
    };
    // `latency_ns` doubles as the marker for which entry a line belongs to.
    daemon.state.journal.log(
        &crate::daemon::compile_journal::JournalEntry::new(context(), "hit", 0, 100, None),
        Some(&session_path),
    );
    // Wait for the first line before unlinking: the writer opens the file lazily
    // on its first `Entry`, so deleting a path that does not exist yet would
    // prove nothing about the leaked handle.
    wait_for_journal_lines(&session_path, 1).await;

    // Unlink the file the still-open writer is holding. `open_append` adds
    // `FILE_SHARE_DELETE` precisely so this succeeds on Windows too.
    std::fs::remove_file(&session_path).unwrap();

    // #1324: zero grace so this exercises reclamation itself, not the idle
    // window that protects an in-use session from its exited starter.
    reap_finished_sessions_with_grace(
        &daemon.state,
        only_dead_client_is_gone,
        std::time::Duration::ZERO,
    );
    assert!(
        daemon.state.sessions.context_count(&reaped).is_none(),
        "the reaper must still drop the map entry it reclaimed"
    );

    daemon.state.journal.log(
        &crate::daemon::compile_journal::JournalEntry::new(context(), "hit", 0, 200, None),
        Some(&session_path),
    );

    // Wait for the *second* entry specifically, so a leaked handle fails on
    // both platforms: on unix the marker never appears at this path at all
    // (the poller hits its deadline), and on Windows it appears as line 2 of
    // the still-pending original file, which the length assertion then rejects.
    let lines = wait_for_journal_line_containing(&session_path, "\"latency_ns\":200").await;
    assert_eq!(
        lines.len(),
        1,
        "the journal writer must reopen the path after the reaper closed it, \
         not keep appending to the file it was already holding: {lines:?}"
    );
}

/// #1907: closing a reaped session's journal handle must not close a path a
/// *surviving* session has already reopened. Two sessions may name the same
/// journal file (`--journal` is caller-owned, #1165), so `CloseSession` keyed
/// on path alone would shut the live session's writer out from under it.
///
/// Kept as a pure `#[test]` over the decision helper -- no daemon, no
/// tempfiles -- because the interesting part is the *filter*, and a threaded
/// journal would only test the same filter more slowly.
#[test]
fn journal_paths_to_release_skips_a_path_a_live_session_still_claims() {
    // A neutral fs-inert fixture root. This test never touches the
    // filesystem, so a real temp root would only buy a POSIX-only literal
    // that `ban_tmp_literal` rightly rejects -- its own lint text asks
    // fs-inert fixtures to use `/fixture/...`, and the repo's PathBuf ban
    // keeps the values as `NormalizedPath` end to end.
    let root = crate::core::NormalizedPath::new("/fixture/zccache-journal-paths");
    let p_shared = root.join("shared.jsonl");
    let p_only_reaped = root.join("reaped.jsonl");

    let mut still_claimed = std::collections::HashSet::new();
    still_claimed.insert(p_shared.clone());
    // By value, not by reference: `&[&T; N]` iterates as `&&T`, which is not
    // the `&NormalizedPath` the helper takes. `[&T; N]` is `Copy`, so both
    // assertions below can share one binding.
    let claimed = [&p_shared, &p_only_reaped, &p_shared];
    assert_eq!(
        super::journal_paths_to_release(claimed, &still_claimed),
        vec![p_only_reaped.clone()],
        "a path a live session still claims must not be closed, and a path \
         named twice must be released once"
    );

    // Nothing claims the shared path any more, so both distinct paths come
    // back -- once each. Sorted before comparing: the helper is free to
    // return them in either order, and pinning an order here would
    // constrain the implementation for no behavioural gain.
    let mut all = super::journal_paths_to_release(claimed, &std::collections::HashSet::new());
    all.sort();
    let mut expected = vec![p_shared, p_only_reaped];
    expected.sort();
    assert_eq!(all, expected, "the duplicate must collapse to one entry");
}

#[tokio::test]
async fn tombstone_reaping_is_a_noop_when_nothing_is_stale() {
    let root = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = root.path().join("cache").into();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );
    let fresh = crate::depgraph::SessionId::new();
    daemon
        .state
        .ended_sessions
        .insert(fresh, std::time::Instant::now());

    // Runs every maintenance tick for the daemon's whole life, so repeated
    // passes must not eventually evict a tombstone that is still in date.
    for _ in 0..3 {
        assert_eq!(
            reap_ended_session_tombstones(&daemon.state, Duration::from_secs(60 * 60)),
            0
        );
    }
    assert!(daemon.state.ended_sessions.contains_key(&fresh));
}

/// Count `sessions_reaped` rows in one explicitly selected lifecycle log.
fn sessions_reaped_rows(log_path: &std::path::Path) -> usize {
    let Ok(log) = std::fs::read_to_string(log_path) else {
        return 0;
    };
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == crate::core::lifecycle::EVENT_SESSIONS_REAPED)
        .count()
}

/// #1165 Finding 1: reaping was visible only as a `tracing::info!`, which is
/// gone the moment the daemon's stderr goes somewhere unbounded. The durable
/// event is what makes "this daemon reclaims a session per compile"
/// attributable after the fact.
///
/// Both directions are asserted in one test, in order: a quiet pass must stay
/// silent (this runs every maintenance tick for the daemon's life, so an
/// unconditional emit would itself become the unbounded surface the issue is
/// about), and a pass that reclaims something must leave a record.
#[tokio::test]
async fn reaping_writes_a_durable_event_only_when_it_reclaims_something() {
    let root = tempfile::tempdir().unwrap();
    let global_root = root.path().join("process-global");
    let _cache_env = crate::daemon::server::tests::CacheDirEnvGuard::set(&global_root);
    let cache_dir: crate::core::NormalizedPath = root.path().join("daemon-owned").into();
    let daemon_log = cache_dir
        .join("logs")
        .join(crate::core::lifecycle::LIVE_LOG_FILENAME);
    let global_log = crate::core::lifecycle::log_file_path();
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            bytes_policy(1),
        )
        .await
        .unwrap(),
    );

    let quiet_before = sessions_reaped_rows(daemon_log.as_path());
    // #1324: zero grace so these exercise reclamation itself, not the
    // idle window that protects an in-use session from its exited starter.
    reap_finished_sessions_with_grace(
        &daemon.state,
        only_dead_client_is_gone,
        std::time::Duration::ZERO,
    );
    assert_eq!(
        sessions_reaped_rows(daemon_log.as_path()),
        quiet_before,
        "a pass that reclaims nothing must not write an event"
    );

    daemon
        .state
        .sessions
        .create(crate::depgraph::SessionConfig {
            client_pid: DEAD_CLIENT_PID,
            working_dir: root.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });

    // #1324: zero grace so these exercise reclamation itself, not the
    // idle window that protects an in-use session from its exited starter.
    reap_finished_sessions_with_grace(
        &daemon.state,
        only_dead_client_is_gone,
        std::time::Duration::ZERO,
    );

    assert_eq!(
        sessions_reaped_rows(daemon_log.as_path()),
        quiet_before + 1,
        "reclaiming a dead client's session must leave exactly one record"
    );
    assert_eq!(
        sessions_reaped_rows(global_log.as_path()),
        0,
        "a state-owned reap event must not follow process-global cache state"
    );
    let log = std::fs::read_to_string(daemon_log.as_path()).unwrap();
    let event: serde_json::Value = log
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .rfind(|event: &serde_json::Value| {
            event["event"] == crate::core::lifecycle::EVENT_SESSIONS_REAPED
        })
        .expect("a sessions_reaped event");
    assert_eq!(event["dead_client"], 1);
    assert_eq!(
        event["remaining"], 0,
        "the event must carry what is left, not just what went"
    );
}

/// #1165 Finding 6: the depfile sweep ran at startup only. That bounds growth
/// *across* daemon lifetimes but not *within* one, and the startup call site
/// is on the standalone path — an embedded host that never restarts its
/// process never swept at all. Both modes share this maintenance loop, so
/// running it here is what closes the gap.
#[tokio::test]
async fn the_periodic_sweep_reclaims_a_dead_instances_depfile_dir() {
    let root = tempfile::tempdir().unwrap();
    let _cache_env = crate::daemon::server::tests::CacheDirEnvGuard::set(root.path());

    let depfiles = crate::core::config::depfile_dir();
    let dead = depfiles.join(format!("{DEAD_CLIENT_PID}-0"));
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("some.d"), b"a.o: a.c").unwrap();
    let live = depfiles.join(format!("{}-0", std::process::id()));
    std::fs::create_dir_all(&live).unwrap();

    sweep_stale_depfile_dirs(None).await;

    assert!(
        !dead.exists(),
        "a depfile dir owned by a dead daemon instance must be reclaimed"
    );
    assert!(
        live.exists(),
        "reclaiming must not touch a running instance's depfile dir — that breaks its build"
    );
    let log = std::fs::read_to_string(crate::core::lifecycle::log_file_path())
        .expect("the sweep must leave a durable record");
    assert!(
        log.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|event| event["event"] == crate::core::lifecycle::EVENT_STALE_DEPFILE_DIRS_SWEPT),
        "the sweep should record what it reclaimed"
    );
}
