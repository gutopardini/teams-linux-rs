//! Own DTLS1.2/SRTP binding on the retained authenticated ICE route. OpenSSL
//! handles protocol crypto; SDP SHA256 pins authenticate both ephemeral peers.
use super::{
    datagram::{Datagram, DatagramIo},
    ice,
    srtp::SrtpKeyingMaterial,
};
use foreign_types::ForeignTypeRef;
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    ssl::{
        ErrorCode, Ssl, SslContext, SslMethod, SslOptions, SslStream, SslVerifyMode, SslVersion,
    },
    x509::{X509NameBuilder, X509},
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::SocketAddr,
    time::Duration,
};
use zeroize::Zeroizing;
const MTU: usize = 1200;
const MAX_RECORD: usize = 2048;
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);
impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DTLSFingerprint([PINNED])")
    }
}
impl Fingerprint {
    pub fn parse(value: &str) -> Result<Self, Error> {
        let hex = value.strip_prefix("sha-256 ").ok_or(Error::Certificate)?;
        if hex.len() != 95 {
            return Err(Error::Certificate);
        }
        let mut bytes = [0; 32];
        let mut count = 0;
        for (index, part) in hex.split(':').enumerate() {
            if index >= 32 || part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Certificate);
            }
            bytes[index] = u8::from_str_radix(part, 16).map_err(|_| Error::Certificate)?;
            count += 1;
        }
        if count != 32 {
            return Err(Error::Certificate);
        }
        Ok(Self(bytes))
    }
    pub fn sdp(&self) -> String {
        format!(
            "sha-256 {}",
            self.0
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":")
        )
    }
    fn matches(&self, certificate: &X509) -> bool {
        certificate
            .digest(MessageDigest::sha256())
            .is_ok_and(|digest| openssl::memcmp::eq(&self.0, &digest))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    Actpass,
    Active,
    Passive,
}
impl Setup {
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "actpass" => Ok(Self::Actpass),
            "active" => Ok(Self::Active),
            "passive" => Ok(Self::Passive),
            _ => Err(Error::Protocol),
        }
    }
    pub fn sdp(self) -> &'static str {
        match self {
            Self::Actpass => "actpass",
            Self::Active => "active",
            Self::Passive => "passive",
        }
    }
    pub fn local_role(self, remote: Self) -> Result<Role, Error> {
        match (self, remote) {
            (Self::Actpass, Self::Active)
            | (Self::Passive, Self::Active)
            | (Self::Passive, Self::Actpass) => Ok(Role::Server),
            (Self::Actpass, Self::Passive)
            | (Self::Active, Self::Passive)
            | (Self::Active, Self::Actpass) => Ok(Role::Client),
            _ => Err(Error::Protocol),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Protocol,
    Certificate,
    Profile,
    TimedOut,
    Transport,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Protocol => "Native DTLS protocol failed",
            Self::Certificate => "Native DTLS fingerprint rejected",
            Self::Profile => "Native DTLS SRTP profile unsupported",
            Self::TimedOut => "Native DTLS timed out",
            Self::Transport => "Native DTLS transport failed",
        })
    }
}
impl std::error::Error for Error {}
fn protocol(_: impl std::fmt::Debug) -> Error {
    Error::Protocol
}
pub struct Identity {
    certificate: X509,
    key: PKey<Private>,
    fingerprint: Fingerprint,
}
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DTLSIdentity([REDACTED])")
    }
}
impl Identity {
    pub fn generate() -> Result<Self, Error> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).map_err(protocol)?;
        let key =
            PKey::from_ec_key(EcKey::generate(&group).map_err(protocol)?).map_err(protocol)?;
        let mut name = X509NameBuilder::new().map_err(protocol)?;
        name.append_entry_by_text("CN", "teams-native.invalid")
            .map_err(protocol)?;
        let name = name.build();
        let mut serial = BigNum::new().map_err(protocol)?;
        serial
            .rand(128, MsbOption::MAYBE_ZERO, false)
            .map_err(protocol)?;
        let serial = serial.to_asn1_integer().map_err(protocol)?;
        let mut cert = X509::builder().map_err(protocol)?;
        cert.set_version(2).map_err(protocol)?;
        cert.set_serial_number(&serial).map_err(protocol)?;
        cert.set_subject_name(&name).map_err(protocol)?;
        cert.set_issuer_name(&name).map_err(protocol)?;
        cert.set_pubkey(&key).map_err(protocol)?;
        let before = Asn1Time::days_from_now(0).map_err(protocol)?;
        let after = Asn1Time::days_from_now(1).map_err(protocol)?;
        cert.set_not_before(&before).map_err(protocol)?;
        cert.set_not_after(&after).map_err(protocol)?;
        cert.sign(&key, MessageDigest::sha256()).map_err(protocol)?;
        let certificate = cert.build();
        let digest = certificate
            .digest(MessageDigest::sha256())
            .map_err(protocol)?;
        let fingerprint = Fingerprint(digest.as_ref().try_into().map_err(protocol)?);
        Ok(Self {
            certificate,
            key,
            fingerprint,
        })
    }
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }
    fn session(self, remote: Fingerprint, role: Role) -> Result<Session, Error> {
        let mut context = SslContext::builder(SslMethod::dtls()).map_err(protocol)?;
        context
            .set_min_proto_version(Some(SslVersion::DTLS1_2))
            .map_err(protocol)?;
        context
            .set_max_proto_version(Some(SslVersion::DTLS1_2))
            .map_err(protocol)?;
        context
            .set_tlsext_use_srtp("SRTP_AES128_CM_SHA1_80")
            .map_err(protocol)?;
        context
            .set_certificate(&self.certificate)
            .map_err(protocol)?;
        context.set_private_key(&self.key).map_err(protocol)?;
        context.check_private_key().map_err(protocol)?;
        context
            .set_cipher_list("ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256")
            .map_err(protocol)?;
        context.set_options(
            SslOptions::NO_COMPRESSION
                | SslOptions::NO_TICKET
                | SslOptions::NO_RENEGOTIATION
                | SslOptions::NO_QUERY_MTU,
        );
        context.set_verify_depth(4);
        context.set_verify_callback(
            SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT,
            move |_, cert| {
                if cert.error_depth() > 4 {
                    return false;
                }
                if cert.error_depth() != 0 {
                    return true;
                }
                cert.current_cert()
                    .and_then(|c| c.digest(MessageDigest::sha256()).ok())
                    .is_some_and(|digest| openssl::memcmp::eq(&remote.0, &digest))
            },
        );
        // SSL_CTX_set_max_cert_list is a stable macro to SSL_CTX_ctrl(51).
        // context is exclusively owned; size is a fixed positive byte limit.
        unsafe {
            openssl_sys::SSL_CTX_ctrl(context.as_ptr(), 51, 16384, std::ptr::null_mut());
        }
        let mut ssl = Ssl::new(&context.build()).map_err(protocol)?;
        ssl.set_mtu(MTU as u32).map_err(protocol)?;
        match role {
            Role::Client => ssl.set_connect_state(),
            Role::Server => ssl.set_accept_state(),
        }
        let stream = SslStream::new(ssl, Packets::default()).map_err(protocol)?;
        Ok(Session { stream, remote })
    }
    pub async fn handshake(
        self,
        socket: &Datagram,
        peer: SocketAddr,
        agent: &ice::IceAgent,
        remote: Fingerprint,
        role: Role,
    ) -> Result<Outcome, Error> {
        let mut session = self.session(remote, role)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let result=tokio::time::timeout_at(deadline,async {
            let mut buffer=[0;MAX_RECORD];let mut total=0usize;let mut packets=0;
            loop {
                let complete=match session.stream.do_handshake() {
                    Ok(())=>true,
                    Err(error) if matches!(error.code(),ErrorCode::WANT_READ|ErrorCode::WANT_WRITE)=>false,
                    Err(_)=>{session.flush(socket,peer).await?;return Err(Error::Certificate);},
                };
                session.flush(socket,peer).await?;
                if complete {
                    let certificate=session.stream.ssl().peer_certificate().ok_or(Error::Certificate)?;
                    if !session.remote.matches(&certificate){return Err(Error::Certificate);}
                    let profile=session.stream.ssl().selected_srtp_profile().ok_or(Error::Profile)?;
                    if profile.name()!="SRTP_AES128_CM_SHA1_80"{return Err(Error::Profile);}
                    let mut exported=Zeroizing::new([0;60]);
                    session.stream.ssl().export_keying_material(&mut *exported,"EXTRACTOR-dtls_srtp",None).map_err(protocol)?;
                    let material=|key:usize,salt:usize|SrtpKeyingMaterial{master_key:exported[key..key+16].try_into().unwrap(),master_salt:exported[salt..salt+14].try_into().unwrap(),tag:1};
                    let client=material(0,32);let server=material(16,46);
                    let (local,remote)=match role {Role::Client=>(client,server),Role::Server=>(server,client)};
                    return Ok(Outcome{local,remote,session});
                }
                let delay=session.timeout().unwrap_or(Duration::from_millis(100)).max(Duration::from_millis(1));
                tokio::select! {biased;
                    _=tokio::time::sleep(delay)=>{session.handle_timeout()?;session.flush(socket,peer).await?;},
                    received=socket.recv_from(&mut buffer)=>{
                        let (len,source)=received.map_err(|_|Error::Transport)?;
                        if ice::is_stun_message(&buffer[..len]) {
                            if ice::is_stun_request(&buffer[..len]) {agent.handle_stun_request(socket,&buffer[..len],source).await;}
                            continue;
                        }
                        if source!=peer||!is_dtls(&buffer[..len]){continue;}
                        total+=len;packets+=1;if total>65536||packets>256{return Err(Error::Protocol);}
                        session.stream.get_mut().input(&buffer[..len])?;
                    }
                }
            }
        }).await.map_err(|_|Error::TimedOut)?;
        result
    }
}
pub struct Outcome {
    pub local: SrtpKeyingMaterial,
    pub remote: SrtpKeyingMaterial,
    pub session: Session,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    Progress,
    Closed,
}
#[derive(Default)]
struct Packets {
    incoming: VecDeque<Vec<u8>>,
    outgoing: VecDeque<Vec<u8>>,
}
impl Packets {
    fn input(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > MAX_RECORD || self.incoming.len() >= 8 {
            return Err(Error::Protocol);
        }
        self.incoming.push_back(data.to_vec());
        Ok(())
    }
}
impl Read for Packets {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let Some(packet) = self.incoming.pop_front() else {
            return Err(io::ErrorKind::WouldBlock.into());
        };
        if packet.len() > buffer.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }
}
impl Write for Packets {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if data.len() > 1400 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if self.outgoing.len() >= 16 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.outgoing.push_back(data.to_vec());
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub fn is_dtls(packet: &[u8]) -> bool {
    packet.len() >= 13 && (20..=64).contains(&packet[0])
}
/// Kept by the sole media receiver to handle duplicate final flights/alerts.
pub struct Session {
    stream: SslStream<Packets>,
    remote: Fingerprint,
}
impl Session {
    fn timeout(&mut self) -> Option<Duration> {
        let mut time = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        // DTLSv1_get_timeout macro. SSL is exclusively borrowed and timeval is
        // valid for the synchronous call; no pointer is retained by OpenSSL.
        let present = unsafe {
            openssl_sys::SSL_ctrl(
                self.stream.ssl().as_ptr(),
                73,
                0,
                (&mut time as *mut libc::timeval).cast(),
            )
        };
        if present != 1 || time.tv_sec < 0 || !(0..1_000_000).contains(&time.tv_usec) {
            return None;
        }
        Some(Duration::from_secs(time.tv_sec as u64) + Duration::from_micros(time.tv_usec as u64))
    }
    fn handle_timeout(&mut self) -> Result<(), Error> {
        // DTLSv1_handle_timeout macro, exclusive SSL owner, no borrowed buffer.
        let result = unsafe {
            openssl_sys::SSL_ctrl(self.stream.ssl().as_ptr(), 74, 0, std::ptr::null_mut())
        };
        if result < 0 {
            Err(Error::Protocol)
        } else {
            Ok(())
        }
    }
    async fn flush(&mut self, socket: &Datagram, peer: SocketAddr) -> Result<(), Error> {
        while let Some(packet) = self.stream.get_mut().outgoing.pop_front() {
            let n = socket
                .send_to(&packet, peer)
                .await
                .map_err(|_| Error::Transport)?;
            if n != packet.len() {
                return Err(Error::Transport);
            }
        }
        Ok(())
    }
    pub async fn receive(
        &mut self,
        data: &[u8],
        socket: &Datagram,
        peer: SocketAddr,
    ) -> Result<Received, Error> {
        self.stream.get_mut().input(data)?;
        let mut unexpected = [0; 1];
        match self.stream.ssl_read(&mut unexpected) {
            Err(error) if matches!(error.code(), ErrorCode::WANT_READ | ErrorCode::WANT_WRITE) => {}
            Err(error) if error.code() == ErrorCode::ZERO_RETURN => {
                self.shutdown(socket, peer).await?;
                return Ok(Received::Closed);
            }
            _ => return Err(Error::Protocol),
        }
        tokio::time::timeout(Duration::from_millis(300), self.flush(socket, peer))
            .await
            .map_err(|_| Error::TimedOut)??;
        Ok(Received::Progress)
    }
    /// Send close_notify once, without waiting for the peer or receiving here.
    /// Delivery is best effort: the sole receive owner and route are released
    /// by the media session even if this bounded send fails.
    pub async fn shutdown(&mut self, socket: &Datagram, peer: SocketAddr) -> Result<(), Error> {
        match self.stream.shutdown() {
            Ok(_) => {}
            Err(error) if matches!(error.code(), ErrorCode::WANT_READ | ErrorCode::WANT_WRITE) => {}
            Err(_) => return Err(Error::Protocol),
        }
        tokio::time::timeout(Duration::from_millis(300), self.flush(socket, peer))
            .await
            .map_err(|_| Error::TimedOut)?
    }
    pub async fn tick(&mut self, socket: &Datagram, peer: SocketAddr) -> Result<(), Error> {
        self.handle_timeout()?;
        tokio::time::timeout(Duration::from_millis(300), self.flush(socket, peer))
            .await
            .map_err(|_| Error::TimedOut)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn agent(name: &str) -> ice::IceAgent {
        let credentials = ice::IceCredentials {
            ufrag: name.into(),
            pwd: format!("synthetic-ice-password-{name}"),
        };
        ice::IceAgent::new(credentials.clone(), credentials, false)
    }
    #[test]
    fn fingerprint_and_roles_are_strict_and_identity_is_ephemeral_redacted() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(
            Fingerprint::parse(&a.fingerprint().sdp()).unwrap(),
            a.fingerprint()
        );
        for input in ["sha-1 AA:BB", "sha-256 00", "sha-256 ", "sha-256 GG"] {
            assert!(Fingerprint::parse(input).is_err());
        }
        assert!(Setup::Actpass.local_role(Setup::Actpass).is_err());
        assert!(Setup::Active.local_role(Setup::Active).is_err());
        assert_eq!(
            Setup::Actpass.local_role(Setup::Passive).unwrap(),
            Role::Client
        );
        assert_eq!(format!("{a:?}"), "DTLSIdentity([REDACTED])");
    }
    #[tokio::test]
    async fn pinned_peers_export_opposite_keys_and_use_srtp_without_sdp_master_keys() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let fp_a = a.fingerprint();
        let fp_b = b.fingerprint();
        let left = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = left.local_addr().unwrap();
        let addr_b = right.local_addr().unwrap();
        let agent_a = agent("left");
        let agent_b = agent("right");
        let (a, b) = tokio::join!(
            a.handshake(&left, addr_b, &agent_a, fp_b, Role::Client),
            b.handshake(&right, addr_a, &agent_b, fp_a, Role::Server)
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(a.local.master_key, b.remote.master_key);
        assert_eq!(a.remote.master_key, b.local.master_key);
        assert_eq!(a.local.master_salt, b.remote.master_salt);
        assert_ne!(a.local.master_key, a.remote.master_key);
        let mut tx = super::super::srtp::create_context(&a.local, &a.remote).unwrap();
        let peer = super::super::srtp::create_context(&b.local, &b.remote).unwrap();
        let plain = super::super::rtp::encode(0, 1, 160, 42, &[255; 160]);
        let encrypted = super::super::srtp::protect(&mut tx, &plain).unwrap();
        assert_eq!(
            super::super::media_protection::RtpReceiver::new(&peer)
                .unprotect(&encrypted)
                .unwrap()
                .0,
            plain
        );
    }
    #[tokio::test]
    async fn authenticated_close_is_acknowledged_without_waiting_for_peer() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let fp_a = a.fingerprint();
        let fp_b = b.fingerprint();
        let left = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = left.local_addr().unwrap();
        let addr_b = right.local_addr().unwrap();
        let agent_a = agent("left");
        let agent_b = agent("right");
        let (a, b) = tokio::join!(
            a.handshake(&left, addr_b, &agent_a, fp_b, Role::Client),
            b.handshake(&right, addr_a, &agent_b, fp_a, Role::Server)
        );
        let mut a = a.unwrap().session;
        let mut b = b.unwrap().session;
        // No remote reader is running: local shutdown must only send, not wait.
        tokio::time::timeout(Duration::from_millis(500), a.shutdown(&left, addr_b))
            .await.unwrap().unwrap();
        let mut packet = [0; 2048];
        let (len, source) = tokio::time::timeout(Duration::from_secs(1), right.recv_from(&mut packet))
            .await.unwrap().unwrap();
        assert_eq!(source, addr_a);
        assert_eq!(packet[0], 21); // encrypted DTLS alert
        // Corrupt ciphertext cannot terminate the authenticated association.
        let mut corrupt = packet[..len].to_vec();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(b.receive(&corrupt, &right, addr_a).await.unwrap(), Received::Progress);
        assert_eq!(b.receive(&packet[..len], &right, addr_a).await.unwrap(), Received::Closed);
        let (len, source) = tokio::time::timeout(Duration::from_secs(1), left.recv_from(&mut packet))
            .await.unwrap().unwrap();
        assert_eq!(source, addr_b);
        assert_eq!(a.receive(&packet[..len], &left, addr_b).await.unwrap(), Received::Closed);
    }

