//! Public embedded host-admission policy contract tests (zccache#1539).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use tempfile::TempDir;

struct CountingHostPolicy {
    calls: Arc<AtomicUsize>,
}

impl HostAdmissionClassifier for CountingHostPolicy {
    fn requires_exclusive(
        &self,
        _request: &HostCompilerRequest<'_>,
    ) -> std::result::Result<bool, HostAdmissionError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }
}

async fn start_service_with_policy(temp: &TempDir, calls: Arc<AtomicUsize>) -> ZccacheService {
    let mut audit = AuditConfig::default();
    audit.mode = crate::audit::AuditMode::Off;
    ZccacheService::start_with_options_and_host_admission_classifier(
        ZccacheConfig {
            host: HostIdentity {
                product: "host-admission-policy-test".into(),
                instance_id: uuid::Uuid::new_v4().to_string(),
                workspace_id: "host-admission-policy-workspace".into(),
            },
            cache_root: temp.path().join("cache").into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        },
        ZccacheStartOptions::default(),
        Arc::new(CountingHostPolicy { calls }),
    )
    .await
    .expect("service start")
}

#[tokio::test]
async fn host_policy_runs_for_a_miss_but_not_its_cache_hit() {
    let Some(compiler) = crate::test_support::find_clang() else {
        return;
    };
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("unit.c");
    let output = temp.path().join("unit.o");
    std::fs::write(&source, "int admission_policy_unit(void) { return 1; }\n")
        .expect("source fixture");
    let calls = Arc::new(AtomicUsize::new(0));
    let service = start_service_with_policy(&temp, Arc::clone(&calls)).await;
    let request = CompileRequest {
        audit: AuditContext::new(
            crate::audit::AuditId::new("host-policy-run").expect("id"),
            crate::audit::AuditId::new("host-policy-trace").expect("id"),
        ),
        compiler,
        args: vec![
            "-c".into(),
            source.to_string_lossy().into_owned(),
            "-o".into(),
            output.to_string_lossy().into_owned(),
        ],
        cwd: temp.path().into(),
        env: Vec::new(),
        stdin: Vec::new(),
    };

    let miss = service.compile(request.clone()).await.expect("cache miss");
    assert!(!miss.cached, "first compile must execute the compiler");
    assert_eq!(calls.load(Ordering::Relaxed), 1, "miss invokes policy once");

    std::fs::remove_file(&output).expect("remove cold output");
    let hit = service.compile(request).await.expect("cache hit");
    assert!(hit.cached, "second compile must replay the cached output");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "cache hit must bypass the host policy and compiler admission"
    );
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}
