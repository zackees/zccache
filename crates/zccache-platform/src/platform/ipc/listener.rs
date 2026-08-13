use crate::platform_imp;

use super::{Endpoint, PeerIdentity, Stream};

/// Bound local IPC listener.
pub struct Listener(platform_imp::ipc::Listener);

impl Listener {
    /// Bind securely to `endpoint`.
    pub fn bind(endpoint: &Endpoint) -> std::io::Result<Self> {
        platform_imp::ipc::Listener::bind(&endpoint.0).map(Self)
    }

    /// Bind from an asynchronous caller.
    pub async fn bind_async(endpoint: &Endpoint) -> std::io::Result<Self> {
        let endpoint = endpoint.clone();
        tokio::task::spawn_blocking(move || Self::bind(&endpoint))
            .await
            .map_err(|error| std::io::Error::other(format!("IPC bind worker failed: {error}")))?
    }

    /// Accept one connection and report primitive peer facts.
    pub async fn accept(&mut self) -> std::io::Result<(Stream, PeerIdentity)> {
        let (stream, peer) = self.0.accept().await?;
        Ok((Stream(stream), PeerIdentity(peer)))
    }

    /// Whether bind repaired a pre-existing permissive endpoint directory.
    pub fn tightened_parent(&self) -> bool {
        self.0.tightened_parent()
    }

    /// Empty any pre-created accept pool, returning its former size.
    #[doc(hidden)]
    pub fn drain_accept_pool(&mut self) -> usize {
        self.0.drain_accept_pool()
    }
}
