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
    // The facade owns endpoint naming per host -- filesystem paths here,
    // the kernel namespace on Windows -- so this no longer picks a name
    // type, and no longer names the transport crate to do it.
    let endpoint = kernal_api::IpcEndpoint::new(endpoint)?;
    drop(kernal_api::IpcStream::connect(&endpoint)?);
    Ok(())
}
