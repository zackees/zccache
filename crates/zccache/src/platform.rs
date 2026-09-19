//! zccache CI-specific process-tree termination policy.
//!
//! Kernal-api owns the native Unix session/process-group capability, proven
//! through the owner's unreaped `Child`. Windows has no numeric group kill and
//! reports `Unsupported`, so the CI runner keeps `taskkill /T` there as its
//! retained tree-kill fallback.

pub(crate) fn force_process_group(child: &std::process::Child) {
    match kernal_api::platform::process::force_terminate_process_group(child) {
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &child.id().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        // Best-effort: the caller is already on the failure path and reaps
        // the direct child next.
        _ => {}
    }
}
