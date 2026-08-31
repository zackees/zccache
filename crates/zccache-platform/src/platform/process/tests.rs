use std::time::Duration;

use super::{exit, inspect, jobserver, priority::Priority, spawn, terminate};

#[test]
fn neutral_priorities_are_distinct_and_ordered() {
    assert!(Priority::High < Priority::Normal);
    assert!(Priority::Normal < Priority::Low);
    assert!(Priority::Low < Priority::Idle);
}

#[test]
fn current_process_is_live_and_has_an_executable() {
    let pid = std::process::id();
    assert!(inspect::is_alive(pid));
    let executable = inspect::executable_path(pid).expect("current executable path");
    assert!(executable.is_absolute());
}

#[test]
fn owned_child_can_be_observed_and_terminated() {
    let mut child = spawn::sleeping_child(Duration::from_secs(30)).expect("spawn child");
    let pid = child.id();
    assert!(inspect::is_alive(pid));
    terminate::force(pid).expect("terminate owned child");
    child.wait().expect("reap child");
    assert!(!inspect::is_alive(pid));
}

#[test]
fn disposable_child_output_and_exit_status_are_observable() {
    let output = spawn::echo_output("zccache-platform-child").expect("spawn echo child");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("zccache-platform-child"));
}

#[cfg(unix)]
#[test]
fn signal_termination_is_preserved_in_the_reserved_negative_namespace() {
    let status = std::process::Command::new("sh")
        .args(["-c", "kill -TERM $$"])
        .status()
        .expect("spawn signal-terminated child");

    let outcome = exit::outcome(&status);

    assert_eq!(outcome.exit_code, -143);
    assert_eq!(outcome.termination_signal, Some(15));
    assert_eq!(exit::termination_signal_from_exit_code(-1), None);
    assert_eq!(exit::termination_signal_from_exit_code(-128), None);
    assert_eq!(exit::termination_signal_from_exit_code(i32::MIN), None);
}

#[cfg(windows)]
#[test]
fn negative_windows_status_remains_native_and_signal_less() {
    use std::os::windows::process::ExitStatusExt as _;

    let status = std::process::ExitStatus::from_raw(0xC000_0005);
    let outcome = exit::outcome(&status);

    assert_eq!(outcome.exit_code, 0xC000_0005_u32 as i32);
    assert_eq!(outcome.termination_signal, None);
    assert_eq!(
        exit::termination_signal_from_exit_code(outcome.exit_code),
        None
    );
}

#[test]
fn child_cpu_ticks_are_nondecreasing() {
    let mut child = spawn::sleeping_child(Duration::from_secs(30)).expect("spawn child");
    let first = inspect::cpu_ticks(child.id()).expect("first CPU reading");
    let second = inspect::cpu_ticks(child.id()).expect("second CPU reading");
    assert!(second >= first);
    child.kill().expect("kill child");
    child.wait().expect("reap child");
}

#[test]
fn native_capabilities_have_stable_labels() {
    assert!(!exit::crash_label(exit::NativeExit::Success).is_empty());
    assert_eq!(jobserver::is_supported(), !crate::host::is_windows());
}

#[test]
fn native_jobserver_matches_reported_capability() {
    let zero = jobserver::NativeJobserver::create(0).unwrap_err();
    assert_eq!(zero.kind(), std::io::ErrorKind::InvalidInput);

    match jobserver::NativeJobserver::create(2) {
        Ok(pool) => {
            assert!(jobserver::is_supported());
            let auth = pool.auth_string();
            let fields: Vec<&str> = auth.split(',').collect();
            assert_eq!(fields.len(), 2);
            assert!(fields.iter().all(|field| field.parse::<i32>().is_ok()));
        }
        Err(error) => {
            assert!(!jobserver::is_supported());
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
    }
}

#[test]
fn cli_entry_preserves_its_exit_code() {
    fn success() -> std::process::ExitCode {
        std::process::ExitCode::SUCCESS
    }

    assert_eq!(
        spawn::run_cli_entry(success),
        std::process::ExitCode::SUCCESS
    );
}
