//! Native jobserver primitives.

#[must_use]
pub fn is_supported() -> bool { crate::platform_imp::process::jobserver::is_supported() }
