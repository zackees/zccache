//! Issue #905 regression tests for the completed embedded host contract.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::{oneshot, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

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

/// Holds compile tasks after `compile_inner` has acquired its shared permit
/// but before the daemon engine starts the compiler. The affected tests use a
/// multi-thread runtime, so this synchronous event callback never blocks a
/// current-thread Tokio runtime.
struct CompileStartGate {
    started: AtomicUsize,
    started_notify: Notify,
    state: Mutex<CompileStartGateState>,
    release_notify: Condvar,
}

#[derive(Default)]
struct CompileStartGateState {
    armed: bool,
    released: bool,
}

/// Releases a test gate even if an assertion unwinds before its normal
/// release point, so a compiler task cannot strand a Tokio worker.
#[must_use = "keep the guard alive until the gated compile has been released"]
struct CompileStartGateRelease {
    gate: Arc<CompileStartGate>,
}

impl Drop for CompileStartGateRelease {
    fn drop(&mut self) {
        self.gate.release();
    }
}

impl CompileStartGate {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            started_notify: Notify::new(),
            state: Mutex::new(CompileStartGateState::default()),
            release_notify: Condvar::new(),
        }
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::Acquire)
    }

    async fn wait_for_starts(&self, expected: usize) {
        complete_within_gate_timeout(
            async {
                loop {
                    let notified = self.started_notify.notified();
                    if self.started() >= expected {
                        return;
                    }
                    notified.await;
                }
            },
            "COMPILE_STARTED event",
        )
        .await;
    }

    fn arm(self: &Arc<Self>) -> CompileStartGateRelease {
        {
            let mut state = self.state.lock().expect("compile gate lock");
            state.armed = true;
            state.released = false;
        }
        CompileStartGateRelease {
            gate: Arc::clone(self),
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("compile gate lock");
        if !state.released {
            state.released = true;
            self.release_notify.notify_all();
        }
    }
}

const COMPILE_GATE_TIMEOUT: Duration = Duration::from_secs(15);

