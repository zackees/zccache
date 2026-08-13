use std::io;
use super::{Endpoint, Stream};
pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    tokio::net::UnixStream::connect(endpoint.as_str()).await.map(Stream)
}
