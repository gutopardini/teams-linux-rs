//! Datagram boundary for direct UDP and an explicitly owned native relay.
//! Direct UDP stays on a concrete future; only relay implementations box I/O.
use std::{future::Future, io, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::net::UdpSocket;
pub type IoFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

pub trait RelayIo: Send + Sync {
    fn local_addr(&self) -> io::Result<SocketAddr>;
    fn send_to<'a>(&'a self, data: &'a [u8], peer: SocketAddr) -> IoFuture<'a, usize>;
    fn recv_from<'a>(&'a self, buffer: &'a mut [u8]) -> IoFuture<'a, (usize, SocketAddr)>;
    fn close(&self) -> IoFuture<'_, ()>;
}

pub trait DatagramIo: Send + Sync {
    fn local_addr(&self) -> io::Result<SocketAddr>;
    fn send_to<'a>(
        &'a self,
        data: &'a [u8],
        peer: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send + 'a;
    fn recv_from<'a>(
        &'a self,
        buffer: &'a mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send + 'a;
}
impl DatagramIo for UdpSocket {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        UdpSocket::local_addr(self)
    }
    async fn send_to(&self, data: &[u8], peer: SocketAddr) -> io::Result<usize> {
        UdpSocket::send_to(self, data, peer).await
    }
    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        UdpSocket::recv_from(self, buffer).await
    }
}

pub enum Datagram {
    Direct(UdpSocket),
    Relay(Arc<dyn RelayIo>),
}
impl Datagram {
    pub async fn close(&self) -> io::Result<()> {
        match self {
            Self::Direct(_) => Ok(()),
            Self::Relay(io) => io.close().await,
        }
    }
}
impl DatagramIo for Datagram {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(socket) => socket.local_addr(),
            Self::Relay(io) => io.local_addr(),
        }
    }
    async fn send_to(&self, data: &[u8], peer: SocketAddr) -> io::Result<usize> {
        match self {
            Self::Direct(socket) => socket.send_to(data, peer).await,
            Self::Relay(io) => io.send_to(data, peer).await,
        }
    }
    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Direct(socket) => socket.recv_from(buffer).await,
            Self::Relay(io) => io.recv_from(buffer).await,
        }
    }
}
