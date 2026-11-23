//! Own UDP/TCP/TLS TURN Send/Data transport (RFC8656), with one receive owner.
//! Authentication and discovery come from explicit, validated relay credentials.
use crate::call_turn_io::Connection;
use hmac::{Hmac, Mac};
use ost::calling::{
    datagram::{IoFuture, RelayIo},
    ice,
};
use sha1::Sha1;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, watch};
const COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
const ALLOCATE: u16 = 0x0003;
const REFRESH: u16 = 0x0004;
const PERMISSION: u16 = 0x0008;
fn failure() -> io::Error {
    io::Error::other("Native relay transaction failed")
}
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "Native relay closed")
}

#[derive(Clone)]
pub struct Credentials {
    username: String,
    password: String,
    realm: String,
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayCredentials([REDACTED])")
    }
}
impl Credentials {
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn into_webrtc_server(self, host: &str) -> teams_av::rtc::prelude::IceServer {
        teams_av::rtc::prelude::IceServer {
            urls: vec![format!("turn:{host}:3478?transport=udp"), format!("turns:{host}:443?transport=tcp")],
            username: self.username,
            password: self.password,
        }
    }
    pub fn new(username: &str, password: &str, realm: &str) -> io::Result<Self> {
        let realm = realm.trim_matches('"');
        if [username, password, realm]
            .iter()
            .any(|v| v.is_empty() || v.len() > 2048 || !v.bytes().all(|b| b.is_ascii_graphic()))
        {
            return Err(failure());
        }
        Ok(Self {
            username: username.into(),
            password: password.into(),
            realm: realm.into(),
        })
    }
    fn key(&self) -> [u8; 16] {
        md5::compute(format!(
            "{}:{}:{}",
            self.username, self.realm, self.password
        ))
        .0
    }
}
struct Auth {
    credentials: Credentials,
    nonce: String,
}
fn attr(packet: &mut Vec<u8>, kind: u16, value: &[u8]) {
    packet.extend(kind.to_be_bytes());
    packet.extend((value.len() as u16).to_be_bytes());
    packet.extend(value);
    while packet.len() % 4 != 0 {
        packet.push(0);
    }
}
fn header(kind: u16, transaction: [u8; 12]) -> Vec<u8> {
    let mut data = Vec::with_capacity(512);
    data.extend(kind.to_be_bytes());
    data.extend([0, 0]);
    data.extend(COOKIE);
    data.extend(transaction);
    data
}
fn finalize(data: &mut [u8]) {
    let length = (data.len() - 20) as u16;
    data[2..4].copy_from_slice(&length.to_be_bytes());
}
fn authenticated(
    kind: u16,
    transaction: [u8; 12],
    attributes: &[(u16, Vec<u8>)],
    auth: &Auth,
) -> Vec<u8> {
    let mut data = header(kind, transaction);
    for (kind, value) in attributes {
        attr(&mut data, *kind, value);
    }
    attr(&mut data, 0x0006, auth.credentials.username.as_bytes());
    attr(&mut data, 0x0014, auth.credentials.realm.as_bytes());
    attr(&mut data, 0x0015, auth.nonce.as_bytes());
    let length = (data.len() - 20 + 24) as u16;
    data[2..4].copy_from_slice(&length.to_be_bytes());
    let mut mac = Hmac::<Sha1>::new_from_slice(&auth.credentials.key()).unwrap();
    mac.update(&data);
    attr(&mut data, 0x0008, &mac.finalize().into_bytes());
    data
}
fn attributes(data: &[u8]) -> Option<Vec<(u16, &[u8])>> {
    if data.len() < 20
        || data.len() > 2048
        || data[4..8] != COOKIE
        || u16::from_be_bytes([data[2], data[3]]) as usize + 20 != data.len()
        || data.len() % 4 != 0
    {
        return None;
    }
    let mut cursor = 20;
    let mut attrs = Vec::new();
    let mut integrity = false;
    while cursor < data.len() {
        let head = data.get(cursor..cursor + 4)?;
        let kind = u16::from_be_bytes([head[0], head[1]]);
        let len = u16::from_be_bytes([head[2], head[3]]) as usize;
        let value = data.get(cursor + 4..cursor + 4 + len)?;
        if kind == 0x8028 {
            if len != 4
                || cursor + 8 != data.len()
                || value != (crate::call_nat::crc32(&data[..cursor]) ^ 0x5354554e).to_be_bytes()
            {
                return None;
            }
        }
        if integrity {
            if kind != 0x8028 || len != 4 || cursor + 8 != data.len() {
                return None;
            }
        } else {
            attrs.push((kind, value));
            if kind == 0x0008 {
                if len != 20 {
                    return None;
                }
                integrity = true;
            }
        }
        cursor += 4 + (len + 3) / 4 * 4;
        if cursor > data.len() || attrs.len() > 48 {
            return None;
        }
    }
    Some(attrs)
}
fn unique<'a>(attrs: &[(u16, &'a [u8])], kind: u16) -> Option<&'a [u8]> {
    let mut values = attrs.iter().filter(|(k, _)| *k == kind);
    let value = values.next()?.1;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}
