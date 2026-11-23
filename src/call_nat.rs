//! Bounded, unauthenticated STUN discovery on the socket later used for media.
//! A mapped address is only a candidate: authenticated ICE still selects a route.
use ost::calling::ice;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::net::UdpSocket;

const COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
        }
    }
    !crc
}

/// Validate the envelope and one IPv4 XOR-MAPPED-ADDRESS. Source/transaction
/// matching are mandatory; malformed or ambiguous mappings never enter SDP.
fn mapped(data: &[u8], transaction: &[u8; 12]) -> Option<SocketAddr> {
    if data.len() < 20 || data[..2] != [1, 1] || data[4..8] != COOKIE || &data[8..20] != transaction
    {
        return None;
    }
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    if length % 4 != 0 || length + 20 != data.len() {
        return None;
    }
    let mut cursor = 20;
    let mut found = None;
    while cursor < data.len() {
        let header = data.get(cursor..cursor + 4)?;
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let size = u16::from_be_bytes([header[2], header[3]]) as usize;
        let value = data.get(cursor + 4..cursor + 4 + size)?;
        if kind == 0x0020 {
            if found.is_some() || value.len() != 8 || value[..2] != [0, 1] {
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
            found = Some(SocketAddr::new(IpAddr::V4(ip), port));
        } else if kind == 0x8028 {
            if size != 4 || cursor + 8 != data.len() {
                return None;
            }
            let expected = crc32(&data[..cursor]) ^ 0x5354554e;
            if value != expected.to_be_bytes() {
                return None;
            }
        }
        // Unknown comprehension-required attributes make this response unusable.
        else if kind < 0x8000 && !matches!(kind, 0x0001 | 0x0008 | 0x001c) {
            return None;
        }
        cursor = cursor.checked_add(4 + (size + 3) / 4 * 4)?;
        if cursor > data.len() {
            return None;
        }
    }
    found
}

pub async fn gather(
    socket: &UdpSocket,
    servers: &[SocketAddr],
    base: IpAddr,
) -> Option<ice::IceCandidate> {
    if !base.is_ipv4()
        || base.is_unspecified()
        || base.is_multicast()
        || servers.is_empty()
        || servers.len() > 4
    {
        return None;
    }
    let local = socket.local_addr().ok()?;
    if !local.is_ipv4() {
        return None;
    }
    let mut probes = Vec::new();
    for server in servers {
        if !server.is_ipv4()
            || server.port() == 0
            || server.ip().is_unspecified()
            || server.ip().is_multicast()
            || probes.iter().any(|(address, _, _)| address == server)
        {
            continue;
        }
        let transaction = ice::generate_transaction_id();
        probes.push((
            *server,
            transaction,
            ice::build_stun_binding_request(&transaction),
        ));
    }
    if probes.is_empty() {
        return None;
    }
    let mut buffer = [0u8; 2048];
    let start = tokio::time::Instant::now();
    for retry in 0..3 {
        for (server, _, request) in &probes {
            let _ = socket.send_to(request, *server).await;
        }
        let end = start + Duration::from_millis([250, 750, 1500][retry]);
        loop {
            tokio::select! {biased;
                _=tokio::time::sleep_until(end)=>break,
                packet=socket.recv_from(&mut buffer)=>{
                    let (size,source)=packet.ok()?;
                    let Some((_,transaction,_))=probes.iter().find(|(server,_,_)|*server==source)else{continue;};
                    let Some(address)=mapped(&buffer[..size],transaction)else{continue;};
                    if address.ip()==base && address.port()==local.port(){return None;}
                    return Some(ice::IceCandidate{foundation:"srflx1".into(),component:1,transport:ice::Transport::Udp,
                        priority:((100u32<<24)|(1<<8)|255),address:address.ip().to_string(),port:address.port(),candidate_type:ice::CandidateType::ServerReflexive,raddr:Some(base.to_string()),rport:Some(local.port())});
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn discovery_ignores_wrong_source_transaction_and_invalid_mapping_then_preserves_media_socket()
     {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = socket.local_addr().unwrap();
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let task = tokio::spawn(async move {
            let mut buffer = [0; 256];
            let (size, peer) = server.recv_from(&mut buffer).await.unwrap();
            assert_eq!(size, 20);
            let transaction: [u8; 12] = buffer[8..20].try_into().unwrap();
            let mapped = "203.0.113.22:45678".parse().unwrap();
            let valid = ice::build_binding_response(&transaction, mapped, None);
            attacker.send_to(&valid, peer).await.unwrap();
            let wrong = ice::build_binding_response(&[9; 12], mapped, None);
            server.send_to(&wrong, peer).await.unwrap();
            let mut malformed = valid.clone();
            malformed[2..4].copy_from_slice(&0u16.to_be_bytes());
            server.send_to(&malformed, peer).await.unwrap();
            server.send_to(&valid, peer).await.unwrap();
        });
        let candidate = gather(&socket, &[address], local.ip()).await.unwrap();
        assert_eq!(candidate.address, "203.0.113.22");
        assert_eq!(candidate.port, 45678);
        assert_eq!(candidate.rport, Some(local.port()));
        assert_eq!(candidate.raddr, Some("127.0.0.1".into()));
        assert_eq!(socket.local_addr().unwrap(), local);
        task.await.unwrap();
    }
    #[test]
    fn malformed_and_ambiguous_stun_envelopes_never_produce_candidates() {
        let transaction = [1; 12];
        let valid = ice::build_binding_response(
            &transaction,
            "203.0.113.2:4000".parse().unwrap(),
            Some(b"fixture"),
        );
        assert!(mapped(&valid, &transaction).is_some());
        for size in 0..valid.len() {
            assert!(mapped(&valid[..size], &transaction).is_none());
        }
        let mut duplicate = valid.clone();
        duplicate.extend_from_slice(&valid[20..32]);
        let len = duplicate.len() - 20;
        duplicate[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        assert!(mapped(&duplicate, &transaction).is_none());
        let mut tampered = valid.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(mapped(&tampered, &transaction).is_none());
        let zero = ice::build_binding_response(&transaction, "0.0.0.0:1".parse().unwrap(), None);
        assert!(mapped(&zero, &transaction).is_none());
    }
    #[tokio::test]
    async fn cancelled_gather_has_no_task_retaining_the_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = socket.local_addr().unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                gather(&socket, &[address], local.ip())
            )
            .await
            .is_err()
        );
        drop(socket);
        let _rebound = UdpSocket::bind(local).await.unwrap();
    }
}
