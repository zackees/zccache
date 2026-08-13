use std::time::Duration;

use super::{exit, inspect, jobserver, priority::Priority, spawn, terminate};

#[test]
fn neutral_priorities_are_distinct_and_ordered() {
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
