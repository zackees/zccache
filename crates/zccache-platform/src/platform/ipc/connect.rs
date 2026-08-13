use crate::platform_imp;

use super::{Endpoint, Stream};

/// Connect to a local IPC endpoint.
pub async fn connect(endpoint: &Endpoint) -> std::io::Result<Stream> {
    platform_imp::ipc::connect(&endpoint.0).await.map(Stream)
}
