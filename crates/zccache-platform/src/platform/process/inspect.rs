//! Process liveness, image-path, and CPU-tick inspection.

use crate::platform_imp;

#[must_use]
pub fn is_alive(pid: u32) -> bool { platform_imp::process::inspect::is_alive(pid) }

pub fn executable_path(pid: u32) -> Option<std::path::PathBuf> {
    platform_imp::process::inspect::executable_path(pid)
}

pub fn cpu_ticks(pid: u32) -> Option<u64> { platform_imp::process::inspect::cpu_ticks(pid) }