fn text(value: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(value).ok()?;
    (!value.is_empty() && value.len() <= 2048 && value.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| value.into())
}
fn xor_address(address: SocketAddr) -> Vec<u8> {
    let IpAddr::V4(ip) = address.ip() else {
        return Vec::new();
    };
    let mut value = vec![0, 1];
    value.extend((address.port() ^ 0x2112).to_be_bytes());
    for (octet, key) in ip.octets().iter().zip(COOKIE) {
        value.push(octet ^ key);
    }
    value
}
fn read_address(value: &[u8]) -> Option<SocketAddr> {
    if value.len() != 8 || value[..2] != [0, 1] {
        return None;
    }
    let port = u16::from_be_bytes([value[2], value[3]]) ^ 0x2112;
    let ip = Ipv4Addr::new(
        value[4] ^ COOKIE[0],
        value[5] ^ COOKIE[1],
        value[6] ^ COOKIE[2],
        value[7] ^ COOKIE[3],
    );
    if port == 0 || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
        return None;
    }
    Some(SocketAddr::new(IpAddr::V4(ip), port))
}
async fn transaction(
    socket: &Connection,
    server: SocketAddr,
    request: &[u8],
    auth: Option<&Auth>,
) -> io::Result<Vec<u8>> {
    let mut buffer = [0u8; 2048];
    let start = tokio::time::Instant::now();
    let delays: &[u64] = if socket.reliable() {
        &[3000]
    } else {
        &[250, 750, 1750]
    };
    for &delay in delays {
        socket.send_to(request, server).await?;
        let end = start + Duration::from_millis(delay);
        loop {
            tokio::select! {biased;
                _=tokio::time::sleep_until(end)=>break,
                packet=socket.recv_from(&mut buffer)=>{let(size,from)=packet?;let response=&buffer[..size];
                    if from==server && response.len()>=20 && response[8..20]==request[8..20] && attributes(response).is_some() && response_usable(response,u16::from_be_bytes([request[0],request[1]]),auth){return Ok(response.to_vec());}
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Native relay timed out",
    ))
}
fn stale_nonce(response: &[u8], method: u16, auth: &Auth) -> Option<String> {
    if response.len() < 20 || response[..2] != (method | 0x0110).to_be_bytes() {
        return None;
    }
    let attrs = attributes(response)?;
    let error = unique(&attrs, 0x0009)?;
    if error.len() < 4 || error[..4] != [0, 0, 4, 38] {
        return None;
    }
    // SHA256/password-algorithm negotiation is a separate supported-mode gate.
    // Do not downgrade a server that requests it to our legacy MD5 mechanism.
    if unique(&attrs, 0x8002).is_some() || unique(&attrs, 0x001c).is_some() {
        return None;
    }
    if attrs.iter().any(|(kind, _)| *kind == 0x0008)
        && !ice::verify_message_integrity(response, &auth.credentials.key())
    {
        return None;
    }
    let realm = text(unique(&attrs, 0x0014)?)?;
    if realm != auth.credentials.realm {
        return None;
    }
    let nonce = text(unique(&attrs, 0x0015)?)?;
    if nonce.starts_with("obMatJos2") {
        return None;
    }
    Some(nonce)
}
fn response_usable(response: &[u8], method: u16, auth: Option<&Auth>) -> bool {
    match auth {
        None => response[..2] == (method | 0x0110).to_be_bytes(),
        Some(auth) => {
            stale_nonce(response, method, auth).is_some()
                || ((response[..2] == (method | 0x0100).to_be_bytes()
                    || response[..2] == (method | 0x0110).to_be_bytes())
                    && ice::verify_message_integrity(response, &auth.credentials.key()))
        }
    }
}
async fn authenticated_exchange(
    socket: &Connection,
    server: SocketAddr,
    method: u16,
    attrs: &[(u16, Vec<u8>)],
    auth: &mut Auth,
) -> io::Result<Vec<u8>> {
    for renewal in 0..=2 {
        let request = authenticated(method, ice::generate_transaction_id(), attrs, auth);
        let response = transaction(socket, server, &request, Some(auth)).await?;
        if let Some(nonce) = stale_nonce(&response, method, auth) {
            if nonce == auth.nonce || renewal == 2 {
                return Err(failure());
            }
            auth.nonce = nonce;
            continue;
        }
        success(&response, method, auth)?;
        return Ok(response);
    }
    Err(failure())
}

fn success(response: &[u8], method: u16, auth: &Auth) -> io::Result<()> {
    if response[..2] != (method | 0x0100).to_be_bytes()
        || !ice::verify_message_integrity(response, &auth.credentials.key())
    {
        return Err(failure());
    }
    Ok(())
}

struct Lease {
    socket: Arc<Connection>,
    server: SocketAddr,
    // The setup owner transfers authentication exactly once to the receive
    // worker. Nonce changes never live in detached credential snapshots.
    auth: Option<Auth>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(auth) = &self.auth {
            let request = authenticated(
                REFRESH,
                ice::generate_transaction_id(),
                &[(0x000d, vec![0, 0, 0, 0])],
                auth,
            );
            let _ = self.socket.try_send_to(&request, self.server);
            self.socket.close_now();
        }
    }
}
pub struct Allocation {
    lease: Lease,
    relay: SocketAddr,
    expires: tokio::time::Instant,
    credentials_until: Option<tokio::time::Instant>,
}
impl std::fmt::Debug for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnAllocation([REDACTED])")
    }
}
impl Allocation {
    pub async fn allocate(server: SocketAddr, credentials: Credentials) -> io::Result<Self> {
        if !server.is_ipv4()
            || server.port() == 0
            || server.ip().is_unspecified()
            || server.ip().is_multicast()
        {
            return Err(failure());
        }
        Self::allocate_on(Connection::udp().await?, server, credentials).await
    }
    pub async fn allocate_tcp(server: SocketAddr, credentials: Credentials) -> io::Result<Self> {
        Self::allocate_on(Connection::tcp(server).await?, server, credentials).await
    }
    pub async fn allocate_tls(
        server: SocketAddr,
        hostname: &str,
        credentials: Credentials,
    ) -> io::Result<Self> {
        Self::allocate_on(
            Connection::tls(server, hostname).await?,
            server,
            credentials,
        )
        .await
    }
    pub(crate) async fn allocate_on(
        socket: Connection,
        server: SocketAddr,
        credentials: Credentials,
    ) -> io::Result<Self> {
        let mut request = header(ALLOCATE, ice::generate_transaction_id());
        attr(&mut request, 0x0019, &[17, 0, 0, 0]);
        finalize(&mut request);
        let response = transaction(&socket, server, &request, None).await?;
        if response[..2] != (ALLOCATE | 0x0110).to_be_bytes() {
            return Err(failure());
        }
        let attrs = attributes(&response).ok_or_else(failure)?;
        let error = unique(&attrs, 0x0009).ok_or_else(failure)?;
        if error.len() < 4 || error[2] != 4 || error[3] != 1 {
            return Err(failure());
        }
        let realm = text(unique(&attrs, 0x0014).ok_or_else(failure)?).ok_or_else(failure)?;
        if realm.trim_matches('"') != credentials.realm {
            return Err(failure());
        }
        // Realm bytes participate in the key derivation; preserve exactly what
        // the authenticated TURN exchange uses after matching the TRAP realm.
        let credentials = Credentials {
            realm,
            ..credentials
        };
        let nonce = text(unique(&attrs, 0x0015).ok_or_else(failure)?).ok_or_else(failure)?;
        if nonce.starts_with("obMatJos2")
            || attrs
                .iter()
                .any(|(kind, _)| matches!(*kind, 0x8002 | 0x001c))
        {
            return Err(failure());
        }
        let mut lease = Lease {
            socket: Arc::new(socket),
            server,
            auth: Some(Auth { credentials, nonce }),
        };
        let response = authenticated_exchange(
            &lease.socket,
            server,
            ALLOCATE,
            &[(0x0019, vec![17, 0, 0, 0])],
            lease.auth.as_mut().unwrap(),
        )
        .await?;
        let attrs = attributes(&response).ok_or_else(failure)?;
        let relay =
            read_address(unique(&attrs, 0x0016).ok_or_else(failure)?).ok_or_else(failure)?;
        let lifetime = unique(&attrs, 0x000d)
            .and_then(|value| <[u8; 4]>::try_from(value).ok())
            .map(u32::from_be_bytes)
            .filter(|v| *v >= 30 && *v <= 3600)
            .ok_or_else(failure)?;
        Ok(Self {
            lease,
            relay,
            expires: tokio::time::Instant::now() + Duration::from_secs(lifetime as u64),
            credentials_until: None,
        })
    }

