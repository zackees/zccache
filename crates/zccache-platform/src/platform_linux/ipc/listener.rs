use std::io;
use std::os::unix::fs::PermissionsExt;

use super::{Endpoint, PeerIdentity, Stream};

pub struct Listener { inner: tokio::net::UnixListener, tightened_parent: bool }

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        endpoint.retire()?;
        let mut tightened_parent = false;
        if let Some(parent) = std::path::Path::new(endpoint.as_str()).parent() {
            crate::platform::fs::permissions::create_dir_all_private(parent)?;
            tightened_parent = crate::platform::fs::permissions::ensure_dir_private(parent)
                .map_err(|error| io::Error::new(error.kind(), format!("insecure socket directory: {error}")))?;
        }
        let listener = tokio::net::UnixListener::bind(endpoint.as_str())?;
        std::fs::set_permissions(endpoint.as_str(), std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { inner: listener, tightened_parent })
    }
    pub async fn accept(&mut self) -> io::Result<(Stream, PeerIdentity)> {
        let (stream, _) = self.inner.accept().await?;
        let peer = match stream.peer_cred() {
            Ok(credentials) => PeerIdentity {
                pid: credentials.pid().and_then(|pid| pid.try_into().ok()),
                current_user: credentials.uid() == unsafe { libc::geteuid() },
                credentials_available: true,
            },
            Err(_) => PeerIdentity { pid: None, current_user: false, credentials_available: false },
        };
        Ok((Stream(stream), peer))
    }
    pub fn drain_accept_pool(&mut self) -> usize { 0 }
    pub fn tightened_parent(&self) -> bool { self.tightened_parent }
}
