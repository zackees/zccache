//! Native process termination.

pub fn force(pid: u32) -> std::io::Result<()> {
    crate::platform_imp::process::terminate::force(pid)
}

/// Best-effort termination of the process group rooted at `pid`.
pub fn force_group(pid: u32) {
    crate::platform_imp::process::terminate::force_group(pid);
}