    pub async fn release(mut self) {
        if let Some(mut auth) = self.lease.auth.take() {
            let _ = tokio::time::timeout(
                Duration::from_millis(300),
                authenticated_exchange(
                    &self.lease.socket,
                    self.lease.server,
                    REFRESH,
                    &[(0x000d, vec![0, 0, 0, 0])],
                    &mut auth,
                ),
            )
            .await;
        }
        self.lease.socket.shutdown().await;
    }
    pub fn limit_credentials(mut self, valid_for: Duration) -> io::Result<Self> {
        if valid_for < Duration::from_secs(10) {
            return Err(failure());
        }
        self.credentials_until = Some(tokio::time::Instant::now() + valid_for);
        Ok(self)
    }
    #[cfg(test)]
    pub(crate) fn reliable(&self) -> bool {
        self.lease.socket.reliable()
    }
    pub fn base_addr(&self) -> io::Result<SocketAddr> {
        self.lease.socket.local_addr()
    }
    pub fn relay_addr(&self) -> SocketAddr {
        self.relay
    }
    pub async fn connect(mut self, peers: &[SocketAddr]) -> io::Result<Arc<Relayed>> {
        if self
            .credentials_until
            .is_some_and(|until| until <= tokio::time::Instant::now())
        {
            return Err(failure());
        }
        if self
            .expires
            .saturating_duration_since(tokio::time::Instant::now())
            < Duration::from_secs(10)
            || peers.is_empty()
            || peers.len() > 64
        {
            return Err(failure());
        }
        let mut allowed = Vec::new();
        let mut permission = Vec::new();
        for peer in peers {
            if !peer.is_ipv4()
                || peer.port() == 0
                || peer.ip().is_unspecified()
                || peer.ip().is_multicast()
            {
                return Err(failure());
            }
            if !allowed.contains(peer) {
                allowed.push(*peer);
            }
            if !permission.iter().any(|(_, value): &(u16, Vec<u8>)| {
                read_address(value).is_some_and(|p| p.ip() == peer.ip())
            }) {
                permission.push((0x0012, xor_address(*peer)));
            }
        }
        authenticated_exchange(
            &self.lease.socket,
            self.lease.server,
            PERMISSION,
            &permission,
            self.lease.auth.as_mut().unwrap(),
        )
        .await?;
        let socket = self.lease.socket.clone();
        let (inbox, receiver) = mpsc::channel(8);
        let (stop, stop_rx) = watch::channel(false);
        let (done, done_rx) = watch::channel(false);
        let io = Arc::new(Relayed {
            socket: socket.clone(),
            server: self.lease.server,
            relay: self.relay,
            allowed: allowed.clone(),
            receiver: Mutex::new(receiver),
            stop,
            done: done_rx,
        });
        let server = self.lease.server;
        let auth = self.lease.auth.take().unwrap();
        let lifetime = self
            .expires
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs() as u32;
        let credentials_until = self
            .credentials_until
            .unwrap_or(tokio::time::Instant::now() + Duration::from_secs(30 * 86400));
        tokio::spawn(async move {
            let _ = relay_worker(
                socket,
                server,
                auth,
                lifetime,
                credentials_until,
                allowed,
                permission,
                inbox,
                stop_rx,
            )
            .await;
            let _ = done.send(true);
        });
        Ok(io)
    }
}

