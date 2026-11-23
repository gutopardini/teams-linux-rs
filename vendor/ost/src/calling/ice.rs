//! ICE implementation — candidate parsing, STUN binding, connectivity checks.
//!
//! Implements enough of RFC 5389 (STUN) and RFC 8445 (ICE) for direct UDP
//! connectivity with the remote peer:
//! 1. Parse ICE candidates from remote SDP offer
//! 2. Build/parse STUN Binding Request/Response with full attributes
//! 3. Perform ICE connectivity checks with MESSAGE-INTEGRITY + FINGERPRINT
//! 4. Gather local host and server-reflexive candidates
//! 5. IceAgent orchestrates the check workflow

use super::datagram::DatagramIo;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use tokio::net::UdpSocket;

type HmacSha1 = Hmac<Sha1>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// STUN magic cookie (RFC 5389).
const MAGIC_COOKIE: u32 = 0x2112A442;

/// STUN message types.
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const BINDING_ERROR_RESPONSE: u16 = 0x0111;

/// STUN attribute types.
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;
const ATTR_PRIORITY: u16 = 0x0024;
const ATTR_USE_CANDIDATE: u16 = 0x0025;
const ATTR_ICE_CONTROLLED: u16 = 0x8029;
const ATTR_ICE_CONTROLLING: u16 = 0x802A;

/// STUN header size (type + length + magic + transaction ID).
const STUN_HEADER_SIZE: usize = 20;

/// FINGERPRINT XOR constant per RFC 5389.
const FINGERPRINT_XOR: u32 = 0x5354554e;

/// Default public STUN server for server-reflexive candidate gathering.
pub const DEFAULT_STUN_SERVER: &str = "stun.l.google.com:19302";

/// ICE connectivity check timeout per attempt.
const CHECK_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum retry count for connectivity checks.
const CHECK_MAX_RETRIES: u32 = 3;

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3) — needed for STUN FINGERPRINT attribute.
// Implemented inline to avoid adding a dependency for 50 lines of code.
// ---------------------------------------------------------------------------

/// CRC-32 lookup table (IEEE polynomial 0xEDB88320, reflected).
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFFFFFFu32;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc ^ 0xFFFFFFFF
}

// ---------------------------------------------------------------------------
// ICE candidate types (unchanged from original)
// ---------------------------------------------------------------------------

/// ICE candidate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateType {
    Host,
    ServerReflexive,
    Relay,
}

/// ICE transport protocol.
#[derive(Debug, Clone, PartialEq)]
pub enum Transport {
    Udp,
    TcpActive,
    TcpPassive,
}

/// Parsed ICE candidate from SDP.
#[derive(Debug, Clone)]
pub struct IceCandidate {
    pub foundation: String,
    pub component: u8,
    pub transport: Transport,
    pub priority: u32,
    pub address: String,
    pub port: u16,
    pub candidate_type: CandidateType,
    /// For srflx/relay: the related address.
    pub raddr: Option<String>,
    pub rport: Option<u16>,
}

impl IceCandidate {
    /// Format this candidate as an SDP `a=candidate:` line (without the `a=` prefix).
    pub fn to_sdp_line(&self) -> String {
        let transport_str = match self.transport {
            Transport::Udp => "UDP",
            Transport::TcpActive => "TCP-ACT",
            Transport::TcpPassive => "TCP-PASS",
        };
        let type_str = match self.candidate_type {
            CandidateType::Host => "host",
            CandidateType::ServerReflexive => "srflx",
            CandidateType::Relay => "relay",
        };
        let mut line = format!(
            "candidate:{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            transport_str,
            self.priority,
            self.address,
            self.port,
            type_str
        );
        if let (Some(ref ra), Some(rp)) = (&self.raddr, self.rport) {
            line.push_str(&format!(" raddr {} rport {}", ra, rp));
        }
        line
    }
}

/// ICE credentials (ufrag + pwd). Never print the ICE generation or password.
#[derive(Clone)]
pub struct IceCredentials {
    pub ufrag: String,
    pub pwd: String,
}

impl std::fmt::Debug for IceCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IceCredentials([REDACTED])")
    }
}

// ---------------------------------------------------------------------------
// Candidate parsing (unchanged from original)
// ---------------------------------------------------------------------------

