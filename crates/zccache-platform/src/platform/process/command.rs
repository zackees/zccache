//! Prepared-command native configuration.

/// Configure a background command so it never creates a visible console.
pub fn hide_window(command: &mut std::process::Command) {
    crate::platform_imp::process::command::hide_window(command);
}
