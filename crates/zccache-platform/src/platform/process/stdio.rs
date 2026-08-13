//! Native standard-I/O detachment and early log redirection.

pub fn detach() {
    crate::platform_imp::process::stdio::detach();
}

#[must_use]
pub fn redirect_to_log(path: &std::path::Path) -> bool {
    crate::platform_imp::process::stdio::redirect_to_log(path)
}