    #[tokio::test]
    async fn wrong_fingerprint_fails_without_export_or_unprotected_fallback() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let fp_a = a.fingerprint();
        let mut wrong = b.fingerprint();
        wrong.0[0] ^= 1;
        let left = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = left.local_addr().unwrap();
        let addr_b = right.local_addr().unwrap();
        let agent_a = agent("left");
        let agent_b = agent("right");
        let (a, b) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                a.handshake(&left, addr_b, &agent_a, wrong, Role::Client),
                b.handshake(&right, addr_a, &agent_b, fp_a, Role::Server)
            )
        })
        .await
        .unwrap();
        assert!(a.is_err());
        assert!(b.is_err());
    }

    #[tokio::test]
    async fn stalled_relay_cannot_hold_shutdown_open() {
        struct Stalled;
        impl super::super::datagram::RelayIo for Stalled {
            fn local_addr(&self) -> io::Result<SocketAddr> { Ok("127.0.0.1:9".parse().unwrap()) }
            fn send_to<'a>(&'a self, _: &'a [u8], _: SocketAddr) -> super::super::datagram::IoFuture<'a, usize> {
                Box::pin(std::future::pending())
            }
            fn recv_from<'a>(&'a self, _: &'a mut [u8]) -> super::super::datagram::IoFuture<'a, (usize, SocketAddr)> {
                Box::pin(std::future::pending())
            }
            fn close(&self) -> super::super::datagram::IoFuture<'_, ()> { Box::pin(async { Ok(()) }) }
        }
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let fp_a = a.fingerprint();
        let fp_b = b.fingerprint();
        let left = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = left.local_addr().unwrap();
        let addr_b = right.local_addr().unwrap();
        let agent_a = agent("left");
        let agent_b = agent("right");
        let (a, b) = tokio::join!(
            a.handshake(&left, addr_b, &agent_a, fp_b, Role::Client),
            b.handshake(&right, addr_a, &agent_b, fp_a, Role::Server)
        );
        let mut a = a.unwrap().session;
        let _b = b.unwrap();
        let stalled = Datagram::Relay(std::sync::Arc::new(Stalled));
        let result = tokio::time::timeout(Duration::from_secs(1), a.shutdown(&stalled, addr_b))
            .await.unwrap();
        assert_eq!(result, Err(Error::TimedOut));
    }
}

