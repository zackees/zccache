use super::*;

use super::session::{
    forward_compiler_session_event, kernel_process_priority, kernel_session_kill_when_owner_dies,
    session_cpu_time_advanced, session_fault_error, CompilerSessionEvent,
};

struct KillAndWaitGuard(Option<std::process::Child>);

impl KillAndWaitGuard {
    fn child_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("owner helper is still armed")
    }

    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for KillAndWaitGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

#[test]
fn daemon_owned_child_dies_when_helper_owner_is_killed() {
    const HELPER_ENV: &str = "ZCCACHE_OWNER_DEATH_TEST_HELPER";
    const PID_FILE_ENV: &str = "ZCCACHE_OWNER_DEATH_TEST_PID_FILE";

    if std::env::var_os(HELPER_ENV).is_some() {
        let builder = owner_death_test_child_builder(PID_FILE_ENV);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("owner-death helper runtime");
        let result = runtime.block_on(async_builder_output_with_priority_and_post_exit_grace(
            builder,
            CompilePriority::Normal,
            None,
        ));
        panic!("owner-death test child returned before its owner was killed: {result:?}");
    }

    let temp = tempfile::tempdir().expect("owner-death test tempdir");
    let pid_file = temp.path().join("child.pid");
    let test_name = "daemon::process::tests::daemon_owned_child_dies_when_helper_owner_is_killed";
    let owner = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(HELPER_ENV, "1")
        .env(PID_FILE_ENV, &pid_file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn owner-death helper process");
    let mut owner = KillAndWaitGuard(Some(owner));

    // A readiness wait, not a perf budget: the helper re-executes this test
    // binary, which then starts PowerShell to publish the PID. Process startup
    // on the windows-11-arm64 20260914 runner image routinely exceeds 10 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let child_pid = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                break pid;
            }
        }
        if let Ok(Some(status)) = owner.child_mut().try_wait() {
            panic!("owner-death helper exited before publishing its child PID: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "owner-death helper did not publish its child PID"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    assert!(
        crate::platform::process::inspect::is_alive(child_pid),
        "test child must be alive before its owner is killed"
    );

    owner.kill_and_wait();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::platform::process::inspect::is_alive(child_pid)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let survived = crate::platform::process::inspect::is_alive(child_pid);
    if survived {
        crate::platform::process::terminate::force(child_pid);
    }
    assert!(
        !survived,
        "daemon-owned child {child_pid} survived after helper owner death"
    );
}

fn owner_death_test_child_builder(pid_file_env: &str) -> kernal_api::SpawnSpec {
    #[cfg(windows)]
    {
        kernal_api::SpawnSpec::new("powershell").args([
            "-NoProfile",
            "-Command",
            &format!(
                "$PID | Set-Content -LiteralPath $env:{pid_file_env}; Start-Sleep -Seconds 30"
            ),
        ])
    }
    #[cfg(unix)]
    {
        kernal_api::SpawnSpec::new("sh").args([
            "-c",
            &format!("printf '%s\\n' \"$$\" > \"${pid_file_env}\"; exec sleep 30"),
        ])
    }
}

// ── run_cpu_blocking (#955) ──