pub struct Relayed {
    socket: Arc<Connection>,
    server: SocketAddr,
    relay: SocketAddr,
    allowed: Vec<SocketAddr>,
    receiver: Mutex<mpsc::Receiver<(SocketAddr, Vec<u8>)>>,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
impl Drop for Relayed {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
impl RelayIo for Relayed {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.relay)
    }
    fn send_to<'a>(&'a self, data: &'a [u8], peer: SocketAddr) -> IoFuture<'a, usize> {
        Box::pin(async move {
            if *self.stop.borrow() || *self.done.borrow() {
                return Err(stopped());
            }
            if data.len() > 1400 || !self.allowed.contains(&peer) {
                return Err(failure());
            }
            let mut packet = header(0x0016, ice::generate_transaction_id());
            attr(&mut packet, 0x0012, &xor_address(peer));
            attr(&mut packet, 0x0013, data);
            finalize(&mut packet);
            self.socket.send_to(&packet, self.server).await?;
            Ok(data.len())
        })
    }
    fn recv_from<'a>(&'a self, buffer: &'a mut [u8]) -> IoFuture<'a, (usize, SocketAddr)> {
        Box::pin(async move {
            let mut receiver = self.receiver.lock().await;
            let (peer, data) = receiver.recv().await.ok_or_else(stopped)?;
            if data.len() > buffer.len() {
                return Err(failure());
            }
            buffer[..data.len()].copy_from_slice(&data);
            Ok((data.len(), peer))
        })
    }
    fn close(&self) -> IoFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.stop.send(true);
            let mut done = self.done.clone();
            tokio::time::timeout(Duration::from_secs(3), async {
                while !*done.borrow_and_update() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .map_err(|_| failure())?;
            Ok(())
        })
    }
}

