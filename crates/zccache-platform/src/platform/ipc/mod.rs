//! Neutral local-transport mechanics.

mod connect;
mod endpoint;
mod listener;
mod peer;
mod stream;

pub use connect::connect;
pub use endpoint::Endpoint;
pub use listener::Listener;
pub use peer::PeerIdentity;
pub use stream::Stream;

/// Current-user name as exposed by the selected host.
pub fn current_user_name() -> Option<String> {
    crate::platform_imp::ipc::current_user_name()
}

/// Select arbitrary product text associated with the native endpoint family.
pub fn select_host_text(file_value: String, windows_value: String) -> String {
    crate::platform_imp::ipc::select_host_text(file_value, windows_value)
}

/// Performs one blocking native local-socket connection probe.
pub fn probe_native(endpoint: &str) -> std::io::Result<()> {
    crate::platform_imp::ipc::probe_native(endpoint)
}

#[cfg(test)]
mod tests;