#[test]
fn run_cpu_blocking_no_runtime_runs_inline() {
    // Outside any tokio runtime the section runs inline and returns
    // the closure's value.
    assert_eq!(run_cpu_blocking(|| 40 + 2), 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_cpu_blocking_multi_thread_ok() {
    // On the daemon's real (multi-thread) runtime this takes the
    // block_in_place branch and must still return the value.
    assert_eq!(run_cpu_blocking(|| "ok"), "ok");
}

#[tokio::test(flavor = "current_thread")]
async fn run_cpu_blocking_current_thread_does_not_panic() {
    // Regression guard: block_in_place panics on a current-thread
    // runtime (the embedded-host path), so run_cpu_blocking MUST fall
    // back to running inline there rather than aborting the compile.
    assert_eq!(run_cpu_blocking(|| 123), 123);
}

#[test]
fn parse_compile_priority_values() {
    assert_eq!(
        CompilePriority::parse("auto").unwrap(),
        CompilePriority::Auto
    );
    assert_eq!(
        CompilePriority::parse("normal").unwrap(),
        CompilePriority::Normal
    );
    assert_eq!(CompilePriority::parse("LOW").unwrap(), CompilePriority::Low);
    assert_eq!(
        CompilePriority::parse(" idle ").unwrap(),
        CompilePriority::Idle
    );
    assert_eq!(
        CompilePriority::parse("high").unwrap(),
        CompilePriority::High
    );
    assert!(CompilePriority::parse("fast").is_err());
}

#[test]
fn formats_compile_priority_for_profiles() {
    assert_eq!(CompilePriority::Auto.as_str(), "auto");
    assert_eq!(CompilePriority::Normal.as_str(), "normal");
    assert_eq!(CompilePriority::Low.as_str(), "low");
    assert_eq!(CompilePriority::Idle.as_str(), "idle");
    assert_eq!(CompilePriority::High.as_str(), "high");
}

#[test]
fn absent_compile_priority_defaults_to_auto() {
    assert_eq!(
        CompilePriority::parse_optional(None).unwrap(),
        CompilePriority::Auto
    );
}

#[test]
fn ci_auto_priority_uses_normal_until_cpu_is_saturated() {
    // CI host (is_ci=true) preserves the historical heuristic:
    // Normal until 95% CPU, then Low. CI runners are dedicated to
    // compilation; no foreground workload to yield to. In-flight
    // count is ignored on CI — the CPU gate is sufficient.
    let is_ci = true;
    assert_eq!(
        CompilePriority::auto_effective_priority(None, is_ci, 0),
        CompilePriority::Normal
    );
    assert_eq!(
        CompilePriority::auto_effective_priority(Some(94.9), is_ci, 32),
        CompilePriority::Normal
    );
    assert_eq!(
        CompilePriority::auto_effective_priority(Some(95.0), is_ci, 0),
        CompilePriority::Low
    );
    assert_eq!(
        CompilePriority::auto_effective_priority(Some(100.0), is_ci, 32),
        CompilePriority::Low
    );
}

#[test]
fn interactive_auto_priority_adapts_to_in_flight_count() {
    // Master-profile 2026-06-25 ISSUE-001: interactive hosts get
    // Normal when no other compile is in flight (single/idle case —
    // bare-rustc speed), Low once a wave is detected. Preserves
    // #813's UI-win on parallel waves while restoring near-bare-rustc
    // speed on the single-compile cases that the unconditional Low
    // was overshooting.
    let is_ci = false;
    // No others in flight → Normal regardless of CPU.
    assert_eq!(
        CompilePriority::auto_effective_priority(None, is_ci, 0),
        CompilePriority::Normal
    );
    assert_eq!(
        CompilePriority::auto_effective_priority(Some(0.0), is_ci, 0),
        CompilePriority::Normal
    );
    assert_eq!(
        CompilePriority::auto_effective_priority(Some(100.0), is_ci, 0),
        CompilePriority::Normal
    );
    // One or more others in flight → Low (yield to UI).
    assert_eq!(
        CompilePriority::auto_effective_priority(None, is_ci, 1),
        CompilePriority::Low
    );
    assert_eq!(
        CompilePriority::auto_effective_priority(Some(50.0), is_ci, 7),
        CompilePriority::Low
    );
}

#[test]
fn auto_priority_decision_records_effective_priority_on_ci() {
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(96.0), true, 0);
    assert_eq!(decision.requested, CompilePriority::Auto);
    assert_eq!(decision.effective, CompilePriority::Low);
    assert_eq!(decision.cpu_usage_percent, Some(96.0));
}

#[test]
fn auto_priority_decision_low_on_interactive_when_wave_in_flight() {
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, 3);
    assert_eq!(decision.requested, CompilePriority::Auto);
    assert_eq!(decision.effective, CompilePriority::Low);
    assert_eq!(decision.cpu_usage_percent, Some(10.0));
}

#[test]
fn auto_priority_decision_normal_on_interactive_when_idle() {
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, 0);
    assert_eq!(decision.requested, CompilePriority::Auto);
    assert_eq!(decision.effective, CompilePriority::Normal);
    assert_eq!(decision.cpu_usage_percent, Some(10.0));
}