async fn relay_worker(
    socket: Arc<Connection>,
    server: SocketAddr,
    mut auth: Auth,
    lifetime: u32,
    credentials_until: tokio::time::Instant,
    allowed: Vec<SocketAddr>,
    permissions: Vec<(u16, Vec<u8>)>,
    inbox: mpsc::Sender<(SocketAddr, Vec<u8>)>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let result=async{
        let mut buffer=[0;2048];let interval=Duration::from_secs((lifetime/2).min(120) as u64);
        let mut refresh=tokio::time::interval_at(tokio::time::Instant::now()+interval,interval);refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        struct Pending {transaction:[u8;12],method:u16,attributes:Vec<(u16,Vec<u8>)>,nonce:String,renewals:u8,request:Vec<u8>,attempts:u32,next:tokio::time::Instant}
        let mut pending=Vec::<Pending>::new();
        let mut watchdog=tokio::time::interval(Duration::from_millis(100));watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {tokio::select!{biased;
            _=stop.changed()=>return Ok(()),
            _=tokio::time::sleep_until(credentials_until)=>return Err(failure()),
            _=watchdog.tick()=>{
                let now=tokio::time::Instant::now();
                for request in &mut pending {
                    if now>=request.next {
                        if socket.reliable() || request.attempts>=3{return Err(failure());}
                        socket.send_to(&request.request,server).await?;request.attempts+=1;
                        request.next=now+Duration::from_millis(500*(1u64<<(request.attempts-1)));
                    }
                }
            },
            _=refresh.tick()=>{
                if !pending.is_empty(){return Err(failure());}
                for (method,attrs) in [(REFRESH,vec![(0x000d,lifetime.to_be_bytes().to_vec())]),(PERMISSION,permissions.clone())] {
                    let transaction=ice::generate_transaction_id();let request=authenticated(method,transaction,&attrs,&auth);socket.send_to(&request,server).await?;pending.push(Pending{transaction,method,attributes:attrs,nonce:auth.nonce.clone(),renewals:0,request,attempts:1,next:tokio::time::Instant::now()+Duration::from_millis(if socket.reliable(){3000}else{500})});
                }
            },
            packet=socket.recv_from(&mut buffer)=>{
                let(size,source)=packet?;if source!=server{continue;}let data=&buffer[..size];let Some(attrs)=attributes(data)else{continue;};
                if data[..2]==[0,0x17] {
                    let Some(peer)=unique(&attrs,0x0012).and_then(read_address)else{continue;};
                    let Some(payload)=unique(&attrs,0x0013)else{continue;};
                    if payload.len()<=1400 && allowed.contains(&peer){let _=inbox.try_send((peer,payload.to_vec()));}
                }else if let Some(index)=pending.iter().position(|request|data[8..20]==request.transaction){
                    let method=pending[index].method;
                    if let Some(nonce)=stale_nonce(data,method,&auth) {
                        let request=&mut pending[index];
                        if request.renewals>=2{return Err(failure());}
                        if request.nonce==auth.nonce {
                            if nonce==auth.nonce{return Err(failure());}
                            auth.nonce=nonce;
                        }
                        request.renewals+=1;request.nonce=auth.nonce.clone();
                        request.transaction=ice::generate_transaction_id();
                        request.request=authenticated(method,request.transaction,&request.attributes,&auth);
                        request.attempts=1;request.next=tokio::time::Instant::now()+Duration::from_millis(if socket.reliable(){3000}else{500});
                        socket.send_to(&request.request,server).await?;continue;
                    }
                    if !response_usable(data,method,Some(&auth)){continue;}
                    success(data,method,&auth)?;
                    if method==REFRESH {
                        let seconds=unique(&attrs,0x000d).and_then(|v|<[u8;4]>::try_from(v).ok()).map(u32::from_be_bytes).filter(|v|*v>=30&&*v<=3600).ok_or_else(failure)?;
                        let interval=Duration::from_secs((seconds/2).min(120) as u64);
                        refresh=tokio::time::interval_at(tokio::time::Instant::now()+interval,interval);refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                    pending.swap_remove(index);
                }
            }
        }}
    }.await;
    // Finish with the latest nonce, and recover a stale deletion challenge
    // within the same bounded shutdown deadline as a normal release.
    let _ = tokio::time::timeout(
        Duration::from_millis(300),
        authenticated_exchange(
            &socket,
            server,
            REFRESH,
            &[(0x000d, vec![0, 0, 0, 0])],
            &mut auth,
        ),
    )
    .await;
    socket.shutdown().await;
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::UdpSocket;
    pub(crate) struct Fixture {
        pub server: SocketAddr,
        pub relay: SocketAddr,
        pub forwarded: Arc<AtomicUsize>,
        pub refreshed: Arc<AtomicUsize>,
        pub challenges: Arc<AtomicUsize>,
        pub released: tokio::sync::oneshot::Receiver<()>,
        pub worker: tokio::task::JoinHandle<()>,
    }
    pub(crate) fn is_delete_request(data: &[u8]) -> bool {
        data.len() >= 20
            && data[..2] == REFRESH.to_be_bytes()
            && attributes(data)
                .is_some_and(|attrs| unique(&attrs, 0x000d) == Some(&[0, 0, 0, 0][..]))
    }
    pub(crate) fn is_stale_response(data: &[u8]) -> bool {
        attributes(data)
            .and_then(|attrs| {
                unique(&attrs, 0x0009).map(|error| error.len() >= 4 && error[..4] == [0, 0, 4, 38])
            })
            .unwrap_or(false)
    }
    pub(crate) async fn fixture() -> Fixture {
        fixture_challenged(None, true).await
    }
    pub(crate) async fn fixture_stale_delete() -> Fixture {
        fixture_challenged(Some(Stage::Delete), true).await
    }
    #[tokio::test]
    async fn tcp_and_tls_recover_nonce_and_release_on_the_same_connection() {
        for tls in [false, true] {
            for stage in [
                Stage::Allocate,
                Stage::Permission,
                Stage::Refresh,
                Stage::Delete,
            ] {
                let fixture = fixture_transport(Some(stage), true, false).await;
                let (addr, bridge) = crate::call_turn_io::tests::bridge(fixture.server, tls).await;
                let io = if tls {
                    Connection::tls_with_roots(
                        addr,
                        "relay.fixture.invalid",
                        crate::call_turn_io::tests::fixture_roots(),
                    )
                    .await
                    .unwrap()
                } else {
                    Connection::tcp(addr).await.unwrap()
                };
                let mut allocation = Allocation::allocate_on(
                    io,
                    addr,
                    Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
                )
                .await
                .unwrap();
                if stage == Stage::Refresh {
                    allocation.expires = tokio::time::Instant::now() + Duration::from_secs(12);
                }
                let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let relay = allocation
                    .connect(&[peer.local_addr().unwrap()])
                    .await
                    .unwrap();
                if stage == Stage::Refresh {
                    tokio::time::timeout(Duration::from_secs(8), async {
                        while fixture.refreshed.load(Ordering::SeqCst) < 1 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .unwrap();
                }
                relay
                    .send_to(b"encoded-fixture", peer.local_addr().unwrap())
                    .await
                    .unwrap();
                let mut bytes = [0; 100];
                let (n, from) = peer.recv_from(&mut bytes).await.unwrap();
                assert_eq!(&bytes[..n], b"encoded-fixture");
                peer.send_to(b"return-fixture", from).await.unwrap();
                let (n, _) = relay.recv_from(&mut bytes).await.unwrap();
                assert_eq!(&bytes[..n], b"return-fixture");
                relay.close().await.unwrap();
                tokio::time::timeout(Duration::from_secs(1), fixture.released)
                    .await
                    .unwrap()
                    .unwrap();
                fixture.worker.await.unwrap();
                tokio::time::timeout(Duration::from_secs(1), bridge)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(fixture.challenges.load(Ordering::SeqCst) >= 1);
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Stage {
        Allocate,
        Permission,
        Refresh,
        Delete,
    }
    async fn fixture_challenged(stage: Option<Stage>, signed: bool) -> Fixture {
        fixture_transport(stage, signed, true).await
    }
    async fn fixture_transport(
        stage: Option<Stage>,
        signed: bool,
        drop_first_refresh: bool,
    ) -> Fixture {
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = control.local_addr().unwrap();
        let transport = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = transport.local_addr().unwrap();
        let (released, release_rx) = tokio::sync::oneshot::channel();
        let forwarded = Arc::new(AtomicUsize::new(0));
        let count = forwarded.clone();
        let refreshed = Arc::new(AtomicUsize::new(0));
        let refreshes = refreshed.clone();
        let challenges = Arc::new(AtomicUsize::new(0));
        let challenge_count = challenges.clone();
        let worker = tokio::spawn(async move {
            let mut auth = Auth {
                credentials: Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                    .unwrap(),
                nonce: "fixture-nonce".into(),
            };
            let mut rolled = false;
            let mut challenged_transactions = Vec::new();
            let mut from = None;
            let mut permissions = Vec::new();
            let mut input = [0; 2048];
            let mut media = [0; 2048];
            let deadline = tokio::time::sleep(Duration::from_secs(20));
            tokio::pin!(deadline);
            loop {
                tokio::select! {biased;
                    _=&mut deadline=>panic!("relay fixture timed out"),
                    packet=control.recv_from(&mut input)=>{
                        let(size,source)=packet.unwrap();let data=&input[..size];let attrs=attributes(data).unwrap();let method=u16::from_be_bytes([data[0],data[1]]);let tx=data[8..20].try_into().unwrap();
                        if method==ALLOCATE && unique(&attrs,0x0008).is_none() {
                            let mut response=header(ALLOCATE|0x0110,tx);attr(&mut response,0x0009,&[0,0,4,1]);attr(&mut response,0x0014,b"rtcmedia");attr(&mut response,0x0015,auth.nonce.as_bytes());finalize(&mut response);control.send_to(&response,source).await.unwrap();continue;
                        }
                        if method!=0x0016 {
                            assert!(ice::verify_message_integrity(data,&auth.credentials.key()),"TURN authentication failed");
                            assert_eq!(unique(&attrs,0x0006),Some(b"fixture-user".as_slice()));
                            let deleting=method==REFRESH && unique(&attrs,0x000d)==Some(&[0,0,0,0][..]);
                            let target=match stage {Some(Stage::Allocate)=>method==ALLOCATE,Some(Stage::Permission)=>method==PERMISSION,Some(Stage::Refresh)=>method==REFRESH&&!deleting,Some(Stage::Delete)=>deleting,None=>false};
                            if target&&!rolled {auth.nonce="renewed-fixture-nonce".into();rolled=true;}
                            if unique(&attrs,0x0015)!=Some(auth.nonce.as_bytes()) {
                                challenge_count.fetch_add(1,Ordering::SeqCst);challenged_transactions.push(tx);
                                let response=challenge(method,tx,&auth,signed);
                                control.send_to(&response,source).await.unwrap();continue;
                            }
                        }
                        assert!(!challenged_transactions.contains(&tx),"a nonce renewal must start a new transaction");
                        let response=match method {
                            ALLOCATE=>{from=Some(source);authenticated(ALLOCATE|0x0100,tx,&[(0x0016,xor_address(relay)),(0x000d,600u32.to_be_bytes().to_vec())],&auth)},
                            PERMISSION=>{assert_eq!(from,Some(source));permissions=attrs.iter().filter(|(kind,_)|*kind==0x0012).map(|(_,value)|read_address(value).unwrap().ip()).collect();authenticated(PERMISSION|0x0100,tx,&[],&auth)},
                            REFRESH=>{
                                if unique(&attrs,0x000d)==Some(&[0,0,0,0][..]){let response=authenticated(REFRESH|0x0100,tx,&[(0x000d,vec![0,0,0,0])],&auth);control.send_to(&response,source).await.unwrap();let _=released.send(());break;}
                                if refreshes.fetch_add(1,Ordering::SeqCst)==0 && drop_first_refresh{continue;}authenticated(REFRESH|0x0100,tx,&[(0x000d,600u32.to_be_bytes().to_vec())],&auth)
                            },
                            0x0016=>{assert_eq!(from,Some(source));let peer=read_address(unique(&attrs,0x0012).unwrap()).unwrap();assert!(permissions.contains(&peer.ip()));let payload=unique(&attrs,0x0013).unwrap();transport.send_to(payload,peer).await.unwrap();count.fetch_add(1,Ordering::SeqCst);continue;},
                            _=>panic!("unexpected TURN method"),
                        };
                        control.send_to(&response,source).await.unwrap();
                    },
                    packet=transport.recv_from(&mut media)=>{
                        let(size,peer)=packet.unwrap();if !permissions.contains(&peer.ip()){continue;}
                        let Some(client)=from else{continue;};let mut packet=header(0x0017,ice::generate_transaction_id());attr(&mut packet,0x0012,&xor_address(peer));attr(&mut packet,0x0013,&media[..size]);finalize(&mut packet);control.send_to(&packet,client).await.unwrap();
                    }
                }
            }
        });
        Fixture {
            server,
            relay,
            forwarded,
            refreshed,
            challenges,
            released: release_rx,
            worker,
        }
    }
    fn challenge(method: u16, transaction: [u8; 12], auth: &Auth, signed: bool) -> Vec<u8> {
        if signed {
            return authenticated(
                method | 0x0110,
                transaction,
                &[(0x0009, vec![0, 0, 4, 38])],
                auth,
            );
        }
        let mut response = header(method | 0x0110, transaction);
        attr(&mut response, 0x0009, &[0, 0, 4, 38]);
        attr(&mut response, 0x0014, auth.credentials.realm.as_bytes());
        attr(&mut response, 0x0015, auth.nonce.as_bytes());
        finalize(&mut response);
        response
    }

    #[tokio::test]
    async fn stale_nonce_recovers_allocate_permission_maintenance_and_deletion_on_the_same_relay() {
        for (stage, signed) in [
            (Stage::Allocate, true),
            (Stage::Permission, false),
            (Stage::Refresh, true),
            (Stage::Delete, false),
        ] {
            let fixture = fixture_challenged(Some(stage), signed).await;
            let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let endpoint = peer.local_addr().unwrap();
            let mut allocation = Allocation::allocate(
                fixture.server,
                Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
            )
            .await
            .unwrap();
            let local = allocation.base_addr().unwrap();
            let relay = allocation.relay_addr();
            if stage == Stage::Refresh {
                allocation.expires = tokio::time::Instant::now() + Duration::from_secs(12);
            }
            let io = allocation.connect(&[endpoint]).await.unwrap();
            if stage == Stage::Refresh {
                tokio::time::timeout(Duration::from_secs(8), async {
                    while fixture.refreshed.load(Ordering::SeqCst) < 2 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
            io.send_to(b"still-connected", endpoint).await.unwrap();
            let mut buffer = [0; 64];
            let (size, source) =
                tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(source, relay);
            assert_eq!(&buffer[..size], b"still-connected");
            io.close().await.unwrap();
            drop(io);
            fixture.released.await.unwrap();
            fixture.worker.await.unwrap();
            assert!(fixture.challenges.load(Ordering::SeqCst) > 0);
            let _rebind = UdpSocket::bind(local).await.unwrap();
        }
    }

    #[tokio::test]
    async fn explicit_prepared_media_close_recovers_an_expired_nonce_before_releasing_ports() {
        let fixture = fixture_challenged(Some(Stage::Delete), true).await;
        let allocation = Allocation::allocate(
            fixture.server,
            Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
        )
        .await
        .unwrap();
        let base = allocation.base_addr().unwrap();
        let mut prepared = crate::call_media::PreparedAudio::bind(
            "127.0.0.1:0".parse().unwrap(),
            &["127.0.0.1".parse().unwrap()],
        )
        .await
        .unwrap();
        let host = prepared.local_addr().unwrap();
        prepared.use_relay(allocation).unwrap();
        prepared.close().await;
        fixture.released.await.unwrap();
        fixture.worker.await.unwrap();
        assert_eq!(fixture.challenges.load(Ordering::SeqCst), 1);
        let _host = UdpSocket::bind(host).await.unwrap();
        let _base = UdpSocket::bind(base).await.unwrap();
    }

    #[tokio::test]
    async fn repeated_challenges_are_bounded_and_bad_integrity_is_ignored_until_a_valid_response() {
        for endless in [false, true] {
            let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let endpoint = server.local_addr().unwrap();
            let socket = Connection::Udp(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let worker = tokio::spawn(async move {
                let mut buffer = [0; 2048];
                let auth = Auth {
                    credentials: Credentials::new("user", "password", "rtcmedia").unwrap(),
                    nonce: "unused".into(),
                };
                let mut seen = Vec::new();
                for index in 0..if endless { 3 } else { 1 } {
                    let (size, peer) = server.recv_from(&mut buffer).await.unwrap();
                    let transaction = buffer[8..20].try_into().unwrap();
                    assert!(!seen.contains(&transaction));
                    seen.push(transaction);
                    assert!(ice::verify_message_integrity(
                        &buffer[..size],
                        &auth.credentials.key()
                    ));
                    if endless {
                        let fresh = Auth {
                            credentials: Credentials::new("user", "password", "rtcmedia").unwrap(),
                            nonce: format!("nonce-{}", index + 1),
                        };
                        let response = challenge(PERMISSION, transaction, &fresh, true);
                        server.send_to(&response, peer).await.unwrap();
                    } else {
                        let response = authenticated(PERMISSION | 0x0100, transaction, &[], &auth);
                        let mut bad = response.clone();
                        *bad.last_mut().unwrap() ^= 1;
                        server.send_to(&bad, peer).await.unwrap();
                        server.send_to(&response, peer).await.unwrap();
                    }
                }
            });
            let mut auth = Auth {
                credentials: Credentials::new("user", "password", "rtcmedia").unwrap(),
                nonce: "initial".into(),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                authenticated_exchange(
                    &socket,
                    endpoint,
                    PERMISSION,
                    &[(0x0012, xor_address("127.0.0.1:5000".parse().unwrap()))],
                    &mut auth,
                ),
            )
            .await
            .unwrap();
            assert_eq!(result.is_ok(), !endless);
            worker.await.unwrap();
        }
    }

    #[test]
    fn stale_nonce_never_changes_realm_or_accepts_tampered_integrity_or_security_downgrade() {
        let auth = Auth {
            credentials: Credentials::new("user", "password", "rtcmedia").unwrap(),
            nonce: "current".into(),
        };
        let fresh = Auth {
            credentials: Credentials::new("user", "password", "rtcmedia").unwrap(),
            nonce: "fresh".into(),
        };
        for signed in [false, true] {
            let packet = challenge(REFRESH, [1; 12], &fresh, signed);
            assert_eq!(stale_nonce(&packet, REFRESH, &auth), Some("fresh".into()));
        }
        let mut bad = challenge(REFRESH, [1; 12], &fresh, true);
        *bad.last_mut().unwrap() ^= 1;
        assert!(stale_nonce(&bad, REFRESH, &auth).is_none());
        assert!(!response_usable(&bad, REFRESH, Some(&auth)));
        let changed = Auth {
            credentials: Credentials::new("user", "password", "other-realm").unwrap(),
            nonce: "fresh".into(),
        };
        assert!(
            stale_nonce(
                &challenge(REFRESH, [1; 12], &changed, false),
                REFRESH,
                &auth
            )
            .is_none()
        );
        let modern = Auth {
            credentials: Credentials::new("user", "password", "rtcmedia").unwrap(),
            nonce: "obMatJos2AAAAunsupported".into(),
        };
        assert!(
            stale_nonce(&challenge(REFRESH, [1; 12], &modern, false), REFRESH, &auth).is_none()
        );
        assert!(
            stale_nonce(
                &challenge(PERMISSION, [1; 12], &fresh, true),
                REFRESH,
                &auth
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn allocation_permission_data_maintenance_and_close_use_the_owned_relay() {
        let fixture = fixture().await;
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_address = peer.local_addr().unwrap();
        let mut allocation = Allocation::allocate(
            fixture.server,
            Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(allocation.relay_addr(), fixture.relay);
        let local = allocation.base_addr().unwrap();
        // Shorten only this local test's initial lease to exercise maintenance.
        allocation.expires = tokio::time::Instant::now() + Duration::from_secs(12);
        let io = allocation.connect(&[peer_address]).await.unwrap();
        assert_eq!(io.send_to(b"outgoing", peer_address).await.unwrap(), 8);
        let mut buffer = [0; 256];
        let (size, source) = peer.recv_from(&mut buffer).await.unwrap();
        assert_eq!(source, fixture.relay);
        assert_eq!(&buffer[..size], b"outgoing");
        peer.send_to(b"incoming", source).await.unwrap();
        let (size, source) = io.recv_from(&mut buffer).await.unwrap();
        assert_eq!(source, peer_address);
        assert_eq!(&buffer[..size], b"incoming");
        assert!(
            io.send_to(b"blocked", "127.0.0.1:9".parse().unwrap())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(7), async {
            while fixture.refreshed.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        io.close().await.unwrap();
        assert!(io.send_to(b"closed", peer_address).await.is_err());
        drop(io);
        fixture.released.await.unwrap();
        fixture.worker.await.unwrap();
        let _rebind = UdpSocket::bind(local).await.unwrap();
    }
    #[tokio::test]
    async fn dropping_an_unconnected_allocation_releases_it_without_opening_audio() {
        let fixture = fixture().await;
        let allocation = Allocation::allocate(
            fixture.server,
            Credentials::new("fixture-user", "fixture-secret", "\"rtcmedia\"").unwrap(),
        )
        .await
        .unwrap();
        drop(allocation);
        tokio::time::timeout(Duration::from_secs(1), fixture.released)
            .await
            .unwrap()
            .unwrap();
        fixture.worker.await.unwrap();
    }
    #[tokio::test]
    async fn cancelling_authenticated_allocate_sends_release_from_the_same_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let endpoint = server.local_addr().unwrap();
        let (seen, arrival) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut buffer = [0; 2048];
            let (size, peer) = server.recv_from(&mut buffer).await.unwrap();
            let tx = buffer[8..20].try_into().unwrap();
            assert_eq!(size, 28);
            let mut challenge = header(ALLOCATE | 0x0110, tx);
            attr(&mut challenge, 0x0009, &[0, 0, 4, 1]);
            attr(&mut challenge, 0x0014, b"rtcmedia");
            attr(&mut challenge, 0x0015, b"fixture-nonce");
            finalize(&mut challenge);
            server.send_to(&challenge, peer).await.unwrap();
            let (size, same) = server.recv_from(&mut buffer).await.unwrap();
            assert_eq!(same, peer);
            let auth = Auth {
                credentials: Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                    .unwrap(),
                nonce: "fixture-nonce".into(),
            };
            assert!(ice::verify_message_integrity(
                &buffer[..size],
                &auth.credentials.key()
            ));
            seen.send(()).unwrap();
            let (size, same) =
                tokio::time::timeout(Duration::from_secs(1), server.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(same, peer);
            assert_eq!(buffer[..2], REFRESH.to_be_bytes());
            assert!(ice::verify_message_integrity(
                &buffer[..size],
                &auth.credentials.key()
            ));
            assert_eq!(
                unique(&attributes(&buffer[..size]).unwrap(), 0x000d),
                Some(&[0, 0, 0, 0][..])
            );
            peer
        });
        let mut pending = Box::pin(Allocation::allocate(
            endpoint,
            Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
        ));
        tokio::select! {_=arrival=>{},_=pending.as_mut()=>panic!("fixture unexpectedly allocated")}
        drop(pending);
        let local = worker.await.unwrap();
        let _rebound = UdpSocket::bind(local).await.unwrap();
    }

    #[tokio::test]
    async fn unexpected_realm_never_receives_a_credential_response() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let endpoint = server.local_addr().unwrap();
        let worker = tokio::spawn(async move {
            let mut buffer = [0; 2048];
            let (_, peer) = server.recv_from(&mut buffer).await.unwrap();
            let tx = buffer[8..20].try_into().unwrap();
            let mut challenge = header(ALLOCATE | 0x0110, tx);
            attr(&mut challenge, 0x0009, &[0, 0, 4, 1]);
            attr(&mut challenge, 0x0014, b"unexpected-realm");
            attr(&mut challenge, 0x0015, b"nonce");
            finalize(&mut challenge);
            server.send_to(&challenge, peer).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), server.recv_from(&mut buffer))
                    .await
                    .is_err()
            );
        });
        assert!(
            Allocation::allocate(
                endpoint,
                Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap()
            )
            .await
            .is_err()
        );
        worker.await.unwrap();
    }

    #[test]
    fn malformed_turn_attributes_addresses_and_credentials_are_rejected() {
        assert!(Credentials::new("a", "b", "rtcmedia").is_ok());
        for value in ["", "\r\n", "invalid space"] {
            assert!(Credentials::new(value, "b", "rtcmedia").is_err());
        }
        let auth = Auth {
            credentials: Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
            nonce: "nonce".into(),
        };
        let valid = authenticated(
            REFRESH,
            [1; 12],
            &[(0x000d, 600u32.to_be_bytes().to_vec())],
            &auth,
        );
        for size in 0..valid.len() {
            assert!(attributes(&valid[..size]).is_none());
        }
        assert!(ice::verify_message_integrity(
            &valid,
            &auth.credentials.key()
        ));
        let mut tampered = valid;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(success(&tampered, REFRESH, &auth).is_err());
        assert!(read_address(&[0; 8]).is_none());
        assert_eq!(
            read_address(&xor_address("203.0.113.2:3478".parse().unwrap())),
            Some("203.0.113.2:3478".parse().unwrap())
        );
    }
}
