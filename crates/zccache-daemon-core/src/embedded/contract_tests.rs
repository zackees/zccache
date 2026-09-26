//! Issue #905 regression tests for the completed embedded host contract.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::time::Duration;

use kernal_api::async_engine::CancellationSource;
use tempfile::TempDir;
use tokio::sync::{oneshot, Notify, Semaphore};

use super::*;

/// zccache#1578: real staged cache hits waiting on a store lock must not park
/// the only Tokio worker supplied by an embedding host.
#[cfg(target_os = "linux")]
#[test]
fn concurrent_real_cache_hits_leave_embedded_control_plane_responsive() {
    let Some(compiler) = crate::test_support::find_rustc() else {
        return;
    };
    let host_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("single-worker host runtime");
    let temp = TempDir::new().expect("fixture directory");
    let mut settings = config(&temp, "real-hit-control-plane", None);
    settings.runtime.handle = Some(host_rt.handle().clone());
    // #1686: the service stores under the versioned effective root, so the
    // lock must be taken there. Locking `<cache_root>/artifacts` gated nothing,
    // and the test only passed when the hits were still running at the check.
    let artifact_dir = crate::core::config::artifacts_dir_from_cache_dir(
        &crate::core::config::effective_cache_root_from_top_level(&settings.cache_root),
    );
    let service = host_rt
        .block_on(ZccacheService::start(settings))
        .expect("embedded service starts");

    let mut requests = Vec::new();
    let mut outputs = Vec::new();
    for index in 0..4 {
        let crate_name = format!("source_{index}");
        let source = temp.path().join(format!("{crate_name}.rs"));
        let out_dir = temp.path().join(format!("target-{index}"));
        std::fs::create_dir_all(&out_dir).expect("create output directory");
        let rlib = out_dir.join(format!("lib{crate_name}.rlib"));
        let rmeta = out_dir.join(format!("lib{crate_name}.rmeta"));
        std::fs::write(&source, format!("pub fn value() -> u32 {{ {index} }}\n"))
            .expect("write source");
        let request = CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("real-hit-run").expect("run id"),
                crate::audit::AuditId::new(format!("real-hit-{index}")).expect("trace id"),
            ),
            compiler: compiler.clone(),
            args: vec![
                "--crate-name".into(),
                crate_name,
                "--crate-type=rlib".into(),
                "--emit=metadata,link".into(),
                "--out-dir".into(),
                out_dir.to_string_lossy().into_owned(),
                source.to_string_lossy().into_owned(),
            ],
            cwd: temp.path().into(),
            env: Vec::new(),
            stdin: Vec::new(),
        };
        let cold = host_rt
            .block_on(service.compile(request.clone()))
            .expect("cold compile");
        assert_eq!(cold.exit_code, 0, "cold compiler: {cold:?}");
        assert!(!cold.cached, "distinct source starts cold");
        assert!(rlib.exists() && rmeta.exists(), "cold rustc outputs");
        requests.push(request);
        outputs.extend([rlib, rmeta]);
    }

    host_rt
        .block_on(service.shutdown(ShutdownMode::Graceful))
        .expect("persist cold artifacts before replay");
    for output in &outputs {
        std::fs::remove_file(output).expect("remove cold output");
    }
    let mut replay_settings = config(&temp, "real-hit-control-plane", None);
    replay_settings.runtime.handle = Some(host_rt.handle().clone());
    let service = host_rt
        .block_on(ZccacheService::start(replay_settings))
        .expect("restart embedded service for staged replay");

    let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let lock_holder = std::thread::spawn(move || {
        let root = zccache_artifact::staged_lock::staged_root(&artifact_dir);
        let file =
            zccache_artifact::staged_lock::open_store_lock(&root).expect("open staged-store lock");
        let lock =
            kernal_api::platform::fs::lock_exclusive_owned(file).expect("hold staged-store lock");
        locked_tx.send(()).expect("announce held lock");
        // Keep the lock until the test explicitly releases it. A disconnected
        // sender also releases it if an assertion unwinds; a wall-clock
        // timeout can instead let delayed CI work turn real hits into misses.
        let _ = release_rx.recv();
        drop(lock);
    });
    locked_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("exclusive store lock acquired");

    let service = Arc::new(service);
    let handles = host_rt.block_on(async {
        requests
            .into_iter()
            .map(|request| {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.compile(request).await })
            })
            .collect::<Vec<_>>()
    });
    let started = std::time::Instant::now();
    let responsive = host_rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.daemon.test_active_cache_requests() < 2 {
                tokio::task::yield_now().await;
            }
            tokio::spawn(async { tokio::task::yield_now().await })
                .await
                .expect("independent host task");
        })
        .await
    });
    assert!(
        responsive.is_ok() && started.elapsed() < Duration::from_secs(1),
        "concurrent real cache hits must leave the host worker responsive"
    );
    if handles.iter().all(tokio::task::JoinHandle::is_finished) {
        // #1686: say which way the contract broke instead of only that every
        // compile finished under the exclusive lock.
        let outcomes = handles
            .into_iter()
            .map(|handle| match host_rt.block_on(handle) {
                Ok(Ok(response)) => format!(
                    "exit={} cached={} outcome={:?}",
                    response.exit_code, response.cached, response.cache_outcome
                ),
                Ok(Err(error)) => format!("error: {error}"),
                Err(error) => format!("join error: {error}"),
            })
            .collect::<Vec<_>>();
        panic!("exclusive lock must still hold a real warm hit; outcomes: {outcomes:?}");
    }
    release_tx.send(()).expect("release staged-store lock");
    lock_holder.join().expect("release store lock");
    for handle in handles {
        let response = host_rt
            .block_on(handle)
            .expect("warm task")
            .expect("warm compile");
        assert_eq!(response.exit_code, 0, "warm compiler: {response:?}");
        assert!(response.cached, "warm compile must be a real cache hit");
    }
    assert!(outputs.iter().all(|output| output.exists()));
    let service = Arc::try_unwrap(service).unwrap_or_else(|_| panic!("service still shared"));
    host_rt
        .block_on(service.shutdown(ShutdownMode::Graceful))
        .expect("embedded shutdown");
}

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