#[test]
fn in_flight_ticket_returns_pre_increment_count_atomically() {
    let baseline = IN_FLIGHT_COMPILES.load(Ordering::Acquire);
    let t1 = InFlightCompileTicket::acquire();
    assert_eq!(t1.in_flight_before(), baseline);
    assert_eq!(IN_FLIGHT_COMPILES.load(Ordering::Acquire), baseline + 1);
    let t2 = InFlightCompileTicket::acquire();
    assert_eq!(t2.in_flight_before(), baseline + 1);
    assert_eq!(IN_FLIGHT_COMPILES.load(Ordering::Acquire), baseline + 2);
    drop(t2);
    assert_eq!(IN_FLIGHT_COMPILES.load(Ordering::Acquire), baseline + 1);
    drop(t1);
    assert_eq!(IN_FLIGHT_COMPILES.load(Ordering::Acquire), baseline);
}

/// zccache#924: serialize tests that touch the process-wide host
/// in-flight slot. Without this, parallel test execution sees the
/// "single-slot, last-write-wins" contract collide between cases.
static HOST_INFLIGHT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn host_counter_zero_when_unregistered() {
    let _guard = HOST_INFLIGHT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // No registration: `current_host_in_flight()` returns 0 and
    // auto-priority falls back to today's behavior.
    assert_eq!(current_host_in_flight(), 0);
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, 0);
    assert_eq!(decision.effective, CompilePriority::Normal);
}

#[test]
fn host_counter_summed_into_auto_priority_decision() {
    let _serial = HOST_INFLIGHT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // zccache#924 acceptance criterion: configure a host counter
    // showing 5 in-flight host spawns and assert the read of
    // `current_host_in_flight()` reflects it. Feed that value into
    // `resolve_with_cpu_usage_and_ci(_, is_ci=false, _)` directly so
    // the assertion holds regardless of the test runner — CI
    // detection on GitHub Actions routes Auto through the CI branch
    // that ignores `in_flight_before`, so a test that calls
    // `resolve_for_current_load` would be non-portable.
    let counter = Arc::new(AtomicUsize::new(5));
    let _registration_guard = register_host_in_flight_counter(Arc::clone(&counter));
    assert_eq!(current_host_in_flight(), 5);

    let summed = total_in_flight(0);
    assert_eq!(summed, 5, "host counter must be summed into in-flight");
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, summed);
    assert_eq!(
        decision.effective,
        CompilePriority::Low,
        "Auto must demote to Low when host counter says the box is busy",
    );

    // Bring the host counter back to 0 and confirm the next read
    // sees the change.
    counter.store(0, Ordering::Release);
    assert_eq!(current_host_in_flight(), 0);
    let summed = total_in_flight(0);
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, summed);
    // The injected zccache count is deliberately 0, so unrelated
    // concurrent tests holding real compile tickets cannot affect this
    // host-counter contract.
    assert_eq!(
        decision.effective,
        CompilePriority::Normal,
        "after host counter drops to 0 the interactive Auto decision must be Normal",
    );
}

#[test]
fn host_inflight_guard_clears_slot_on_drop() {
    let _serial = HOST_INFLIGHT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let counter = Arc::new(AtomicUsize::new(7));
    {
        let _guard = register_host_in_flight_counter(Arc::clone(&counter));
        assert_eq!(current_host_in_flight(), 7);
    }
    // RAII guard dropped — slot must be empty again so subsequent
    // tests / future starts see the clean state.
    assert_eq!(
        current_host_in_flight(),
        0,
        "dropping the host-inflight guard must restore the zccache-internal-only baseline"
    );
}

#[test]
fn host_counter_saturates_without_overflow() {
    let _serial = HOST_INFLIGHT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Defensive: a pathological host counter near usize::MAX must
    // not overflow when summed with the ticket's pre-increment
    // count. The implementation uses `saturating_add` for exactly
    // this case — guard the contract here so a refactor cannot
    // regress to wrapping arithmetic.
    //
    // Use explicit `is_ci = false` so the assertion holds on both
    // CI runners and interactive hosts.
    let counter = Arc::new(AtomicUsize::new(usize::MAX));
    let _guard = register_host_in_flight_counter(Arc::clone(&counter));
    let summed = total_in_flight(1);
    assert_eq!(
        summed,
        usize::MAX,
        "saturating_add must clamp at usize::MAX"
    );
    let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, summed);
    assert_eq!(decision.effective, CompilePriority::Low);
}

