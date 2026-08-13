use std::io;
use std::time::Duration;
use tokio::net::windows::named_pipe::ClientOptions;
use super::{Endpoint, Stream};

pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    let mut delay = Duration::from_millis(10);
    loop {
            let native = endpoint.as_str().to_owned();
            let opened = tokio::task::spawn_blocking(move || ClientOptions::new().open(native))
                .await
                .map_err(|error| io::Error::other(format!("pipe open worker failed: {error}")))?;
            match opened {
                Ok(client) => return Ok(Stream::Client(client)),
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
                Err(error) => return Err(error),
            }
    }
}