async fn complete_within_gate_timeout<F>(future: F, operation: &'static str) -> F::Output
where
    F: Future,
{
    tokio::time::timeout(COMPILE_GATE_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {operation}"))
}

impl EmbeddedEventSink for CompileStartGate {
    fn emit(&self, event: &AuditEvent) {
        if event.event.0 != crate::audit::AuditEventName::COMPILE_STARTED {
            return;
        }
        self.started.fetch_add(1, Ordering::Release);
        self.started_notify.notify_waiters();
        let mut state = self.state.lock().expect("compile gate lock");
        if !state.armed {
            return;
        }
        while !state.released {
            state = self
                .release_notify
                .wait(state)
                .expect("compile gate lock remains valid");
        }
    }
}

fn c_compile_request(
    temp: &TempDir,
    compiler: crate::core::NormalizedPath,
    name: &str,
) -> (CompileRequest, PathBuf) {
    let source = temp.path().join(format!("{name}.c"));
    let output = temp.path().join(format!("{name}.o"));
    std::fs::write(&source, format!("int {name}(void) {{ return 1; }}\n"))
        .expect("C source fixture");
    (
        CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new(format!("{name}-run")).expect("run id"),
                crate::audit::AuditId::new(format!("{name}-trace")).expect("trace id"),
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
        },
        output,
    )
}

async fn assert_pending<F>(future: &mut std::pin::Pin<Box<F>>, message: &'static str)
where
    F: Future,
{
    std::future::poll_fn(|context| {
        assert!(
            matches!(future.as_mut().poll(context), Poll::Pending),
            "{message}"
        );
        Poll::Ready(())
    })
    .await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_work_blocks_compile_admission_at_capacity_one() {
    let Some(compiler) = crate::test_support::find_clang() else {
        return;
    };
    let temp = TempDir::new().expect("temp cache root");
    let gate = Arc::new(CompileStartGate::new());
    let service = ZccacheService::start_with_event_sink(
        config(&temp, "compile-limit", Some(1)),
        gate.clone(),
    )
    .await
    .expect("service start");
    let external = service
        .acquire_external_work_permit()
        .await
        .expect("external work admitted");
    let (request, output) = c_compile_request(&temp, compiler, "external_before_compile");
    let mut compile = Box::pin(service.compile(request));
    assert_pending(
        &mut compile,
        "a real compile must wait while external work holds the only permit",
    )
    .await;
    assert!(
        gate.started() == 0 && !output.exists(),
        "a queued real compile must not emit COMPILE_STARTED or create output"
    );

    let _gate_release = gate.arm();
    let gate_for_release = Arc::clone(&gate);
    let output_before_release = output.clone();
    let release = tokio::spawn(async move {
        gate_for_release.wait_for_starts(1).await;
        assert!(
            !output_before_release.exists(),
            "the compiler must remain held at COMPILE_STARTED"
        );
        gate_for_release.release();
    });
    drop(external);
    let response =
        complete_within_gate_timeout(compile, "compile after external work releases capacity")
            .await
            .expect("real compile succeeds after external work releases capacity");
    assert_eq!(response.exit_code, 0);
    complete_within_gate_timeout(release, "compile gate release")
        .await
        .expect("compile gate released");
    assert!(output.exists(), "real compile creates its requested output");

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compile_admission_blocks_external_work_at_capacity_one() {
    let Some(compiler) = crate::test_support::find_clang() else {
        return;
    };
    let temp = TempDir::new().expect("temp cache root");
    let gate = Arc::new(CompileStartGate::new());
    let service = ZccacheService::start_with_event_sink(
        config(&temp, "external-limit", Some(1)),
        gate.clone(),
    )
    .await
    .expect("service start");
    let _gate_release = gate.arm();
    let (request, output) = c_compile_request(&temp, compiler, "compile_before_external");
    let compile_service = service.clone();
    let compile = tokio::spawn(async move { compile_service.compile(request).await });
    gate.wait_for_starts(1).await;
    assert!(
        !output.exists(),
        "the real compiler must be held after its admission event"
    );

    let mut external = Box::pin(service.acquire_external_work_permit());
    assert_pending(
        &mut external,
        "external work must wait while the real compile holds the only permit",
    )
    .await;
    assert!(
        gate.started() == 1,
        "the queued external task must not let another compile admission through"
    );

    gate.release();
    let response = complete_within_gate_timeout(compile, "held real compile")
        .await
        .expect("compile task joined")
        .expect("real compile succeeds");
    assert_eq!(response.exit_code, 0);
    assert!(output.exists(), "real compile creates its requested output");
    let external = complete_within_gate_timeout(external, "external work after compile completion")
        .await
        .expect("external work admitted after compile completion");
    drop(external);
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compiler_and_external_work_share_the_combined_limit() {
    let Some(compiler) = crate::test_support::find_clang() else {
        return;
    };
    let temp = TempDir::new().expect("temp cache root");
    let gate = Arc::new(CompileStartGate::new());
    let service = ZccacheService::start_with_event_sink(
        config(&temp, "combined-limit", Some(3)),
        gate.clone(),
    )
    .await
    .expect("service start");
    let _gate_release = gate.arm();
    let (first_request, first_output) = c_compile_request(&temp, compiler.clone(), "combined_one");
    let (second_request, second_output) =
        c_compile_request(&temp, compiler.clone(), "combined_two");
    let (third_request, third_output) = c_compile_request(&temp, compiler, "combined_three");
    let first_service = service.clone();
    let first = tokio::spawn(async move { first_service.compile(first_request).await });
    let second_service = service.clone();
    let second = tokio::spawn(async move { second_service.compile(second_request).await });
    gate.wait_for_starts(2).await;
    let external = service
        .acquire_external_work_permit()
        .await
        .expect("external work admitted");
    let mut third = Box::pin(service.compile(third_request));
    assert_pending(
        &mut third,
        "two real compiles plus external work must exhaust the combined limit",
    )
    .await;
    assert!(
        gate.started() == 2
            && !first_output.exists()
            && !second_output.exists()
            && !third_output.exists(),
        "the third real compile must remain outside the combined capacity"
    );

    gate.release();
    for compile in [first, second] {
        let response = complete_within_gate_timeout(compile, "held real compile")
            .await
            .expect("compile task joined")
            .expect("held real compile succeeds");
        assert_eq!(response.exit_code, 0);
    }
    let response = complete_within_gate_timeout(third, "third real compile")
        .await
        .expect("third real compile succeeds after combined capacity releases");
    assert_eq!(response.exit_code, 0);
    assert!(
        first_output.exists() && second_output.exists() && third_output.exists(),
        "all real compiles create their requested outputs"
    );
    drop(external);
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn dropped_waiter_and_guard_release_external_work_capacity() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "drop-release", Some(1)))
        .await
        .expect("service start");
    let held = service
        .acquire_external_work_permit()
        .await
        .expect("first external work admitted");
    let waiting_service = service.clone();
    let (waiting_tx, waiting_rx) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        waiting_tx.send(()).expect("test observes waiter");
        waiting_service.acquire_external_work_permit().await
    });
    waiting_rx.await.expect("waiter started");
    tokio::task::yield_now().await;
    waiter.abort();
    assert!(waiter.await.is_err(), "aborted waiter must finish");

    drop(held);
    let released = service
        .acquire_external_work_permit()
        .await
        .expect("dropped waiter must not consume capacity");
    drop(released);
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn host_cancellation_wakes_external_work_waiters() {
    let temp = TempDir::new().expect("temp cache root");
    let cancellation = CancellationToken::new();
    let mut service_config = config(&temp, "external-cancellation", Some(1));
    service_config.cancellation = Some(cancellation.clone());
    let service = ZccacheService::start(service_config)
        .await
        .expect("service start");
    let held = service
        .acquire_external_work_permit()
        .await
        .expect("first external work admitted");
    let queued_service = service.clone();
    let (waiting_tx, waiting_rx) = oneshot::channel();
    let (outcome_tx, outcome_rx) = oneshot::channel();
    let queued = tokio::spawn(async move {
        waiting_tx
            .send(())
            .expect("test observes cancellation waiter");
        let _ = outcome_tx.send(queued_service.acquire_external_work_permit().await);
    });
    waiting_rx.await.expect("cancellation waiter started");
    tokio::task::yield_now().await;
    cancellation.cancel();
    let outcome = outcome_rx.await.expect("cancelled waiter reports outcome");
    assert!(matches!(outcome, Err(EmbeddedError::Cancelled)));
    queued.await.expect("cancelled waiter task joined");
    drop(held);
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown after host cancellation");
}

#[tokio::test]
async fn graceful_shutdown_wakes_external_work_waiters() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "external-shutdown", Some(1)))
        .await
        .expect("service start");
    let held = service
        .acquire_external_work_permit()
        .await
        .expect("first external work admitted");
    let queued_service = service.clone();
    let (waiting_tx, waiting_rx) = oneshot::channel();
    let (outcome_tx, outcome_rx) = oneshot::channel();
    let queued = tokio::spawn(async move {
        waiting_tx.send(()).expect("test observes shutdown waiter");
        let _ = outcome_tx.send(queued_service.acquire_external_work_permit().await);
    });
    waiting_rx.await.expect("shutdown waiter started");
    tokio::task::yield_now().await;
    let shutdown_service = service.clone();
    let shutdown =
        tokio::spawn(async move { shutdown_service.shutdown(ShutdownMode::Graceful).await });
    let outcome = outcome_rx.await.expect("shutdown waiter reports outcome");
    assert!(matches!(outcome, Err(EmbeddedError::ShutDown)));
    queued.await.expect("shutdown waiter task joined");
    drop(held);
    shutdown
        .await
        .expect("shutdown task joined")
        .expect("graceful shutdown");
}

#[tokio::test]
async fn unlimited_external_work_returns_usable_guards() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "unlimited-external", None))
        .await
        .expect("service start");
    let first = service
        .acquire_external_work_permit()
        .await
        .expect("first unlimited guard");
    let second = service
        .acquire_external_work_permit()
        .await
        .expect("second unlimited guard");
    drop((first, second));
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
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