#[test]
fn client_env_selects_high_mode() {
    let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "high".to_string())];
    assert_eq!(
        CompilePriority::from_client_env(Some(&env)),
        CompilePriority::High
    );
}

#[test]
fn client_env_invalid_value_falls_back_to_low() {
    let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "fast".to_string())];
    assert_eq!(
        CompilePriority::from_client_env(Some(&env)),
        CompilePriority::Low
    );
}

#[test]
fn link_priority_env_overrides_link_like_compile_priority() {
    let env = vec![
        (COMPILE_PRIORITY_ENV.to_string(), "low".to_string()),
        (
            ZCCACHE_COMPILE_PRIORITY_LINK.to_string(),
            "high".to_string(),
        ),
    ];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env(
            Some(&env),
            true,
            None,
            None
        ),
        CompilePriority::High
    );
}

#[test]
fn daemon_link_priority_env_overrides_link_like_compile_priority() {
    let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "low".to_string())];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env(
            Some(&env),
            true,
            Some("high"),
            None
        ),
        CompilePriority::High
    );
}

#[test]
fn link_like_compile_priority_on_ci_defaults_to_normal_without_link_override() {
    let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "idle".to_string())];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env_ci(
            Some(&env),
            true,
            None,
            None,
            true, // is_ci
        ),
        CompilePriority::Normal
    );
}

#[test]
fn link_like_compile_priority_on_interactive_defaults_to_auto_without_link_override() {
    // #1511: Auto keeps a lone link-like compile at Normal and demotes only
    // followers in a parallel wave, retaining #813's UI protection.
    let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "idle".to_string())];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env_ci(
            Some(&env),
            true,
            None,
            None,
            false, // interactive
        ),
        CompilePriority::Auto
    );
}

#[test]
fn is_ci_host_detects_known_env_vars() {
    let make_lookup = |hit: &'static str| {
        move |name: &str| {
            if name == hit {
                Some("true".to_string())
            } else {
                None
            }
        }
    };
    for var in CI_DETECT_ENV_VARS {
        let detected = is_ci_host_with_env(make_lookup(var));
        assert_eq!(
            detected,
            Some(*var),
            "is_ci_host_with_env failed to detect {var}",
        );
    }
}

#[test]
fn is_ci_host_treats_falsy_values_as_interactive() {
    for falsy in ["0", "false", "FALSE", "no", "off", "n", "", "   "] {
        let lookup = |_name: &str| Some(falsy.to_string());
        assert_eq!(
            is_ci_host_with_env(lookup),
            None,
            "value {falsy:?} should NOT be treated as CI",
        );
    }
}

#[test]
fn is_ci_host_returns_none_when_no_env_set() {
    let lookup = |_name: &str| None;
    assert_eq!(is_ci_host_with_env(lookup), None);
}

#[test]
fn non_link_compile_priority_preserves_existing_auto_behavior() {
    let env = vec![
        (
            ZCCACHE_COMPILE_PRIORITY_LINK.to_string(),
            "high".to_string(),
        ),
        (COMPILE_PRIORITY_ENV.to_string(), "auto".to_string()),
    ];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env(
            Some(&env),
            false,
            Some("idle"),
            None
        ),
        CompilePriority::Auto
    );
}

#[test]
fn invalid_link_priority_env_falls_back_to_low() {
    let env = vec![(
        ZCCACHE_COMPILE_PRIORITY_LINK.to_string(),
        "fast".to_string(),
    )];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env(
            Some(&env),
            true,
            None,
            None
        ),
        CompilePriority::Low
    );
}

#[test]
fn kernel_session_owner_death_is_enabled_on_every_host() {
    // Windows needs this just as much as Unix: the canonical builder turns it
    // into its Job Object containment during native spawn.
    assert!(kernel_session_kill_when_owner_dies());
}

