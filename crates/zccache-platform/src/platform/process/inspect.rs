//! Process liveness, image-path, CPU-tick, and peak-memory inspection.

use crate::platform_imp;

#[must_use]
pub fn is_alive(pid: u32) -> bool {
    platform_imp::process::inspect::is_alive(pid)
}

pub fn executable_path(pid: u32) -> Option<std::path::PathBuf> {
    platform_imp::process::inspect::executable_path(pid)
}

pub fn cpu_ticks(pid: u32) -> Option<u64> {
    platform_imp::process::inspect::cpu_ticks(pid)
}

/// Resident-memory high-water mark of `pid`, in bytes (soldr#3152).
///
/// Linux reads `VmHWM`, macOS the lifetime maximum physical footprint, and
/// Windows `PeakWorkingSetSize`. `None` when the process is gone or its
/// memory accounting has already been torn down (a Unix zombie).
pub fn peak_rss_bytes(pid: u32) -> Option<u64> {
    platform_imp::process::inspect::peak_rss_bytes(pid)
}