#[cfg(test)]
mod retransmission_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn agent() -> ice::IceAgent {
        let credentials = ice::IceCredentials {
            ufrag: "fixture".into(),
            pwd: "synthetic-password-123456".into(),
        };
        ice::IceAgent::new(credentials.clone(), credentials, false)
    }
    fn has_ccs(mut packet: &[u8]) -> bool {
        while packet.len() >= 13 {
            if packet[0] == 20 {
                return true;
            }
            let len = u16::from_be_bytes([packet[11], packet[12]]) as usize + 13;
            if len > packet.len() {
                break;
            }
            packet = &packet[len..];
        }
        false
    }
    #[tokio::test]
    async fn lost_final_flight_recovers_with_the_retained_server_session() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let fp_a = a.fingerprint();
        let fp_b = b.fingerprint();
        let left = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = left.local_addr().unwrap();
        let addr_b = right.local_addr().unwrap();
        let proxy_a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let proxy_b = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_a = proxy_a.local_addr().unwrap();
        let peer_b = proxy_b.local_addr().unwrap();
        let dropped = std::sync::Arc::new(AtomicUsize::new(0));
        let count = dropped.clone();
        let forward = tokio::spawn(async move {
            let mut a = [0; 2048];
            let mut b = [0; 2048];
            loop {
                tokio::select! {
                    packet=proxy_a.recv_from(&mut a)=>{let(n,from)=packet.unwrap();assert_eq!(from,addr_a);proxy_b.send_to(&a[..n],addr_b).await.unwrap();},
                    packet=proxy_b.recv_from(&mut b)=>{let(n,from)=packet.unwrap();assert_eq!(from,addr_b);
                        if has_ccs(&b[..n])&&count.load(Ordering::SeqCst)==0 {count.fetch_add(1,Ordering::SeqCst);continue;}
                        proxy_a.send_to(&b[..n],addr_a).await.unwrap();
                    }
                }
            }
        });
        let (done, mut done_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            let mut server = b
                .handshake(&right, peer_b, &agent(), fp_a, Role::Server)
                .await
                .unwrap();
            let mut timer = tokio::time::interval(Duration::from_millis(100));
            let mut input = [0; 2048];
            loop {
                tokio::select! {
                    _=done_rx.changed()=>return (server.local,server.remote),
                    _=timer.tick()=>server.session.tick(&right,peer_b).await.unwrap(),
                    packet=right.recv_from(&mut input)=>{let(n,from)=packet.unwrap();assert_eq!(from,peer_b);server.session.receive(&input[..n],&right,peer_b).await.unwrap();},
                }
            }
        });
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            a.handshake(&left, peer_a, &agent(), fp_b, Role::Client),
        )
        .await
        .unwrap()
        .unwrap();
        done.send(true).unwrap();
        let (local, remote) = server.await.unwrap();
        assert_eq!(client.local.master_key, remote.master_key);
        assert_eq!(client.remote.master_key, local.master_key);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        forward.abort();
        let _ = forward.await;
    }
}