#[tokio::test]
async fn session_spawn_errors_preserve_native_io_category() {
    let error = async_builder_output_with_priority(
        kernal_api::SpawnSpec::new("zccache-missing-session-program-fixture"),
        CompilePriority::Normal,
    )
    .await
    .expect_err("a missing program must not spawn");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn session_stream_faults_preserve_native_io_category_and_code() {
    let code = 12345;
    let native = session_fault_error(std::io::ErrorKind::Other, "fixture", Some(code));
    assert_eq!(native.raw_os_error(), Some(code));
    let portable = session_fault_error(std::io::ErrorKind::BrokenPipe, "fixture", None);
    assert_eq!(portable.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(portable.to_string(), "fixture");
}

#[test]
fn kernel_process_priority_preserves_zccache_scheduling_intent() {
    use kernal_api::ProcessPriority;

    assert_eq!(
        kernel_process_priority(CompilePriority::Normal),
        ProcessPriority::Normal
    );
    assert_eq!(
        kernel_process_priority(CompilePriority::Low),
        ProcessPriority::Low
    );
    assert_eq!(
        kernel_process_priority(CompilePriority::Idle),
        ProcessPriority::Idle
    );
    assert_eq!(
        kernel_process_priority(CompilePriority::High),
        ProcessPriority::High
    );
}

#[tokio::test]
async fn semantic_best_effort_priority_does_not_prevent_child_start() {
    #[cfg(unix)]
    let builder = kernal_api::SpawnSpec::new("sh").args(["-c", "exit 0"]);
    #[cfg(windows)]
    let builder = kernal_api::SpawnSpec::new(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
            .join("System32")
            .join("cmd.exe"),
    )
    .args(["/D", "/C", "exit 0"]);

    let session = builder
        // An unprivileged Unix caller ordinarily cannot raise its nice level;
        // that denial used to be logged after spawn, not fail a compile.
        .priority_best_effort(kernal_api::ProcessPriority::High)
        .spawn_session(Default::default())
        .await
        .expect("best-effort priority denial must not prevent child start");
    assert!(session
        .wait()
        .await
        .expect("child remains waitable after best-effort priority")
        .is_success());
}

#[test]
fn semantic_session_start_future_is_send_without_a_caller_held_lock() {
    fn assert_send<T: Send>(_: T) {}

    assert_send(async_builder_output_with_priority(
        kernal_api::SpawnSpec::new("zccache-session-send-fixture"),
        CompilePriority::Normal,
    ));
}

/// A child that writes `stdout` to stdout and `stderr` to stderr (no trailing
/// newlines) and exits 0.
///
/// On Windows `<nul set /p =text` is the newline-free `printf`, but `set /p`
/// reading EOF sets ERRORLEVEL 1, and `cmd /C` exits with the last command's
/// ERRORLEVEL — so the fixture must end in an explicit `exit 0` to model the
/// successful compiler the Unix `sh -c printf` fixture already is.
fn stdout_stderr_success_fixture() -> kernal_api::SpawnSpec {
    #[cfg(unix)]
    let builder = kernal_api::SpawnSpec::new("sh").args(["-c", "printf stdout; printf stderr >&2"]);
    #[cfg(windows)]
    let builder = kernal_api::SpawnSpec::new(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
            .join("System32")
            .join("cmd.exe"),
    )
    .args([
        "/D",
        "/C",
        "<nul set /p =stdout & <nul set /p =stderr 1>&2 & exit 0",
    ]);
    builder
}

#[tokio::test]
async fn semantic_streaming_session_forwards_chunks_without_duplicate_capture() {
    let builder = stdout_stderr_success_fixture();

    let (sender, mut receiver) = kernal_api::async_engine::channel(8);
    let (output, decision) = async_builder_output_streaming_with_priority_decision(
        builder,
        CompilePriority::Normal,
        sender,
        "streaming-fixture".to_owned(),
    )
    .await;
    let output = output.expect("streaming fixture must succeed");
    assert!(output.status.success(), "fixture exit: {:?}", output.status);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(decision.effective, CompilePriority::Normal);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            crate::daemon::compile_output::RawOutputChunk::Stdout(bytes) => stdout.extend(bytes),
            crate::daemon::compile_output::RawOutputChunk::Stderr(bytes) => stderr.extend(bytes),
        }
    }
    assert_eq!(stdout, b"stdout");
    assert_eq!(stderr, b"stderr");
}

