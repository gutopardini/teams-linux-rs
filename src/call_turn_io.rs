//! TURN control connections. Stream framing has dedicated read/write owners:
//! cancelling a caller never abandons half a frame on a reusable connection.
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{Mutex, mpsc, oneshot, watch},
};
use tokio_rustls::{TlsConnector, rustls};
const MAX_FRAME: usize = 2048;
const IO_TIMEOUT: Duration = Duration::from_secs(3);
fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "Native relay connection closed",
    )
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid native relay frame")
}

pub(crate) enum Connection {
    Udp(UdpSocket),
    Stream(StreamConnection),
}
struct WriteFrame {
    bytes: Vec<u8>,
    complete: oneshot::Sender<io::Result<usize>>,
}
pub(crate) struct StreamConnection {
    local: SocketAddr,
    remote: SocketAddr,
    outgoing: mpsc::Sender<WriteFrame>,
    incoming: Mutex<mpsc::Receiver<Vec<u8>>>,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
impl Drop for StreamConnection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
impl Connection {
    pub(crate) async fn udp() -> io::Result<Self> {
        Ok(Self::Udp(UdpSocket::bind("0.0.0.0:0").await?))
    }
    pub(crate) async fn tcp(server: SocketAddr) -> io::Result<Self> {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(server)).await??;
        stream.set_nodelay(true)?;
        Ok(Self::stream(stream.local_addr()?, server, stream))
    }
    pub(crate) async fn tls(server: SocketAddr, hostname: &str) -> io::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|root| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                root.subject,
                root.spki,
                root.name_constraints,
            )
        }));
        Self::tls_with_roots(server, hostname, roots).await
    }
    // Extra trust is never exposed to discovery or account configuration.
    // Production always calls tls() with the public root bundle.
    pub(crate) async fn tls_with_roots(
        server: SocketAddr,
        hostname: &str,
        roots: rustls::RootCertStore,
    ) -> io::Result<Self> {
        let name = rustls::ServerName::try_from(hostname).map_err(|_| invalid())?;
        let config = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(server)).await??;
        stream.set_nodelay(true)?;
        let local = stream.local_addr()?;
        let tls = tokio::time::timeout(
            IO_TIMEOUT,
            TlsConnector::from(Arc::new(config)).connect(name, stream),
        )
        .await??;
        Ok(Self::stream(local, server, tls))
    }
    fn stream<S>(local: SocketAddr, remote: SocketAddr, stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing, mut writes) = mpsc::channel::<WriteFrame>(8);
        let (incoming_tx, incoming) = mpsc::channel(8);
        let (stop, mut stopped) = watch::channel(false);
        let (done, done_rx) = watch::channel(false);
        tokio::spawn(async move {
            {
                let (mut reader, mut writer) = tokio::io::split(stream);
                let read = async {
                    loop {
                        // This future is cancelled only when closing the whole
                        // connection, never on transaction timeout/refresh ticks.
                        let packet = read_frame(&mut reader).await?;
                        incoming_tx.send(packet).await.map_err(|_| closed())?;
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), io::Error>(())
                };
                let write = async {
                    while let Some(frame) = writes.recv().await {
                        let size = frame.bytes.len();
                        let result = tokio::time::timeout(IO_TIMEOUT, async {
                            writer.write_all(&frame.bytes).await?;
                            writer.flush().await
                        })
                        .await;
                        match result {
                            Ok(Ok(())) => {
                                let _ = frame.complete.send(Ok(size));
                            }
                            _ => {
                                let _ = frame.complete.send(Err(closed()));
                                return Err(closed());
                            }
                        }
                    }
                    Ok::<(), io::Error>(())
                };
                tokio::select! {
                    _ = stopped.changed() => {},
                    _ = read => {},
                    _ = write => {},
                }
            }
            let _ = done.send(true);
            // Both halves and queues drop together on any failure. Never
            // resume writing another frame after a partial write or timeout.
        });
        Self::Stream(StreamConnection {
            local,
            remote,
            outgoing,
            incoming: Mutex::new(incoming),
            stop,
            done: done_rx,
        })
    }
    pub(crate) fn close_now(&self) {
        if let Self::Stream(s) = self {
            let _ = s.stop.send(true);
        }
    }
    pub(crate) async fn shutdown(&self) {
        if let Self::Stream(s) = self {
            self.close_now();
            let mut done = s.done.clone();
            let _ = tokio::time::timeout(IO_TIMEOUT, async {
                while !*done.borrow_and_update() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
    }
    pub(crate) fn reliable(&self) -> bool {
        matches!(self, Self::Stream(_))
    }
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Udp(s) => s.local_addr(),
            Self::Stream(s) => Ok(s.local),
        }
    }
    pub(crate) fn try_send_to(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_send_to(data, target),
            // Drop must never enqueue a release behind an incomplete stream
            // frame. Explicit release awaits the authenticated exchange.
            Self::Stream(_) => Err(closed()),
        }
    }
    pub(crate) async fn send_to(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.send_to(data, target).await,
            Self::Stream(s) => {
                if target != s.remote || !valid_header(data) || frame_len(data)? != data.len() {
                    return Err(invalid());
                }
                let (complete, wait) = oneshot::channel();
                // Bounded queue; no mutex held across network writes. The
                // owner completes a frame even if this caller is cancelled.
                s.outgoing
                    .send(WriteFrame {
                        bytes: data.to_vec(),
                        complete,
                    })
                    .await
                    .map_err(|_| closed())?;
                wait.await.map_err(|_| closed())?
            }
        }
    }
    pub(crate) async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(s) => s.recv_from(buffer).await,
            Self::Stream(s) => {
                let frame = s.incoming.lock().await.recv().await.ok_or_else(closed)?;
                if frame.len() > buffer.len() {
                    return Err(invalid());
                }
                buffer[..frame.len()].copy_from_slice(&frame);
                Ok((frame.len(), s.remote))
            }
        }
    }
}
fn valid_header(header: &[u8]) -> bool {
    header.len() >= 20 && header[0] & 0xc0 == 0 && header[4..8] == [0x21, 0x12, 0xa4, 0x42]
}
fn frame_len(header: &[u8]) -> io::Result<usize> {
    if !valid_header(header) {
        return Err(invalid());
    }
    let size = 20 + u16::from_be_bytes([header[2], header[3]]) as usize;
    if size > MAX_FRAME || size % 4 != 0 {
        return Err(invalid());
    }
    Ok(size)
}
async fn read_frame<R: AsyncRead + Unpin>(read: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0; 20];
    // Idle is legal; after the first byte, the complete frame has a deadline.
    read.read_exact(&mut header[..1]).await?;
    tokio::time::timeout(IO_TIMEOUT, async {
        read.read_exact(&mut header[1..]).await?;
        let size = frame_len(&header)?;
        let mut frame = vec![0; size];
        frame[..20].copy_from_slice(&header);
        read.read_exact(&mut frame[20..]).await?;
        Ok(frame)
    })
    .await?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::{net::TcpListener, task::JoinHandle};
    pub(crate) fn fixture_roots() -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(&rustls::Certificate(
                include_bytes!("../tests/fixtures/turn/cert.der").to_vec(),
            ))
            .unwrap();
        roots
    }
    pub(crate) fn acceptor() -> tokio_rustls::TlsAcceptor {
        let config = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::Certificate(
                    include_bytes!("../tests/fixtures/turn/cert.der").to_vec(),
                )],
                rustls::PrivateKey(include_bytes!("../tests/fixtures/turn/key.der").to_vec()),
            )
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }
    pub(crate) async fn bridge(udp: SocketAddr, tls: bool) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            if tls {
                if let Ok(stream) = acceptor().accept(tcp).await {
                    forward(stream, udp).await;
                }
            } else {
                forward(tcp, udp).await;
            }
        });
        (addr, task)
    }
    async fn forward<S: AsyncRead + AsyncWrite + Unpin>(stream: S, target: SocketAddr) {
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        udp.connect(target).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        let requests = async {
            while let Ok(frame) = read_frame(&mut reader).await {
                udp.send(&frame).await.unwrap();
            }
        };
        let responses = async {
            let mut buf = [0; 2048];
            loop {
                let n = udp.recv(&mut buf).await.unwrap();
                // Force fragmentation inside both the STUN header and body.
                for chunk in buf[..n].chunks(7) {
                    if writer.write_all(chunk).await.is_err() {
                        return;
                    }
                }
                if writer.flush().await.is_err() {
                    return;
                }
            }
        };
        tokio::select! { _ = requests => {}, _ = responses => {} }
    }
    fn packet(tag: u8) -> Vec<u8> {
        let mut packet = vec![0; 24];
        packet[2..4].copy_from_slice(&4u16.to_be_bytes());
        packet[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        packet[8] = tag;
        packet
    }
    #[tokio::test]
    async fn stream_frames_survive_fragmentation_coalescing_and_cancelled_receive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (send_rest, wait_rest) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            tcp.write_all(&packet(1)[..9]).await.unwrap();
            wait_rest.await.unwrap();
            let mut rest = packet(1)[9..].to_vec();
            rest.extend(packet(2));
            tcp.write_all(&rest).await.unwrap();
            // Verify serialized concurrent writes and no prefix corruption.
            let a = read_frame(&mut tcp).await.unwrap();
            let b = read_frame(&mut tcp).await.unwrap();
            assert_ne!(a[8], b[8]);
        });
        let io = Connection::tcp(addr).await.unwrap();
        let mut buffer = [0; 2048];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), io.recv_from(&mut buffer))
                .await
                .is_err()
        );
        send_rest.send(()).unwrap();
        for tag in [1, 2] {
            let (n, source) = io.recv_from(&mut buffer).await.unwrap();
            assert_eq!(source, addr);
            assert_eq!(&buffer[..n], &packet(tag));
        }
        let a = packet(3);
        let b = packet(4);
        let (a, b) = tokio::join!(io.send_to(&a, addr), io.send_to(&b, addr));
        assert_eq!(a.unwrap(), 24);
        assert_eq!(b.unwrap(), 24);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancelled_send_finishes_frame_before_next_write() {
        let (client, mut server) = tokio::io::duplex(8);
        let addr = "127.0.0.1:5000".parse().unwrap();
        let io = Arc::new(Connection::stream(addr, addr, client));
        let first = io.clone();
        let job = tokio::spawn(async move { first.send_to(&packet(7), addr).await });
        let mut prefix = [0; 4];
        server.read_exact(&mut prefix).await.unwrap();
        job.abort();
        let _ = job.await;
        let second = io.clone();
        let job = tokio::spawn(async move { second.send_to(&packet(8), addr).await });
        let mut rest = vec![0; 20];
        server.read_exact(&mut rest).await.unwrap();
        let mut frame = prefix.to_vec();
        frame.extend(rest);
        assert_eq!(frame, packet(7));
        assert_eq!(read_frame(&mut server).await.unwrap(), packet(8));
        assert_eq!(job.await.unwrap().unwrap(), 24);
        io.shutdown().await;
        assert_eq!(server.read(&mut [0; 1]).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn partial_header_cannot_hold_a_connection_forever() {
        let (client, mut server) = tokio::io::duplex(64);
        let address = "127.0.0.1:5000".parse().unwrap();
        let io = Connection::stream(address, address, client);
        server.write_all(&packet(1)[..9]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), io.recv_from(&mut [0; 2048]))
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(server.read(&mut [0; 1]).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn malformed_or_truncated_stream_closes_instead_of_resynchronizing() {
        for mode in 0..4 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut data = packet(1);
                match mode {
                    0 => data[0] = 0x40, // unnegotiated ChannelData
                    1 => data[2..4].copy_from_slice(&4096u16.to_be_bytes()),
                    2 => data[3] = 3, // not padded
                    _ => data.truncate(21),
                }
                tcp.write_all(&data).await.unwrap();
            });
            let io = Connection::tcp(addr).await.unwrap();
            assert!(io.recv_from(&mut [0; 2048]).await.is_err());
            assert!(io.send_to(&packet(9), addr).await.is_err());
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn dropping_stream_owner_closes_connection_and_tls_rejects_untrusted_or_wrong_name() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let io = Connection::tcp(addr).await.unwrap();
        let (mut tcp, _) = listener.accept().await.unwrap();
        drop(io);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), tcp.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        for wrong_name in [false, true] {
            let (addr, worker) = bridge("127.0.0.1:9".parse().unwrap(), true).await;
            let result = if wrong_name {
                Connection::tls_with_roots(addr, "wrong.fixture.invalid", fixture_roots()).await
            } else {
                Connection::tls(addr, "relay.fixture.invalid").await
            };
            assert!(result.is_err());
            worker.await.unwrap();
        }
    }
}