#[cfg(test)]
mod independent_tests {
    use super::*;
    use std::io::BufRead;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[tokio::test]
    #[ignore = "requires own OpenSSL loopback peer in TEAMS_LINUX_DTLS_PEER; no accounts/devices"]
    async fn independent_openssl_peer_confirms_fingerprint_profile_and_exporter_layout() {
        let identity = Identity::generate().unwrap();
        let binary = std::env::var("TEAMS_LINUX_DTLS_PEER").unwrap();
        let mut child = Child(
            std::process::Command::new(binary)
                .arg(identity.fingerprint().sdp())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut stdout = std::io::BufReader::new(child.0.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        let parts = ready.trim_end().splitn(3, ' ').collect::<Vec<_>>();
        assert_eq!(parts[0], "READY");
        let peer: SocketAddr = format!("127.0.0.1:{}", parts[1]).parse().unwrap();
        let pin = Fingerprint::parse(parts[2]).unwrap();
        let socket = Datagram::Direct(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let credentials = ice::IceCredentials {
            ufrag: "fixture".into(),
            pwd: "synthetic-password-123456".into(),
        };
        let agent = ice::IceAgent::new(credentials.clone(), credentials, false);
        let outcome = identity
            .handshake(&socket, peer, &agent, pin, Role::Client)
            .await
            .unwrap();
        let mut exported = String::new();
        stdout.read_line(&mut exported).unwrap();
        let exported = exported.trim_end().strip_prefix("EXPORTED ").unwrap();
        assert_eq!(exported.len(), 120);
        let bytes = (0..120)
            .step_by(2)
            .map(|i| u8::from_str_radix(&exported[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcome.local.master_key, bytes[..16]);
        assert_eq!(outcome.remote.master_key, bytes[16..32]);
        assert_eq!(outcome.local.master_salt, bytes[32..46]);
        assert_eq!(outcome.remote.master_salt, bytes[46..60]);
        assert!(child.0.wait().unwrap().success());
    }
}
