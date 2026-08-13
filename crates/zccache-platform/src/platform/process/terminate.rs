//! Native process termination.

pub fn force(pid: u32) -> std::io::Result<()> {
    crate::platform_imp::process::terminate::force(pid)
}
