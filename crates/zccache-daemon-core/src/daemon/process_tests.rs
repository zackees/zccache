use super::*;

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
    let baseline = current_in_flight_compiles();
    let t1 = InFlightCompileTicket::acquire();
    assert_eq!(t1.in_flight_before(), baseline);
    assert_eq!(current_in_flight_compiles(), baseline + 1);
    let t2 = InFlightCompileTicket::acquire();
    assert_eq!(t2.in_flight_before(), baseline + 1);
    assert_eq!(current_in_flight_compiles(), baseline + 2);
    drop(t2);
    assert_eq!(current_in_flight_compiles(), baseline + 1);
    drop(t1);
    assert_eq!(current_in_flight_compiles(), baseline);
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
fn auto_priority_can_sample_current_load() {
    let decision = CompilePriority::Auto.resolve_for_current_load();
    assert_eq!(decision.requested, CompilePriority::Auto);
    assert!(matches!(
        decision.effective,
        CompilePriority::Normal | CompilePriority::Low
    ));
    if let Some(cpu_usage_percent) = decision.cpu_usage_percent {
        assert!((0.0..=100.0).contains(&cpu_usage_percent));
    }
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
fn link_like_compile_priority_on_interactive_defaults_to_low_without_link_override() {
    // Issue #813 / #810: link.exe is the single worst single-thread
    // hog on Windows MSVC. Interactive hosts demote it to Low so the
    // late-build link step doesn't lock up the UI.
    let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "idle".to_string())];

    assert_eq!(
        CompilePriority::from_client_env_for_link_like_with_daemon_env_ci(
            Some(&env),
            true,
            None,
            None,
            false, // interactive
        ),
        CompilePriority::Low
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
fn platform_priority_mapping_is_explicit() {
    use zccache_platform::process::priority::Priority;

    assert_eq!(CompilePriority::Auto.platform_priority(), Priority::Normal);
    assert_eq!(
        CompilePriority::Normal.platform_priority(),
        Priority::Normal
    );
    assert_eq!(CompilePriority::Low.platform_priority(), Priority::Low);
    assert_eq!(CompilePriority::Idle.platform_priority(), Priority::Idle);
    assert_eq!(CompilePriority::High.platform_priority(), Priority::High);
}

// ── Console-window suppression (Windows only) ───────────────────────
//
// The process boundary owns Windows console policy. Zccache supplies
// commands and applies its post-spawn priority/Job Object policy only.
// The end-to-end behavior (child having no console window) is hard to
// capture can make the test binary console-less. Soldr's integration test
// probes the real detached daemon; this unit check guards the shared API's
// default.

/// The shared Tokio spawn policy must remain consoleless by default.
#[cfg(windows)]
#[test]
fn running_process_tokio_policy_is_consoleless() {
    assert!(!running_process::TokioSpawnOptions::default().show_console);
}
