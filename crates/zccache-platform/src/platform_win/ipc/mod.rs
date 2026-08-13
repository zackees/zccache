mod connect;
mod endpoint;
mod listener;
mod peer;
mod pipe_security;
mod stream;

pub use connect::connect;
pub use endpoint::Endpoint;
pub use listener::Listener;
pub use peer::PeerIdentity;
pub use stream::Stream;

pub fn current_user_name() -> Option<String> { std::env::var("USERNAME").ok() }
pub fn select_host_text(_file_value: String, windows_value: String) -> String { windows_value }
