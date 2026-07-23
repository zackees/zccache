//! Embedded disk-maintenance API regression tests (issue #1148).

use crate::embedded::{
    AuditConfig, DiskCacheLimits, DiskMaintenanceKind, DiskMaintenancePressure, HostIdentity,
    RuntimeHooks, ServiceLimits, ShutdownMode, ZccacheConfig, ZccacheService,
};

fn test_config(cache_root: crate::core::NormalizedPath, product: &str) -> ZccacheConfig {
    let mut audit = AuditConfig::default();
    audit.mode = crate::audit::AuditMode::Off;
    ZccacheConfig {
        host: HostIdentity {
            product: product.into(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            workspace_id: format!("{product}-workspace"),
        },
        cache_root,
        audit,
        limits: ServiceLimits::default(),
        runtime: RuntimeHooks::default(),
        cancellation: None,
    }
}

#[tokio::test]
async fn embedded_api_maintains_only_its_configured_root() {
    let temp = tempfile::TempDir::new().expect("temporary root");
    let service = ZccacheService::start_with_disk_limits(
        test_config(temp.path().join("owned").into(), "maintenance-test"),
        DiskCacheLimits {
            max_cache_bytes: Some(1),
            ..DiskCacheLimits::default()
        },
    )
    .await
    .expect("service start");
    let cache_root = service.stats().await.expect("stats").cache_root;

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !cache_root.join(".disk-maintenance-last-full-v1").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup maintenance marker");

    let artifact_dir = cache_root.join("artifacts");
    std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
    std::fs::write(artifact_dir.join("key.meta"), vec![0_u8; 1024]).expect("metadata fixture");
    std::fs::write(artifact_dir.join("key_0"), vec![0_u8; 4096]).expect("payload fixture");
    let sibling = temp.path().join("another-product");
    std::fs::create_dir_all(&sibling).expect("sibling root");
    std::fs::write(sibling.join("sentinel"), b"must survive").expect("sibling sentinel");

    let concurrent = service.clone();
    let (first, second) = tokio::join!(
        service.maintain_disk(DiskMaintenanceKind::Pressure),
        concurrent.maintain_disk(DiskMaintenanceKind::Pressure)
    );
    let reports = [first.expect("first pass"), second.expect("second pass")];
    assert!(reports
        .iter()
        .all(|report| report.kind == DiskMaintenanceKind::Pressure));
    assert!(reports
        .iter()
        .any(|report| report.pressure == DiskMaintenancePressure::Hard));
    assert_eq!(
        reports
            .iter()
            .map(|report| report.artifacts_removed)
            .sum::<usize>(),
        1
    );
    assert!(!artifact_dir.join("key.meta").exists());
    assert!(!artifact_dir.join("key_0").exists());
    assert_eq!(
        std::fs::read(sibling.join("sentinel")).expect("sibling remains"),
        b"must survive"
    );

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("service shutdown");
}

#[tokio::test]
async fn embedded_startup_reclaims_seeded_cache_before_any_compile_traffic() {
    let temp = tempfile::TempDir::new().expect("temporary root");
    let top_level: crate::core::NormalizedPath = temp.path().join("product-root").into();
    let effective = crate::core::config::effective_cache_root_from_top_level(&top_level);
    let artifact_dir = effective.join("artifacts");
    std::fs::create_dir_all(&artifact_dir).expect("artifact root");
    let key = "a".repeat(64);
    std::fs::write(artifact_dir.join(format!("{key}.meta")), vec![0_u8; 4096])
        .expect("seed metadata");
    std::fs::write(artifact_dir.join(format!("{key}_0")), vec![0_u8; 4096]).expect("seed payload");

    let service = ZccacheService::start_with_disk_limits(
        test_config(top_level, "seeded-maintenance-test"),
        DiskCacheLimits {
            max_cache_bytes: Some(1),
            ..DiskCacheLimits::default()
        },
    )
    .await
    .expect("service start");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !effective.join(".disk-maintenance-last-full-v1").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup catch-up maintenance");

    assert!(!artifact_dir.join(format!("{key}.meta")).exists());
    assert!(!artifact_dir.join(format!("{key}_0")).exists());
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("service shutdown");
}

#[tokio::test]
async fn standalone_daemon_startup_expires_seeded_stale_cache() {
    let temp = tempfile::TempDir::new().expect("temporary root");
    let cache_dir: crate::core::NormalizedPath = temp.path().join("standalone-root").into();
    let artifact_dir = crate::core::config::artifacts_dir_from_cache_dir(&cache_dir);
    std::fs::create_dir_all(&artifact_dir).expect("artifact root");
    let key = "e".repeat(64);
    let meta_path = artifact_dir.join(format!("{key}.meta"));
    let payload_path = artifact_dir.join(format!("{key}_0"));
    std::fs::write(&meta_path, b"invalid legacy metadata").expect("seed metadata");
    std::fs::write(&payload_path, vec![0_u8; 4096]).expect("seed payload");
    let stale = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    );
    filetime::set_file_mtime(&meta_path, stale).expect("age metadata");
    filetime::set_file_mtime(&payload_path, stale).expect("age payload");

    let endpoint = crate::ipc::unique_test_endpoint();
    let mut server = super::super::DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir)
        .expect("standalone bind");
    let shutdown = server.shutdown_handle();
    let task = tokio::spawn(async move { server.run(0).await });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !cache_dir.join(".disk-maintenance-last-full-v1").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("standalone startup maintenance");

    assert!(!meta_path.exists());
    assert!(!payload_path.exists());
    shutdown.notify_one();
    task.await
        .expect("standalone task join")
        .expect("standalone shutdown");
}
