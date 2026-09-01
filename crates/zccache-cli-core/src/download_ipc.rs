//! Download-daemon IPC adapter.
//!
//! The download daemon intentionally retains its independently versioned
//! bincode protocol. This adapter keeps that codec and its frame layout in
//! `zccache-download-protocol`, while using the shared platform connection as
//! opaque byte transport. It must not use the main daemon prost APIs.

use serde::{de::DeserializeOwned, Serialize};

pub(crate) struct DownloadIpcConnection {
    inner: crate::ipc::IpcConnection,
}

impl DownloadIpcConnection {
    pub(crate) async fn connect(endpoint: &str) -> Result<Self, crate::ipc::IpcError> {
        crate::ipc::connect(endpoint)
            .await
            .map(Self::from_connection)
    }

    pub(crate) fn from_connection(inner: crate::ipc::IpcConnection) -> Self {
        Self { inner }
    }

    pub(crate) async fn send<T: Serialize>(
        &mut self,
        message: &T,
    ) -> Result<(), crate::ipc::IpcError> {
        let frame = crate::download_protocol::encode_message(message).map_err(|error| {
            crate::ipc::IpcError::OpaqueProtocol(format!("download bincode encode failed: {error}"))
        })?;
        self.inner.send_opaque_bytes(&frame).await
    }

    pub(crate) async fn recv<T: DeserializeOwned>(
        &mut self,
    ) -> Result<Option<T>, crate::ipc::IpcError> {
        self.inner
            .recv_opaque_with(crate::download_protocol::decode_message::<T>)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download_protocol::{Request, Response};

    #[tokio::test]
    async fn download_protocol_roundtrips_over_the_opaque_transport() {
        let endpoint = crate::ipc::unique_test_endpoint();
        let mut listener = crate::ipc::IpcListener::bind(&endpoint).expect("bind endpoint");

        let server = tokio::spawn(async move {
            let connection = listener.accept().await.expect("accept client");
            let mut connection = DownloadIpcConnection::from_connection(connection);
            let request: Option<Request> = connection.recv().await.expect("receive request");
            assert_eq!(request, Some(Request::Ping));
            connection
                .send(&Response::Pong)
                .await
                .expect("send response");
        });

        let mut client = DownloadIpcConnection::connect(&endpoint)
            .await
            .expect("connect client");
        client.send(&Request::Ping).await.expect("send request");
        let response: Option<Response> = client.recv().await.expect("receive response");
        assert_eq!(response, Some(Response::Pong));

        server.await.expect("join server");
    }
}