// A readiness wait, not a perf budget: it covers service start plus the
// real clang's probing spawns, which on the windows-11-arm64 20260914 runner
// image routinely exceed 15 s. Assertions about ordering stay unchanged.
const COMPILE_GATE_TIMEOUT: Duration = Duration::from_secs(60);

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
    let cancellation = CancellationSource::new();
    let mut service_config = config(&temp, "external-cancellation", Some(1));
    service_config.cancellation = Some(cancellation.token());
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

#[tokio::test]
async fn forced_shutdown_precedes_a_ready_compile_result() {
    let temp = TempDir::new().expect("temp cache root");
    let service = ZccacheService::start(config(&temp, "forced-ready-order", None))
        .await
        .expect("service start");
    service.force_cancellation.cancel();

    let outcome = service.await_compile(async { Ok::<_, String>(()) }).await;
    assert!(matches!(outcome, Err(EmbeddedError::Cancelled)));

    service
        .shutdown(ShutdownMode::Force)
        .await
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

/// zccache#1588: a compiler whose descendant does the heavy work (the
/// `rustc` -> `cc` -> `ld` shape) journals a tree peak covering it.
#[cfg(unix)]
#[tokio::test]
async fn embedded_compile_journals_a_tree_peak_covering_a_heavy_descendant() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = TempDir::new().expect("temp cache root");
    let compiler = temp.path().join("linking-compiler");
    std::fs::write(&compiler, "#!/bin/sh\nsh -c 'x=$(head -c 64000000 /dev/zero | tr \"\\000\" a); sleep 3; true' & wait\nexit 3\n")
        .expect("write compiler");
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
        .expect("make compiler executable");
    let service = ZccacheService::start(config(&temp, "embedded-tree-peak", None))
        .await
        .expect("service start");
    let response = service
        .compile(CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("tree-peak-run").expect("non-empty"),
                crate::audit::AuditId::new("tree-peak-trace").expect("non-empty"),
            ),
            compiler: compiler.into(),
            args: Vec::new(),
            cwd: temp.path().into(),
            env: Vec::new(),
            stdin: Vec::new(),
        })
        .await
        .expect("compiler returns a compile response");
    assert_eq!(response.exit_code, 3);
    // zccache#1588: the host receives the same measurement on the response, so
    // an embedding host can learn a unit's peak without reading the journal.
    let host_tree = response
        .child_memory
        .tree_peak_rss_bytes
        .expect("tree peak on the compile response");
    assert!(
        host_tree >= 48 * 1024 * 1024,
        "response tree peak {host_tree} missed the heavy descendant"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let row = loop {
        let row = contract_find_journal(temp.path())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|c| c.lines().next().map(str::to_owned));
        match row {
            Some(row) => break row,
            None if std::time::Instant::now() > deadline => {
                panic!("embedded compile produced no compile_journal.jsonl record")
            }
            None => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    };
    let v: serde_json::Value = serde_json::from_str(&row).expect("valid JSON journal line");
    let tree = v["tree_peak_rss_bytes"]
        .as_u64()
        .expect("tree peak journaled");
    let own = v["child_peak_rss_bytes"]
        .as_u64()
        .expect("child peak journaled");
    assert!(
        tree >= 48 * 1024 * 1024,
        "tree peak {tree} missed the heavy descendant: {v}"
    );
    assert!(
        tree > own,
        "tree peak must exceed the compiler's own peak: {v}"
    );
    assert_eq!(v["tree_peak_rss_source"], "sampled");

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown service");
}