/// Parse an `a=candidate:` line from SDP into an IceCandidate.
pub fn parse_candidate(line: &str) -> Result<IceCandidate> {
    let line = line.trim();
    let content = if line.starts_with("a=candidate:") {
        &line["a=candidate:".len()..]
    } else if line.starts_with("candidate:") {
        &line["candidate:".len()..]
    } else {
        bail!("not a candidate line: {}", line);
    };

    let parts: Vec<&str> = content.split_whitespace().collect();
    if parts.len() < 7 {
        bail!("candidate line too short: {}", line);
    }

    let foundation = parts[0].to_string();
    let component: u8 = parts[1].parse().context("bad component")?;

    let transport = match parts[2].to_uppercase().as_str() {
        "UDP" => Transport::Udp,
        "TCP-ACT" => Transport::TcpActive,
        "TCP-PASS" => Transport::TcpPassive,
        other => bail!("unsupported transport: {}", other),
    };

    let priority: u32 = parts[3].parse().context("bad priority")?;
    let address = parts[4].to_string();
    let port: u16 = parts[5].parse().context("bad port")?;

    if parts[6] != "typ" {
        bail!("expected 'typ' keyword at position 6, got: {}", parts[6]);
    }

    let candidate_type = if parts.len() > 7 {
        match parts[7] {
            "host" => CandidateType::Host,
            "srflx" => CandidateType::ServerReflexive,
            "relay" => CandidateType::Relay,
            other => bail!("unknown candidate type: {}", other),
        }
    } else {
        bail!("missing candidate type");
    };

    let mut raddr = None;
    let mut rport = None;
    let mut i = 8;
    while i < parts.len() {
        match parts[i] {
            "raddr" if i + 1 < parts.len() => {
                raddr = Some(parts[i + 1].to_string());
                i += 2;
            }
            "rport" if i + 1 < parts.len() => {
                rport = Some(parts[i + 1].parse().context("bad rport")?);
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    Ok(IceCandidate {
        foundation,
        component,
        transport,
        priority,
        address,
        port,
        candidate_type,
        raddr,
        rport,
    })
}

/// Parse all ICE candidates from an SDP string (audio section only).
pub fn parse_candidates_from_sdp(sdp: &str) -> Vec<IceCandidate> {
    parse_candidates_from_sdp_section(sdp, "audio")
}

/// Parse all ICE candidates from a specific SDP media section.
pub fn parse_candidates_from_sdp_section(sdp: &str, section_type: &str) -> Vec<IceCandidate> {
    let mut candidates = Vec::new();
    let mut in_target = false;
    let prefix = format!("m={}", section_type);

    for line in sdp.lines() {
        let line = line.trim();
        if line.starts_with(&prefix) {
            in_target = true;
        } else if line.starts_with("m=") {
            in_target = false;
        }

        if in_target && line.starts_with("a=candidate:") {
            if let Ok(c) = parse_candidate(line) {
                candidates.push(c);
            }
        }
    }

    candidates
}

/// Select the best remote candidate for direct UDP connectivity (no STUN check).
pub fn select_remote_candidate(candidates: &[IceCandidate]) -> Option<SocketAddr> {
    let mut udp_rtp: Vec<&IceCandidate> = candidates
        .iter()
        .filter(|c| c.transport == Transport::Udp && c.component == 1)
        .collect();

    udp_rtp.sort_by(|a, b| b.priority.cmp(&a.priority));

    for candidate in udp_rtp {
        if let Ok(addr) = format!("{}:{}", candidate.address, candidate.port).parse() {
            return Some(addr);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// STUN message building and parsing (RFC 5389)
// ---------------------------------------------------------------------------

/// Generate a random 12-byte STUN transaction ID.
pub fn generate_transaction_id() -> [u8; 12] {
    let id1 = uuid::Uuid::new_v4();
    let id2 = uuid::Uuid::new_v4();
    let b1 = id1.as_bytes();
    let b2 = id2.as_bytes();
    let mut txn = [0u8; 12];
    txn[..8].copy_from_slice(&b1[..8]);
    txn[8..12].copy_from_slice(&b2[..4]);
    txn
}

/// Build a minimal STUN Binding Request (header only, no attributes).
pub fn build_stun_binding_request(transaction_id: &[u8; 12]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(STUN_HEADER_SIZE);
    buf.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // length = 0
    buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    buf.extend_from_slice(transaction_id);
    buf
}

/// Build a STUN Binding Request with USERNAME, MESSAGE-INTEGRITY, and FINGERPRINT
/// for ICE connectivity checks.
///
/// `username` is `{remote_ufrag}:{local_ufrag}`.
/// `key` is the remote ICE password (used as HMAC-SHA1 key for MESSAGE-INTEGRITY).
/// `priority` is the local candidate priority to advertise.
/// `controlling` indicates whether we are the controlling agent.
/// `tie_breaker` is a 64-bit random value for tie-breaking.
pub fn build_ice_binding_request(
    transaction_id: &[u8; 12],
    username: &str,
    key: &[u8],
    priority: u32,
    controlling: bool,
    tie_breaker: u64,
) -> Vec<u8> {
    // Start with header (length placeholder)
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // length placeholder
    buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    buf.extend_from_slice(transaction_id);

    // USERNAME attribute
    append_stun_attr_string(&mut buf, ATTR_USERNAME, username);

    // PRIORITY attribute (4 bytes)
    append_stun_attr(&mut buf, ATTR_PRIORITY, &priority.to_be_bytes());

    // ICE-CONTROLLING or ICE-CONTROLLED (8 bytes)
    if controlling {
        append_stun_attr(&mut buf, ATTR_ICE_CONTROLLING, &tie_breaker.to_be_bytes());
        // USE-CANDIDATE: nominate this pair (aggressive nomination)
        append_stun_attr(&mut buf, ATTR_USE_CANDIDATE, &[]);
    } else {
        append_stun_attr(&mut buf, ATTR_ICE_CONTROLLED, &tie_breaker.to_be_bytes());
    }

    // MESSAGE-INTEGRITY: HMAC-SHA1 over the message up to (but not including) this attribute.
    // Per RFC 5389: the length field in the header must include the MESSAGE-INTEGRITY
    // attribute (type 2 + length 2 + value 20 = 24 bytes).
    let mi_offset = buf.len();
    let mi_length = (mi_offset - STUN_HEADER_SIZE + 24) as u16;
    buf[2..4].copy_from_slice(&mi_length.to_be_bytes());

    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC key length is valid");
    mac.update(&buf);
    let hmac_result = mac.finalize().into_bytes();
    append_stun_attr(&mut buf, ATTR_MESSAGE_INTEGRITY, &hmac_result[..20]);

    // FINGERPRINT: CRC-32 of everything up to (but not including) this attribute,
    // XOR'd with 0x5354554e. The header length must include the FINGERPRINT attr (8 bytes).
    let fp_offset = buf.len();
    let fp_length = (fp_offset - STUN_HEADER_SIZE + 8) as u16;
    buf[2..4].copy_from_slice(&fp_length.to_be_bytes());

    let crc = crc32(&buf);
    let fingerprint = crc ^ FINGERPRINT_XOR;
    append_stun_attr(&mut buf, ATTR_FINGERPRINT, &fingerprint.to_be_bytes());

    buf
}

/// Build a STUN Binding Success Response with XOR-MAPPED-ADDRESS.
pub fn build_binding_response(
    transaction_id: &[u8; 12],
    mapped_addr: SocketAddr,
    key: Option<&[u8]>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // length placeholder
    buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    buf.extend_from_slice(transaction_id);

    // XOR-MAPPED-ADDRESS
    let xma = encode_xor_mapped_address(mapped_addr, transaction_id);
    append_stun_attr(&mut buf, ATTR_XOR_MAPPED_ADDRESS, &xma);

    if let Some(key) = key {
        // MESSAGE-INTEGRITY
        let mi_offset = buf.len();
        let mi_length = (mi_offset - STUN_HEADER_SIZE + 24) as u16;
        buf[2..4].copy_from_slice(&mi_length.to_be_bytes());

        let mut mac = HmacSha1::new_from_slice(key).expect("HMAC key length is valid");
        mac.update(&buf);
        let hmac_result = mac.finalize().into_bytes();
        append_stun_attr(&mut buf, ATTR_MESSAGE_INTEGRITY, &hmac_result[..20]);

        // FINGERPRINT
        let fp_offset = buf.len();
        let fp_length = (fp_offset - STUN_HEADER_SIZE + 8) as u16;
        buf[2..4].copy_from_slice(&fp_length.to_be_bytes());

        let crc = crc32(&buf);
        let fingerprint = crc ^ FINGERPRINT_XOR;
        append_stun_attr(&mut buf, ATTR_FINGERPRINT, &fingerprint.to_be_bytes());
    } else {
        // Update length without integrity/fingerprint
        let attr_len = (buf.len() - STUN_HEADER_SIZE) as u16;
        buf[2..4].copy_from_slice(&attr_len.to_be_bytes());
    }

    buf
}

/// Check if a received UDP packet is a STUN message (any type).
pub fn is_stun_message(data: &[u8]) -> bool {
    if data.len() < STUN_HEADER_SIZE {
        return false;
    }
    // First two bits must be 0, magic cookie must match.
    let first_byte = data[0];
    if first_byte & 0xC0 != 0 {
        return false;
    }
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    magic == MAGIC_COOKIE
}

/// Check if a received UDP packet is a STUN Binding Success Response.
pub fn is_stun_response(data: &[u8]) -> bool {
    if data.len() < STUN_HEADER_SIZE {
        return false;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    msg_type == BINDING_RESPONSE && magic == MAGIC_COOKIE
}

/// Check if a received UDP packet is a STUN Binding Request.
pub fn is_stun_request(data: &[u8]) -> bool {
    if data.len() < STUN_HEADER_SIZE {
        return false;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    msg_type == BINDING_REQUEST && magic == MAGIC_COOKIE
}

/// Extract the transaction ID from a STUN message.
pub fn get_transaction_id(data: &[u8]) -> Option<[u8; 12]> {
    if data.len() < STUN_HEADER_SIZE {
        return None;
    }
    let mut txn = [0u8; 12];
    txn.copy_from_slice(&data[8..20]);
    Some(txn)
}

/// Parse a STUN Binding Response and extract the XOR-MAPPED-ADDRESS.
pub fn parse_binding_response(data: &[u8]) -> Option<SocketAddr> {
    if data.len() < STUN_HEADER_SIZE {
        return None;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    if msg_type != BINDING_RESPONSE {
        return None;
    }
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if magic != MAGIC_COOKIE {
        return None;
    }

    let txn_id = &data[8..20];
    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let attrs_end = std::cmp::min(STUN_HEADER_SIZE + msg_len, data.len());

    let mut pos = STUN_HEADER_SIZE;
    while pos + 4 <= attrs_end {
        let attr_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let attr_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        let attr_start = pos + 4;
        let attr_end = attr_start + attr_len;

        if attr_end > attrs_end {
            break;
        }

        if attr_type == ATTR_XOR_MAPPED_ADDRESS {
            return decode_xor_mapped_address(&data[attr_start..attr_end], txn_id);
        }

        // Also handle MAPPED-ADDRESS as fallback
        if attr_type == ATTR_MAPPED_ADDRESS {
            return decode_mapped_address(&data[attr_start..attr_end]);
        }

        // Advance to next attribute (padded to 4-byte boundary)
        pos = attr_start + ((attr_len + 3) & !3);
    }

    None
}

/// Verify MESSAGE-INTEGRITY of a received STUN message.
pub fn verify_message_integrity(data: &[u8], key: &[u8]) -> bool {
    let Some((position, received_hmac)) = integrity_attribute(data) else {
        return false;
    };
    let mut checked = data[..position].to_vec();
    let adjusted_len = (position + 24 - STUN_HEADER_SIZE) as u16;
    checked[2..4].copy_from_slice(&adjusted_len.to_be_bytes());
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts arbitrary key length");
    mac.update(&checked);
    // Use the MAC implementation's constant-time comparison.
    mac.verify_slice(received_hmac).is_ok()
}

/// Locate MI in a complete UDP STUN datagram. Attributes after MI must never
/// supply values interpreted as authenticated. Only a final fingerprint is
/// permitted there; malformed/truncated/duplicate attributes are rejected.
fn integrity_attribute(data: &[u8]) -> Option<(usize, &[u8])> {
    if !is_stun_message(data) {
        return None;
    }
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    if length % 4 != 0 || STUN_HEADER_SIZE + length != data.len() {
        return None;
    }
    let mut position = STUN_HEADER_SIZE;
    let mut integrity = None;
    while position < data.len() {
        if position + 4 > data.len() {
            return None;
        }
        let kind = u16::from_be_bytes([data[position], data[position + 1]]);
        let length = u16::from_be_bytes([data[position + 2], data[position + 3]]) as usize;
        let start = position + 4;
        let end = start.checked_add(length)?;
        let padded = start.checked_add((length + 3) & !3)?;
        if padded > data.len() {
            return None;
        }
        if kind == ATTR_MESSAGE_INTEGRITY {
            if length != 20 || integrity.is_some() {
                return None;
            }
            integrity = Some((position, &data[start..end]));
        } else if integrity.is_some() && kind != ATTR_FINGERPRINT {
            return None;
        }
        if kind == ATTR_FINGERPRINT {
            if length != 4 || padded != data.len() {
                return None;
            }
            let expected = crc32(&data[..position]) ^ FINGERPRINT_XOR;
            if data[start..end] != expected.to_be_bytes() {
                return None;
            }
        }
        position = padded;
    }
    integrity
}

fn attribute_before_integrity(data: &[u8], wanted: u16) -> Option<&[u8]> {
    let (end, _) = integrity_attribute(data)?;
    let mut position = STUN_HEADER_SIZE;
    let mut found = None;
    while position < end {
        let kind = u16::from_be_bytes([data[position], data[position + 1]]);
        let length = u16::from_be_bytes([data[position + 2], data[position + 3]]) as usize;
        let start = position + 4;
        if kind == wanted {
            if found.is_some() {
                return None;
            }
            found = Some(&data[start..start + length]);
        }
        position = start + ((length + 3) & !3);
    }
    found
}

// ---------------------------------------------------------------------------
// STUN attribute helpers
// ---------------------------------------------------------------------------

/// Append a STUN attribute with raw value bytes (handles 4-byte padding).
fn append_stun_attr(buf: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    buf.extend_from_slice(&attr_type.to_be_bytes());
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
    // Pad to 4-byte boundary
    let pad = (4 - (value.len() % 4)) % 4;
    for _ in 0..pad {
        buf.push(0);
    }
}

/// Append a STUN attribute with a UTF-8 string value.
fn append_stun_attr_string(buf: &mut Vec<u8>, attr_type: u16, value: &str) {
    append_stun_attr(buf, attr_type, value.as_bytes());
}

/// Encode a SocketAddr as XOR-MAPPED-ADDRESS value bytes.
fn encode_xor_mapped_address(addr: SocketAddr, transaction_id: &[u8; 12]) -> Vec<u8> {
    let mut val = Vec::new();
    val.push(0); // reserved
    match addr.ip() {
        IpAddr::V4(ip) => {
            val.push(0x01); // family: IPv4
            let xport = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
            val.extend_from_slice(&xport.to_be_bytes());
            let ip_bytes = ip.octets();
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 {
                val.push(ip_bytes[i] ^ cookie_bytes[i]);
            }
        }
        IpAddr::V6(ip) => {
            val.push(0x02); // family: IPv6
            let xport = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
            val.extend_from_slice(&xport.to_be_bytes());
            let ip_bytes = ip.octets();
            let mut xor_key = [0u8; 16];
            xor_key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            xor_key[4..16].copy_from_slice(transaction_id);
            for i in 0..16 {
                val.push(ip_bytes[i] ^ xor_key[i]);
            }
        }
    }
    val
}

/// Decode XOR-MAPPED-ADDRESS attribute value.
fn decode_xor_mapped_address(value: &[u8], transaction_id: &[u8]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let xport = u16::from_be_bytes([value[2], value[3]]);
    let port = xport ^ (MAGIC_COOKIE >> 16) as u16;

    match family {
        0x01 if value.len() >= 8 => {
            // IPv4
            let cookie = MAGIC_COOKIE.to_be_bytes();
            let ip = Ipv4Addr::new(
                value[4] ^ cookie[0],
                value[5] ^ cookie[1],
                value[6] ^ cookie[2],
                value[7] ^ cookie[3],
            );
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 if value.len() >= 20 => {
            // IPv6
            let mut xor_key = [0u8; 16];
            xor_key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            if transaction_id.len() >= 12 {
                xor_key[4..16].copy_from_slice(&transaction_id[..12]);
            }
            let mut octets = [0u8; 16];
            for i in 0..16 {
                octets[i] = value[4 + i] ^ xor_key[i];
            }
            let ip = std::net::Ipv6Addr::from(octets);
            Some(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => None,
    }
}

/// Decode MAPPED-ADDRESS attribute value (no XOR).
fn decode_mapped_address(value: &[u8]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]);

    match family {
        0x01 if value.len() >= 8 => {
            let ip = Ipv4Addr::new(value[4], value[5], value[6], value[7]);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ICE candidate gathering
// ---------------------------------------------------------------------------

/// Gather host candidates from the local socket's bound address.
///
/// Returns candidates for all local non-loopback IPv4 addresses if bound to 0.0.0.0,
/// otherwise just the bound address.
pub fn gather_host_candidates(local_addr: SocketAddr) -> Vec<IceCandidate> {
    let mut candidates = Vec::new();
    let port = local_addr.port();

    if local_addr.ip().is_unspecified() {
        // Bound to 0.0.0.0 — enumerate local interfaces.
        // Use the "connect to 8.8.8.8" trick to find the default outbound IP.
        if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
            if socket.connect("8.8.8.8:80").is_ok() {
                if let Ok(addr) = socket.local_addr() {
                    let ip = addr.ip().to_string();
                    candidates.push(IceCandidate {
                        foundation: "1".into(),
                        component: 1,
                        transport: Transport::Udp,
                        priority: compute_priority(CandidateType::Host, 1, 1),
                        address: ip,
                        port,
                        candidate_type: CandidateType::Host,
                        raddr: None,
                        rport: None,
                    });
                }
            }
        }
    } else {
        candidates.push(IceCandidate {
            foundation: "1".into(),
            component: 1,
            transport: Transport::Udp,
            priority: compute_priority(CandidateType::Host, 1, 1),
            address: local_addr.ip().to_string(),
            port,
            candidate_type: CandidateType::Host,
            raddr: None,
            rport: None,
        });
    }

    candidates
}

/// Gather a server-reflexive candidate by sending a STUN Binding Request to a public
/// STUN server. Returns None if the STUN server is unreachable.
pub async fn gather_srflx_candidate(socket: &UdpSocket, stun_server: &str) -> Option<IceCandidate> {
    // Resolve STUN server address
    let server_addr: SocketAddr = match tokio::net::lookup_host(stun_server).await {
        Ok(mut addrs) => addrs.next()?,
        Err(e) => {
            tracing::debug!("Failed to resolve STUN server {}: {}", stun_server, e);
            return None;
        }
    };

    let txn_id = generate_transaction_id();
    let request = build_stun_binding_request(&txn_id);

    // Send request and wait for response
    for attempt in 0..2 {
        if let Err(e) = socket.send_to(&request, server_addr).await {
            tracing::debug!("STUN send to {} failed: {}", server_addr, e);
            return None;
        }

        let mut buf = [0u8; 256];
        match tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _from))) => {
                let data = &buf[..len];
                if is_stun_response(data) {
                    // Verify transaction ID matches
                    if let Some(resp_txn) = get_transaction_id(data) {
                        if resp_txn == txn_id {
                            if let Some(mapped_addr) = parse_binding_response(data) {
                                let local_addr = socket.local_addr().ok()?;
                                return Some(IceCandidate {
                                    foundation: "2".into(),
                                    component: 1,
                                    transport: Transport::Udp,
                                    priority: compute_priority(
                                        CandidateType::ServerReflexive,
                                        1,
                                        1,
                                    ),
                                    address: mapped_addr.ip().to_string(),
                                    port: mapped_addr.port(),
                                    candidate_type: CandidateType::ServerReflexive,
                                    raddr: Some(local_addr.ip().to_string()),
                                    rport: Some(local_addr.port()),
                                });
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::debug!("STUN recv error (attempt {}): {}", attempt, e);
            }
            Err(_) => {
                tracing::debug!("STUN timeout (attempt {})", attempt);
            }
        }
    }

    None
}

/// Compute ICE candidate priority per RFC 8445 section 5.1.2.1.
fn compute_priority(ctype: CandidateType, local_preference: u16, component: u8) -> u32 {
    let type_preference: u32 = match ctype {
        CandidateType::Host => 126,
        CandidateType::ServerReflexive => 100,
        CandidateType::Relay => 0,
    };
    (type_preference << 24) | ((local_preference as u32) << 8) | (256 - component as u32)
}

// ---------------------------------------------------------------------------
// ICE connectivity checks
// ---------------------------------------------------------------------------

/// Perform a single STUN connectivity check against a remote candidate.
///
/// Sends a STUN Binding Request with USERNAME, MESSAGE-INTEGRITY, and FINGERPRINT.
/// Returns the XOR-MAPPED-ADDRESS from the response (our address as seen by the peer).
pub async fn check_candidate(
    socket: &UdpSocket,
    candidate_addr: SocketAddr,
    local_ufrag: &str,
    remote_ufrag: &str,
    remote_pwd: &str,
    local_priority: u32,
    controlling: bool,
) -> Result<SocketAddr> {
    check_candidate_authenticated(
        socket,
        candidate_addr,
        local_ufrag,
        remote_ufrag,
        remote_pwd,
        local_priority,
        controlling,
        None,
    )
    .await
}

async fn check_candidate_authenticated<D:DatagramIo>(
    socket: &D,
    candidate_addr: SocketAddr,
    local_ufrag: &str,
    remote_ufrag: &str,
    remote_pwd: &str,
    local_priority: u32,
    controlling: bool,
    responder: Option<&IceAgent>,
) -> Result<SocketAddr> {
    let username = format!("{}:{}", remote_ufrag, local_ufrag);
    let tie_breaker = {
        let id = uuid::Uuid::new_v4();
        let b = id.as_bytes();
        u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    };

    for attempt in 0..CHECK_MAX_RETRIES {
        let txn_id = generate_transaction_id();
        let request = build_ice_binding_request(
            &txn_id,
            &username,
            remote_pwd.as_bytes(),
            local_priority,
            controlling,
            tie_breaker,
        );

        socket
            .send_to(&request, candidate_addr)
            .await
            .with_context(|| format!("STUN send to {} failed", candidate_addr))?;

        tracing::debug!(
            "ICE check #{} sent to {} (txn {:02x}{:02x}{:02x}{:02x}...)",
            attempt + 1,
            candidate_addr,
            txn_id[0],
            txn_id[1],
            txn_id[2],
            txn_id[3]
        );

        // Wait for response on the socket
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + CHECK_TIMEOUT;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Ok(Ok((len, from))) => {
                    let data = &buf[..len];

                    if let Some(mapped) = verified_binding_response(
                        data,
                        from,
                        candidate_addr,
                        txn_id,
                        remote_pwd.as_bytes(),
                    ) {
                        return Ok(mapped);
                    }
                    if let Some(agent) = responder {
                        agent.handle_stun_request(socket, data, from).await;
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!("recv error during ICE check: {}", e);
                    break;
                }
                Err(_) => {
                    // Timeout
                    break;
                }
            }
        }

        tracing::debug!("ICE check #{} to {} timed out", attempt + 1, candidate_addr);
    }

    bail!(
        "ICE connectivity check failed after {} attempts to {}",
        CHECK_MAX_RETRIES,
        candidate_addr
    )
}

fn verified_binding_response(
    data: &[u8],
    source: SocketAddr,
    candidate: SocketAddr,
    transaction: [u8; 12],
    key: &[u8],
) -> Option<SocketAddr> {
    if source != candidate
        || !is_stun_response(data)
        || get_transaction_id(data) != Some(transaction)
        || !verify_message_integrity(data, key)
    {
        return None;
    }
    parse_binding_response(data)
}

// ---------------------------------------------------------------------------
// ICE Agent
// ---------------------------------------------------------------------------

/// Result of ICE connectivity checks.
#[derive(Debug, Clone)]
pub struct IceCheckResult {
    /// The verified remote address to send media to.
    pub remote_addr: SocketAddr,
    /// Our address as seen by the remote peer (from XOR-MAPPED-ADDRESS).
    pub mapped_addr: SocketAddr,
    /// Candidate category that authenticated the connectivity check.
    pub candidate_type: CandidateType,
}

/// ICE agent that performs connectivity checks and handles incoming STUN requests.
pub struct IceAgent {
    /// Local ICE credentials.
    pub local_creds: IceCredentials,
    /// Remote ICE credentials.
    pub remote_creds: IceCredentials,
    /// Whether we are the controlling agent (answerer = controlled for incoming calls).
    pub controlling: bool,
}

impl IceAgent {
    /// Create a new ICE agent.
    pub fn new(
        local_creds: IceCredentials,
        remote_creds: IceCredentials,
        controlling: bool,
    ) -> Self {
        Self {
            local_creds,
            remote_creds,
            controlling,
        }
    }

    /// Run connectivity checks against remote candidates in priority order.
    ///
    /// Returns the first candidate with an authenticated response. One receive
    /// owner also handles authenticated peer checks; cancellation leaves no
    /// detached responder task holding the socket.
    pub async fn check_connectivity<D:DatagramIo>(
        &self,
        socket: Arc<D>,
        remote_candidates: &[IceCandidate],
    ) -> Result<IceCheckResult> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.check_connectivity_inner(socket, remote_candidates),
        )
        .await
        .context("ICE connectivity deadline exceeded")?
    }

    async fn check_connectivity_inner<D:DatagramIo>(
        &self,
        socket: Arc<D>,
        remote_candidates: &[IceCandidate],
    ) -> Result<IceCheckResult> {
        if remote_candidates.len() > 64 {
            bail!("Too many remote ICE candidates");
        }
        // Sort candidates by priority (highest first), filter to UDP component 1
        let mut candidates: Vec<&IceCandidate> = remote_candidates
            .iter()
            .filter(|c| c.transport == Transport::Udp && c.component == 1)
            .collect();
        candidates.sort_by(|a, b| b.priority.cmp(&a.priority));

        if candidates.is_empty() {
            bail!("No UDP candidates to check");
        }

        tracing::info!(
            "Starting ICE connectivity checks ({} candidates)",
            candidates.len()
        );

        // One receive owner and paced checks avoid letting dead high-priority
        // host candidates consume the entire deadline before a reachable relay.
        struct Probe { address:SocketAddr, candidate_type:CandidateType, transaction:[u8;12], request:Vec<u8>, attempts:u32, next:tokio::time::Instant }
        let now=tokio::time::Instant::now();
        let username=format!("{}:{}",self.remote_creds.ufrag,self.local_creds.ufrag);
        let tie_breaker=u64::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap());
        let mut probes=Vec::new();
        let local_priority=compute_priority(CandidateType::Host,1,1);
        for candidate in candidates {
            let Ok(ip)=candidate.address.parse::<std::net::IpAddr>()else{continue;};
            if !ip.is_ipv4() || candidate.port==0 || ip.is_unspecified() || ip.is_multicast(){continue;}
            let address=SocketAddr::new(ip,candidate.port);
            if probes.iter().any(|p:&Probe|p.address==address){continue;}
            let transaction=generate_transaction_id();
            let request=build_ice_binding_request(&transaction,&username,self.remote_creds.pwd.as_bytes(),local_priority,self.controlling,tie_breaker);
            let next=now+Duration::from_millis(50*probes.len() as u64);
            probes.push(Probe{address,candidate_type:candidate.candidate_type,transaction,request,attempts:0,next});
        }
        if probes.is_empty(){bail!("No supported UDP candidate addresses");}
        let mut buffer=[0u8;2048];
        loop {
            let now=tokio::time::Instant::now();
            for probe in &mut probes {
                if probe.attempts<CHECK_MAX_RETRIES && now>=probe.next {
                    // Retransmissions retain the STUN transaction identity.
                    if socket.send_to(&probe.request,probe.address).await.is_err(){probe.attempts=CHECK_MAX_RETRIES;continue;}
                    probe.attempts+=1;
                    probe.next=now+CHECK_TIMEOUT*(1u32<<(probe.attempts-1));
                }
            }
            let next=probes.iter().filter(|p|p.attempts<CHECK_MAX_RETRIES || now<p.next).map(|p|p.next).min();
            let Some(next)=next else{bail!("ICE candidates exhausted without an authenticated response");};
            tokio::select! {biased;
                // Check timers before draining more untrusted UDP packets.
                _=tokio::time::sleep_until(next)=>{},
                packet=socket.recv_from(&mut buffer)=>{
                    let (size,source)=packet.context("ICE socket receive failed")?;
                    let data=&buffer[..size];
                    if let Some(probe)=probes.iter().find(|p|p.address==source && p.attempts>0) {
                        if let Some(mapped)=verified_binding_response(data,source,probe.address,probe.transaction,self.remote_creds.pwd.as_bytes()) {
                            return Ok(IceCheckResult{remote_addr:source,mapped_addr:mapped,candidate_type:probe.candidate_type});
                        }
                    }
                    self.handle_stun_request(&*socket,data,source).await;
                }
            }
        }
    }

    /// Returns the source address if it was a valid STUN request.
    pub async fn handle_stun_request<D:DatagramIo>(
        &self,
        socket: &D,
        data: &[u8],
        from: SocketAddr,
    ) -> Option<SocketAddr> {
        if !is_stun_request(data) {
            return None;
        }

        let txn_id = get_transaction_id(data)?;

        // Both integrity and the username must belong to this ICE generation.
        if !verify_message_integrity(data, self.local_creds.pwd.as_bytes()) {
            return None;
        }
        let username = format!("{}:{}", self.local_creds.ufrag, self.remote_creds.ufrag);
        if attribute_before_integrity(data, ATTR_USERNAME) != Some(username.as_bytes()) {
            return None;
        }

        // Build and send response
        let response = build_binding_response(&txn_id, from, Some(self.local_creds.pwd.as_bytes()));

        if let Err(e) = socket.send_to(&response, from).await {
            tracing::debug!("Failed to send STUN response to {}: {}", from, e);
        } else {
            tracing::debug!("Sent STUN binding response to {} (mapped: {})", from, from);
        }

        Some(from)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(name: &str) -> IceCredentials {
        IceCredentials {
            ufrag: name.into(),
            pwd: format!("fixture-password-{name}-123456"),
        }
    }

    fn host(address: SocketAddr) -> IceCandidate {
        IceCandidate {
            foundation: "fixture".into(),
            component: 1,
            transport: Transport::Udp,
            priority: 100,
            address: address.ip().to_string(),
            port: address.port(),
            candidate_type: CandidateType::Host,
            raddr: None,
            rport: None,
        }
    }

    #[tokio::test]
    async fn paced_checks_reach_a_working_candidate_before_dead_hosts_exhaust_deadline(){
        let local=Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer=UdpSocket::bind("127.0.0.1:0").await.unwrap();let peer_addr=peer.local_addr().unwrap();
        let mut silent=Vec::new();let mut candidates=Vec::new();
        for _ in 0..8 {let socket=UdpSocket::bind("127.0.0.1:0").await.unwrap();let mut candidate=host(socket.local_addr().unwrap());candidate.priority=200;candidates.push(candidate);silent.push(socket);}
        candidates.push(host(peer_addr));
        let responder=tokio::spawn(async move{let mut buffer=[0;2048];let(size,source)=peer.recv_from(&mut buffer).await.unwrap();let data=&buffer[..size];
            assert!(verify_message_integrity(data,credentials("remote").pwd.as_bytes()));
            let response=build_binding_response(&get_transaction_id(data).unwrap(),source,Some(credentials("remote").pwd.as_bytes()));peer.send_to(&response,source).await.unwrap();});
        let agent=IceAgent::new(credentials("local"),credentials("remote"),true);
        let start=std::time::Instant::now();
        let result=tokio::time::timeout(Duration::from_secs(2),agent.check_connectivity(local,&candidates)).await.unwrap().unwrap();
        assert_eq!(result.remote_addr,peer_addr);assert!(start.elapsed()<Duration::from_secs(2));responder.await.unwrap();
    }

    #[test]
    fn independent_rfc5769_packets_validate_integrity_padding_and_mapped_addresses() {
        // Data-only test vectors, RFC 5769 sections 2.1–2.3:
        // https://www.rfc-editor.org/rfc/rfc5769.html
        // Independent of the packet constructors being tested above.
        let vectors = [
            (
                concat!(
                    "000100582112a442b7e7a701bc34d686fa87dfae",
                    "802200105354554e207465737420636c69656e74",
                    "002400046e0001ff80290008932ff9b151263b36",
                    "000600096576746a3a68367659202020",
                    "000800149aeaa70cbfd8cb56781ef2b5b2d3f249c1b571a2",
                    "80280004e57a3bcf"
                ),
                None,
            ),
            (
                concat!(
                    "0101003c2112a442b7e7a701bc34d686fa87dfae",
                    "8022000b7465737420766563746f7220",
                    "002000080001a147e112a643",
                    "000800142b91f599fd9e90c38c7489f92af9ba53f06be7d7",
                    "80280004c07d4c96"
                ),
                Some("192.0.2.1:32853"),
            ),
            (
                concat!(
                    "010100482112a442b7e7a701bc34d686fa87dfae",
                    "8022000b7465737420766563746f7220",
                    "002000140002a1470113a9faa5d3f179bc25f4b5bed2b9d9",
                    "00080014a382954e4be67bf11784c97c8292c275bfe3ed41",
                    "80280004c8fb0b4c"
                ),
                Some("[2001:db8:1234:5678:11:2233:4455:6677]:32853"),
            ),
        ];
        for (hex, address) in vectors {
            let bytes = hex
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect::<Vec<_>>();
            assert!(verify_message_integrity(&bytes, b"VOkJxbRl1RmTxUk/WvJxBt"));
            assert!(!verify_message_integrity(&bytes, b"wrong-fixture-key"));
            if let Some(address) = address {
                assert_eq!(
                    parse_binding_response(&bytes),
                    Some(address.parse().unwrap())
                );
            }
        }
    }

    #[test]
    fn authenticated_response_requires_source_transaction_and_integrity() {
        let peer: SocketAddr = "127.0.0.1:3001".parse().unwrap();
        let mapped: SocketAddr = "127.0.0.1:4001".parse().unwrap();
        let transaction = [0x32; 12];
        let key = b"fixture-password";
        let valid = build_binding_response(&transaction, mapped, Some(key));
        assert_eq!(
            verified_binding_response(&valid, peer, peer, transaction, key),
            Some(mapped)
        );
        assert!(verified_binding_response(
            &valid,
            "127.0.0.1:3002".parse().unwrap(),
            peer,
            transaction,
            key
        )
        .is_none());
        assert!(verified_binding_response(&valid, peer, peer, [0x33; 12], key).is_none());
        assert!(verified_binding_response(&valid, peer, peer, transaction, b"wrong-key").is_none());
        let unsigned = build_binding_response(&transaction, mapped, None);
        assert!(verified_binding_response(&unsigned, peer, peer, transaction, key).is_none());
    }

    #[test]
    fn integrity_rejects_truncation_bad_lengths_and_attributes_after_mac() {
        let key = b"fixture-password";
        let request = build_ice_binding_request(&[0x41; 12], "local:remote", key, 100, true, 20);
        assert!(verify_message_integrity(&request, key));
        for length in 0..request.len() {
            assert!(!verify_message_integrity(&request[..length], key));
        }
        let mut too_short = request.clone();
        too_short[2..4].copy_from_slice(&0u16.to_be_bytes());
        assert!(!verify_message_integrity(&too_short, key));
        let mut too_long = request.clone();
        too_long[2..4].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(!verify_message_integrity(&too_long, key));
        let mut injected = request[..request.len() - 8].to_vec();
        append_stun_attr_string(&mut injected, ATTR_USERNAME, "local:remote");
        let length = (injected.len() - 20) as u16;
        injected[2..4].copy_from_slice(&length.to_be_bytes());
        // The original HMAC prefix is unchanged. This must still be refused:
        // an authenticated parser cannot consume unauthenticated trailing attrs.
        assert!(!verify_message_integrity(&injected, key));
        let mut bad_fingerprint = request.clone();
        *bad_fingerprint.last_mut().unwrap() ^= 1;
        assert!(!verify_message_integrity(&bad_fingerprint, key));
    }

    #[tokio::test]
    async fn only_authenticated_current_generation_checks_receive_a_reply() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = credentials("local");
        let remote = credentials("remote");
        let agent = IceAgent::new(local.clone(), remote, false);
        for (username, key) in [
            ("local:remote", b"wrong".as_slice()),
            ("local:old-generation", local.pwd.as_bytes()),
        ] {
            let data = build_ice_binding_request(&[3; 12], username, key, 10, true, 99);
            assert!(agent
                .handle_stun_request(&socket, &data, peer.local_addr().unwrap())
                .await
                .is_none());
        }
        let mut buffer = [0u8; 512];
        assert!(
            tokio::time::timeout(Duration::from_millis(30), peer.recv_from(&mut buffer))
                .await
                .is_err()
        );
        let valid =
            build_ice_binding_request(&[4; 12], "local:remote", local.pwd.as_bytes(), 10, true, 99);
        assert!(agent
            .handle_stun_request(&socket, &valid, peer.local_addr().unwrap())
            .await
            .is_some());
        let (length, source) =
            tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(source, socket.local_addr().unwrap());
        assert!(verify_message_integrity(
            &buffer[..length],
            local.pwd.as_bytes()
        ));
        assert!(!format!("{local:?}").contains(&local.pwd));
        assert!(!format!("{local:?}").contains(&local.ufrag));
    }

    #[tokio::test]
    async fn simultaneous_peer_checks_share_one_receiver_without_lost_responses() {
        let left = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let left_addr = left.local_addr().unwrap();
        let right_addr = right.local_addr().unwrap();
        let left_agent = IceAgent::new(credentials("left"), credentials("right"), true);
        let right_agent = IceAgent::new(credentials("right"), credentials("left"), false);
        let left_candidates = [host(left_addr)];
        let right_candidates = [host(right_addr)];
        let (l, r) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                left_agent.check_connectivity(left.clone(), &right_candidates),
                right_agent.check_connectivity(right.clone(), &left_candidates)
            )
        })
        .await
        .unwrap();
        assert_eq!(l.unwrap().remote_addr, right_addr);
        assert_eq!(r.unwrap().remote_addr, left_addr);
        assert_eq!(left.local_addr().unwrap(), left_addr);
        assert_eq!(right.local_addr().unwrap(), right_addr);
    }

    #[tokio::test]
    async fn cancelling_checks_drops_the_last_socket_owner_without_a_background_responder() {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let address = socket.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let candidate = host(peer.local_addr().unwrap());
        let worker = tokio::spawn(async move {
            IceAgent::new(credentials("left"), credentials("right"), true)
                .check_connectivity(socket, &[candidate])
                .await
        });
        let mut buffer = [0u8; 512];
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let _rebound = UdpSocket::bind(address)
            .await
            .expect("cancelled ICE left a socket-owning task");
    }

    #[test]
    fn test_parse_host_candidate() {
        let line = "a=candidate:1 1 UDP 2130706431 10.0.0.1 21730 typ host";
        let c = parse_candidate(line).unwrap();
        assert_eq!(c.foundation, "1");
        assert_eq!(c.component, 1);
        assert_eq!(c.transport, Transport::Udp);
        assert_eq!(c.priority, 2130706431);
        assert_eq!(c.address, "10.0.0.1");
        assert_eq!(c.port, 21730);
        assert_eq!(c.candidate_type, CandidateType::Host);
        assert!(c.raddr.is_none());
    }

    #[test]
    fn test_parse_relay_candidate() {
        let line =
            "a=candidate:3 1 UDP 184548351 52.114.0.1 27882 typ relay raddr 10.0.0.1 rport 11632";
        let c = parse_candidate(line).unwrap();
        assert_eq!(c.candidate_type, CandidateType::Relay);
        assert_eq!(c.raddr.as_deref(), Some("10.0.0.1"));
        assert_eq!(c.rport, Some(11632));
    }

    #[test]
    fn test_parse_srflx_candidate() {
        let line =
            "a=candidate:6 1 UDP 1694234111 203.0.113.1 11632 typ srflx raddr 10.0.0.1 rport 11632";
        let c = parse_candidate(line).unwrap();
        assert_eq!(c.candidate_type, CandidateType::ServerReflexive);
    }

    #[test]
    fn test_select_best_candidate() {
        let candidates = vec![
            IceCandidate {
                foundation: "3".into(),
                component: 1,
                transport: Transport::Udp,
                priority: 184548351,
                address: "52.114.0.1".into(),
                port: 27882,
                candidate_type: CandidateType::Relay,
                raddr: Some("10.0.0.1".into()),
                rport: Some(11632),
            },
            IceCandidate {
                foundation: "1".into(),
                component: 1,
                transport: Transport::Udp,
                priority: 2130706431,
                address: "10.0.0.1".into(),
                port: 21730,
                candidate_type: CandidateType::Host,
                raddr: None,
                rport: None,
            },
        ];

        let selected = select_remote_candidate(&candidates).unwrap();
        assert_eq!(selected.to_string(), "10.0.0.1:21730");
    }

    #[test]
    fn test_parse_candidates_from_sdp() {
        let sdp = "\
v=0\r\n\
o=- 0 0 IN IP4 10.0.0.1\r\n\
s=session\r\n\
t=0 0\r\n\
m=audio 21730 RTP/SAVP 0\r\n\
a=candidate:1 1 UDP 2130706431 10.0.0.1 21730 typ host\r\n\
a=candidate:1 2 UDP 2130705918 10.0.0.1 21731 typ host\r\n\
a=candidate:3 1 UDP 184548351 52.0.0.1 27882 typ relay raddr 10.0.0.1 rport 11632\r\n\
m=video 14606 RTP/SAVP 122\r\n\
a=candidate:1 1 UDP 2130706431 10.0.0.1 14606 typ host\r\n";

        let candidates = parse_candidates_from_sdp(sdp);
        assert_eq!(candidates.len(), 3);
    }

    #[test]
    fn test_stun_binding_request() {
        let txn_id = [1u8; 12];
        let req = build_stun_binding_request(&txn_id);
        assert_eq!(req.len(), 20);
        assert_eq!(&req[0..2], &[0x00, 0x01]);
        assert_eq!(&req[4..8], &[0x21, 0x12, 0xA4, 0x42]);
    }

    #[test]
    fn test_is_stun_response() {
        let mut resp = vec![0u8; 20];
        resp[0] = 0x01;
        resp[1] = 0x01;
        resp[4] = 0x21;
        resp[5] = 0x12;
        resp[6] = 0xA4;
        resp[7] = 0x42;
        assert!(is_stun_response(&resp));
        assert!(!is_stun_response(&[0u8; 20]));
        assert!(!is_stun_response(&[0u8; 5]));
    }

    #[test]
    fn test_crc32_known_values() {
        // "123456789" should produce CRC32 = 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32(b""), 0x00000000);
    }

    #[test]
    fn test_xor_mapped_address_roundtrip() {
        let addr: SocketAddr = "192.168.1.100:12345".parse().unwrap();
        let txn_id = [0x01u8; 12];
        let encoded = encode_xor_mapped_address(addr, &txn_id);
        let decoded = decode_xor_mapped_address(&encoded, &txn_id).unwrap();
        assert_eq!(decoded, addr);
    }

    #[test]
    fn test_build_binding_response_parseable() {
        let txn_id = [0xABu8; 12];
        let addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let response = build_binding_response(&txn_id, addr, None);

        assert!(is_stun_response(&response));
        let parsed = parse_binding_response(&response).unwrap();
        assert_eq!(parsed, addr);
    }

    #[test]
    fn test_ice_binding_request_has_integrity_and_fingerprint() {
        let txn_id = [0x42u8; 12];
        let key = b"testpassword";
        let request = build_ice_binding_request(&txn_id, "remote:local", key, 100, true, 0xDEAD);

        // Should be a valid STUN message
        assert!(is_stun_message(&request));
        assert!(is_stun_request(&request));

        // Should have MESSAGE-INTEGRITY that verifies
        assert!(verify_message_integrity(&request, key));

        // Wrong key should fail
        assert!(!verify_message_integrity(&request, b"wrongkey"));
    }

    #[test]
    fn test_ice_binding_request_fingerprint() {
        let txn_id = [0x42u8; 12];
        let key = b"testpassword";
        let request = build_ice_binding_request(&txn_id, "remote:local", key, 100, true, 0xDEAD);

        // Find FINGERPRINT attribute (last 8 bytes: 2 type + 2 length + 4 value)
        let len = request.len();
        assert!(len >= 28); // at least header + some attrs + fingerprint
        let fp_type = u16::from_be_bytes([request[len - 8], request[len - 7]]);
        assert_eq!(fp_type, ATTR_FINGERPRINT);

        let fp_val = u32::from_be_bytes([
            request[len - 4],
            request[len - 3],
            request[len - 2],
            request[len - 1],
        ]);

        // Verify: CRC32 of everything before FINGERPRINT, XOR'd with magic
        let crc = crc32(&request[..len - 8]);
        assert_eq!(fp_val, crc ^ FINGERPRINT_XOR);
    }

    #[test]
    fn test_candidate_to_sdp_line() {
        let c = IceCandidate {
            foundation: "1".into(),
            component: 1,
            transport: Transport::Udp,
            priority: 2130706431,
            address: "10.0.0.1".into(),
            port: 21730,
            candidate_type: CandidateType::Host,
            raddr: None,
            rport: None,
        };
        let line = c.to_sdp_line();
        assert_eq!(line, "candidate:1 1 UDP 2130706431 10.0.0.1 21730 typ host");
    }

    #[test]
    fn test_candidate_to_sdp_line_srflx() {
        let c = IceCandidate {
            foundation: "2".into(),
            component: 1,
            transport: Transport::Udp,
            priority: 1694498815,
            address: "203.0.113.1".into(),
            port: 11632,
            candidate_type: CandidateType::ServerReflexive,
            raddr: Some("10.0.0.1".into()),
            rport: Some(21730),
        };
        let line = c.to_sdp_line();
        assert!(line.contains("typ srflx"));
        assert!(line.contains("raddr 10.0.0.1 rport 21730"));
    }

    #[test]
    fn test_gather_host_candidates() {
        let addr: SocketAddr = "192.168.1.100:5000".parse().unwrap();
        let candidates = gather_host_candidates(addr);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].address, "192.168.1.100");
        assert_eq!(candidates[0].port, 5000);
        assert_eq!(candidates[0].candidate_type, CandidateType::Host);
    }

    #[test]
    fn test_compute_priority() {
        let host = compute_priority(CandidateType::Host, 65535, 1);
        let srflx = compute_priority(CandidateType::ServerReflexive, 65535, 1);
        let relay = compute_priority(CandidateType::Relay, 65535, 1);
        // Host should have highest priority
        assert!(host > srflx);
        assert!(srflx > relay);
    }
}
