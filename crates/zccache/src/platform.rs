//! zccache CI-specific process-tree termination policy.
//!
//! Kernal-api owns the native Unix session/process-group capability. Windows
//! keeps `taskkill /T` here because the CI runner needs a retained tree kill
//! fallback rather than a console-process-group operation.

#[cfg(windows)]
pub(crate) fn force_process_group(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(windows))]
pub(crate) fn force_process_group(pid: u32) {
    let _ = kernal_api::platform::process::force_terminate_process_group(pid);
}
