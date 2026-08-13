use std::pin::Pin;
use std::task::{Context, Poll};

use crate::platform_imp;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Statically dispatched bidirectional local IPC stream.
pub struct Stream(pub(crate) platform_imp::ipc::Stream);

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // SAFETY: projection does not move the concrete stream.
        unsafe { self.map_unchecked_mut(|stream| &mut stream.0) }.poll_read(context, buffer)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // SAFETY: projection does not move the concrete stream.
        unsafe { self.map_unchecked_mut(|stream| &mut stream.0) }.poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // SAFETY: projection does not move the concrete stream.
        unsafe { self.map_unchecked_mut(|stream| &mut stream.0) }.poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // SAFETY: projection does not move the concrete stream.
        unsafe { self.map_unchecked_mut(|stream| &mut stream.0) }.poll_shutdown(context)
    }
}
