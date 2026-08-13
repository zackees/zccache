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

pub fn current_user_name() -> Option<String> { super::host::current_user() }
pub fn select_host_text(file_value: String, _windows_value: String) -> String { file_value }

pub fn probe_native(endpoint: &str) -> std::io::Result<()> {
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::{GenericFilePath, Stream, ToFsName};
    let name = ToFsName::to_fs_name::<GenericFilePath>(endpoint)?;
    drop(Stream::connect(name)?);
    Ok(())
}
