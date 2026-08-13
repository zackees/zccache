use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};

pub enum Stream { Server(NamedPipeServer), Client(NamedPipeClient) }
impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        // SAFETY: projecting a pinned enum to its active field does not move that field.
        unsafe { match self.get_unchecked_mut() { Self::Server(stream) => Pin::new_unchecked(stream).poll_read(cx, buffer), Self::Client(stream) => Pin::new_unchecked(stream).poll_read(cx, buffer) } }
    }
}
impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<io::Result<usize>> {
        // SAFETY: projecting a pinned enum to its active field does not move that field.
        unsafe { match self.get_unchecked_mut() { Self::Server(stream) => Pin::new_unchecked(stream).poll_write(cx, buffer), Self::Client(stream) => Pin::new_unchecked(stream).poll_write(cx, buffer) } }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // SAFETY: projecting a pinned enum to its active field does not move that field.
        unsafe { match self.get_unchecked_mut() { Self::Server(stream) => Pin::new_unchecked(stream).poll_flush(cx), Self::Client(stream) => Pin::new_unchecked(stream).poll_flush(cx) } }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // SAFETY: projecting a pinned enum to its active field does not move that field.
        unsafe { match self.get_unchecked_mut() { Self::Server(stream) => Pin::new_unchecked(stream).poll_shutdown(cx), Self::Client(stream) => Pin::new_unchecked(stream).poll_shutdown(cx) } }
    }
}