// Only the Unix-gated tree-peak test above calls this; ungated it is dead code
// on Windows, where CI builds tests with `-D warnings`.
#[cfg(unix)]
fn contract_find_journal(dir: &std::path::Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = contract_find_journal(&path) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some("compile_journal.jsonl") {
            return Some(path);
        }
    }
    None
}

/// soldr#3152: a compiler that actually runs must land its measured peak RSS
/// on the embedded journal row — the calibration input for memory-aware
/// admission.
#[cfg(unix)]
#[tokio::test]
async fn embedded_compile_journals_child_peak_rss() {
    use std::os::unix::fs::PermissionsExt as _;

    fn find_journal(dir: &std::path::Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_journal(&path) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some("compile_journal.jsonl") {
                return Some(path);
            }
        }
        None
    }

    let temp = TempDir::new().expect("temp cache root");
    let compiler = temp.path().join("slow-compiler");
    std::fs::write(&compiler, "#!/bin/sh\nsleep 1\nexit 3\n").expect("write compiler");
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
        .expect("make compiler executable");
    let service = ZccacheService::start(config(&temp, "embedded-peak-rss", None))
        .await
        .expect("service start");
    let response = service
        .compile(CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("peak-rss-run").expect("non-empty"),
                crate::audit::AuditId::new("peak-rss-trace").expect("non-empty"),
            ),
            compiler: compiler.into(),
            args: Vec::new(),
            cwd: temp.path().into(),
            env: Vec::new(),
            stdin: Vec::new(),
        })
        .await
        .expect("compiler returns a compile response");
    assert_eq!(response.exit_code, 3);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let row = loop {
        let row = find_journal(temp.path())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|c| c.lines().next().map(str::to_owned));
        match row {
            Some(row) => break row,
            None if std::time::Instant::now() > deadline => {
                panic!("embedded compile produced no compile_journal.jsonl record")
            }
            None => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    };
    let v: serde_json::Value = serde_json::from_str(&row).expect("valid JSON journal line");
    assert!(
        v["child_peak_rss_bytes"].as_u64().is_some_and(|b| b > 0),
        "a compiler that ran must journal its peak RSS: {v}"
    );

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown service");
}
