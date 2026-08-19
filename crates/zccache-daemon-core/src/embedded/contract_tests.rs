//! Issue #905 regression tests for the completed embedded host contract.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tokio::sync::Semaphore;

use super::*;

fn config(temp: &TempDir, instance: &str, max_parallel_compiles: Option<usize>) -> ZccacheConfig {
    let mut audit = AuditConfig::default();
    audit.mode = crate::audit::AuditMode::Off;
    ZccacheConfig {
        host: HostIdentity {
            product: "service-contract-test".into(),
            instance_id: instance.into(),
            workspace_id: instance.into(),
        },
        cache_root: temp.path().join("zccache").into(),
        audit,
        limits: ServiceLimits {
            max_parallel_compiles,
            ..ServiceLimits::default()
        },
        runtime: RuntimeHooks::default(),
        cancellation: None,
    }
}

#[tokio::test]
async fn zero_parallel_compiles_is_rejected_before_startup() {
    let temp = TempDir::new().expect("temp cache root");
    let result = ZccacheService::start(config(&temp, "zero-limit", Some(0))).await;
    assert!(
        matches!(result, Err(EmbeddedError::Start(message)) if message.contains("greater than zero")),
        "zero is not a usable compile limit"
    );
}

#[tokio::test]
async fn oversized_parallel_compile_limit_is_rejected_without_panicking() {
    let temp = TempDir::new().expect("temp cache root");
    let result = ZccacheService::start(config(
        &temp,
        "oversized-limit",
        Some(Semaphore::MAX_PERMITS + 1),
    ))
    .await;
    assert!(
        matches!(result, Err(EmbeddedError::Start(message)) if message.contains("must not exceed")),
        "an oversized public limit must return a startup error"
    );
}

#[tokio::test]
async fn max_parallel_compiles_gates_compile_admission() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "compile-limit", Some(1)))
        .await
        .expect("service start");

    let first = service
        .acquire_compile_permit()
        .await
        .expect("first admission")
        .expect("configured limit returns a permit");
    let queued_service = service.clone();
    let mut queued = tokio::spawn(async move { queued_service.acquire_compile_permit().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut queued)
            .await
            .is_err(),
        "a second compile must wait while the only permit is held"
    );

    drop(first);
    let second = tokio::time::timeout(std::time::Duration::from_secs(1), queued)
        .await
        .expect("queued compile admitted after release")
        .expect("queued task joined")
        .expect("second admission")
        .expect("configured limit returns a permit");
    drop(second);

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn forced_shutdown_cancels_a_compile_waiting_for_admission() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "queued-force", Some(1)))
        .await
        .expect("service start");
    let first = service
        .acquire_compile_permit()
        .await
        .expect("first admission")
        .expect("configured limit returns a permit");
    let queued_service = service.clone();
    let queued = tokio::spawn(async move { queued_service.acquire_compile_permit().await });
    tokio::task::yield_now().await;

    let shutdown = tokio::spawn(service.shutdown(ShutdownMode::Force));
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), queued)
        .await
        .expect("forced shutdown must wake queued admission")
        .expect("queued task joined");
    assert!(matches!(outcome, Err(EmbeddedError::Cancelled)));
    drop(first);
    shutdown
        .await
        .expect("shutdown task joined")
        .expect("forced shutdown");
}

#[tokio::test]
async fn forced_shutdown_cancels_an_inflight_compile_future() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "forced-shutdown", None))
        .await
        .expect("service start");
    let compile_service = service.clone();
    let compile = tokio::spawn(async move {
        compile_service
            .await_compile(std::future::pending::<std::result::Result<(), String>>())
            .await
    });
    tokio::task::yield_now().await;

    let shutdown = tokio::spawn(service.shutdown(ShutdownMode::Force));
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), compile)
        .await
        .expect("forced shutdown must wake the compile")
        .expect("compile task joined");
    assert!(matches!(outcome, Err(EmbeddedError::Cancelled)));
    shutdown
        .await
        .expect("shutdown task joined")
        .expect("forced shutdown");
}

fn unspawnable_request(run: &str) -> CompileRequest {
    CompileRequest {
        audit: AuditContext::new(
            crate::audit::AuditId::new(run).expect("non-empty"),
            crate::audit::AuditId::new("audit-trace").expect("non-empty"),
        ),
        compiler: PathBuf::from("/nonexistent/compiler-that-never-runs").into(),
        args: vec!["--version".into()],
        cwd: std::env::current_dir().expect("cwd").into(),
        env: Vec::new(),
        stdin: Vec::new(),
    }
}

#[tokio::test]
async fn host_event_sink_receives_redacted_events_when_file_audit_is_off() {
    let temp = TempDir::new().expect("temp root");
    let events = Arc::new(Mutex::new(Vec::<AuditEvent>::new()));
    let captured = Arc::clone(&events);
    let sink: Arc<dyn EmbeddedEventSink> = Arc::new(move |event: &AuditEvent| {
        captured.lock().expect("event lock").push(event.clone());
    });
    let mut service_config = config(&temp, "host-events", None);
    service_config
        .audit
        .redaction
        .redact_field_keys
        .push("compiler".into());
    let service = ZccacheService::start_with_event_sink(service_config, sink)
        .await
        .expect("service with host sink starts");

    let _ = service.compile(unspawnable_request("host-event-run")).await;
    {
        let events = events.lock().expect("event lock");
        let names: Vec<&str> = events.iter().map(|event| event.event.0.as_str()).collect();
        assert!(names.contains(&"compile.started"), "events: {names:?}");
        assert!(names.contains(&"compile.finished"), "events: {names:?}");
        assert!(
            events
                .iter()
                .all(|event| event.run_id.0 == "host-event-run"),
            "the host's causal context must be preserved"
        );
        let started = events
            .iter()
            .find(|event| event.event.0 == "compile.started")
            .expect("started event");
        assert_eq!(
            started.fields.get("compiler"),
            Some(&serde_json::Value::String("<redacted>".into()))
        );
    }

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}
