//! Unit tests for daemon-owned bounded retention (issue #1148).

use super::*;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn artifact(key: &str, bytes: u64, now: SystemTime, age: Duration) -> DiskArtifact {
    DiskArtifact {
        key: key.to_string(),
        allocated_bytes: bytes,
        last_access: now - age,
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
    let publication_barrier = Arc::new(tokio::sync::RwLock::new(()));
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
                },
                Some(&publication_barrier),
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
    let old_time = filetime::FileTime::from_system_time(old);
    filetime::set_file_mtime(&meta_path, old_time).unwrap();
    filetime::set_file_mtime(&payload_path, old_time).unwrap();

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
    let allocated = allocated_bytes(&meta_path, &std::fs::metadata(&meta_path).unwrap());
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
    let entries = vec![artifact("fresh", 10, now, Duration::from_secs(1))];
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

#[test]
fn issue_1148_windows_allocated_size_combines_high_low_and_falls_back() {
    assert_eq!(
        windows_allocated_size_result(7, 1, 0, 99),
        (1_u64 << 32) | 7
    );
    assert_eq!(windows_allocated_size_result(u32::MAX, 0, 5, 99), 99);
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
    daemon
        .state
        .index_writer_tx
        .send(IndexWriterCommand::Insert(key.clone(), meta))
        .unwrap();
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
