//! Source-level scaffolding for the embedded-service MVP from zccache#903.
//!
//! The service-shape test below stays behind `cfg(any())` until the MVP API is
//! fully wired into durable audit emission and limit enforcement.

#[test]
#[ignore = "documentation guard only; embedded public API is not exported yet"]
fn embedded_service_docs_record_mvp_boundary() {
    let docs = include_str!("../../../docs/architecture/embedded-service.md");

    assert!(docs.contains("## MVP Status"));
    assert!(docs.contains("Public Rust API | MVP landed"));
    assert!(docs.contains("soldr embedded integration | Open"));
    assert!(docs.contains("fbuild embedded integration | Open"));
    assert!(docs.contains("ZccacheService::start(config)"));
    assert!(docs.contains("shutdown(ShutdownMode::Graceful)"));
}

#[cfg(any())]
mod expected_public_api_shape {
    use tempfile::TempDir;
    use zccache::embedded::{
        AuditConfig, HostIdentity, RuntimeHooks, ServiceLimits, ShutdownMode, ZccacheConfig,
        ZccacheService,
    };

    #[tokio::test]
    async fn start_stats_flush_and_shutdown_without_compiler() {
        let temp = TempDir::new().expect("temporary embedded cache root");
        let cache_root = temp.path().join("zccache");
        let audit_root = temp.path().join("audit");

        let service = ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".to_owned(),
                instance_id: "embedded-service-test".to_owned(),
                workspace_id: "workspace".to_owned(),
            },
            cache_root,
            audit: AuditConfig {
                output_root: Some(audit_root.to_string_lossy().into_owned()),
                ..AuditConfig::default()
            },
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("embedded zccache service starts");

        let stats = service
            .stats()
            .await
            .expect("stats are available before compiles");
        assert_eq!(stats.total_compilations, 0);

        service
            .flush()
            .await
            .expect("flush before host audit collection");
        let shutdown = service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("graceful shutdown");
        assert!(shutdown.flushed.pending_writes_drained);
    }
}
