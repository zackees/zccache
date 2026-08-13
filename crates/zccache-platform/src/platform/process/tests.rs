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
    assert_eq!(jobserver::is_supported(), !cfg!(windows));
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
