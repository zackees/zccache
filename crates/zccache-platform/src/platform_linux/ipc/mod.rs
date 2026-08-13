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

pub fn current_user_name() -> Option<String> {
    std::env::var("USER").ok()
}

pub fn select_host_text(file_value: String, _windows_value: String) -> String {
    file_value
}