#[tokio::test]
async fn semantic_compiler_capture_uses_the_same_session_watchdog_loop() {
    let builder = stdout_stderr_success_fixture();

    let (output, decision) = async_builder_output_with_priority_decision(
        builder,
        CompilePriority::Normal,
        "captured-compiler-fixture".to_owned(),
    )
    .await;
    let output = output.expect("captured compiler fixture must succeed");
    assert!(output.status.success(), "fixture exit: {:?}", output.status);
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
    assert_eq!(decision.effective, CompilePriority::Normal);
}

#[test]
fn semantic_session_stall_monitor_requires_observable_flat_cpu() {
    use std::time::Duration;

    assert!(session_cpu_time_advanced(None, None));
    assert!(session_cpu_time_advanced(
        Some(Duration::from_secs(1)),
        None
    ));
    assert!(session_cpu_time_advanced(
        None,
        Some(Duration::from_secs(1))
    ));
    assert!(session_cpu_time_advanced(
        Some(Duration::from_secs(1)),
        Some(Duration::from_secs(2)),
    ));
    assert!(!session_cpu_time_advanced(
        Some(Duration::from_secs(2)),
        Some(Duration::from_secs(2)),
    ));
    assert_eq!(
        crate::daemon::child_watchdog::stall_tick(),
        Duration::from_secs(5),
        "the canonical session monitor must retain the former sampling cadence",
    );
}

#[tokio::test]
async fn semantic_compiler_consumer_disconnect_remains_a_kill_reap_failure() {
    let (sender, receiver) = kernal_api::async_engine::channel(1);
    drop(receiver);
    let mut stdout_bytes = 0;
    let mut stderr_bytes = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let outcome = forward_compiler_session_event(
        kernal_api::ProcessOutputEvent::Chunk(kernal_api::ProcessOutputChunk::Stdout(
            b"fixture".to_vec(),
        )),
        &Some(sender),
        &mut stdout_bytes,
        &mut stderr_bytes,
        &mut stdout,
        &mut stderr,
    )
    .await;
    let CompilerSessionEvent::ConsumerDisconnected(error) = outcome else {
        panic!("disconnected consumers must be distinguishable from pipe faults");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn semantic_session_admission_denial_prevents_spawn() {
    let admission = kernal_api::SpawnAdmission::new(|| {
        Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "materialization admission denied",
        ))
    });
    let Err(error) = kernal_api::SpawnSpec::new("zccache-admission-denial-fixture")
        .spawn_admission(admission)
        .spawn_session(Default::default())
        .await
    else {
        panic!("denied admission must not spawn");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

// The exclusive materialization guard is held across awaits on purpose: the
// test proves the native spawn cannot proceed while it is held.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_session_admission_holds_materialization_lock_through_native_spawn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[cfg(unix)]
    let builder = kernal_api::SpawnSpec::new("sh").args(["-c", "exit 0"]);
    #[cfg(windows)]
    let builder = kernal_api::SpawnSpec::new(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
            .join("System32")
            .join("cmd.exe"),
    )
    .args(["/D", "/C", "exit 0"]);

    let materialization = crate::daemon::spawn_exclusion::materialize_exclusive();
    let admission_entered = Arc::new(AtomicBool::new(false));
    let entered = admission_entered.clone();
    let admission = kernal_api::SpawnAdmission::new(move || {
        entered.store(true, Ordering::SeqCst);
        Ok::<_, std::io::Error>(crate::daemon::spawn_exclusion::spawn_shared())
    });
    let start = kernal_api::async_engine::launch(async move {
        builder
            .spawn_admission(admission)
            .spawn_session(Default::default())
            .await
            .map(|_session| ())
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !admission_entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor must attempt admission");
    assert!(
        !start.is_finished(),
        "native spawn must wait while materialization owns the exclusive lock"
    );

    drop(materialization);
    start
        .await
        .expect("start task must not be cancelled")
        .expect("session must start after materialization releases its lock");
}
