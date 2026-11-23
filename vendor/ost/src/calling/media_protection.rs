//! Own native receive/control boundary for AES_CM_128_HMAC_SHA1_80.
//! Authenticate before committing bounded per-SSRC replay state (RFC3711).
use super::{
    rtp,
    srtp::{SrtpContext, SrtpSessionKeys},
};
use aes::Aes128;
use anyhow::{bail, Result};
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::collections::BTreeMap;
type AesCtr = ctr::Ctr128BE<Aes128>;
const TAG: usize = 10;
const MAX_SOURCES: usize = 1024;
#[derive(Default, Clone, Copy)]
struct Window {
    highest: Option<u64>,
    seen: u128,
    closed: bool,
}
impl Window {
    fn accepts(&self, index: u64) -> bool {
        if self.closed {
            return false;
        }
        match self.highest {
            None => true,
            Some(high) if index > high => true,
            Some(high) => {
                let behind = high - index;
                behind < 128 && self.seen & (1u128 << behind) == 0
            }
        }
    }
    fn commit(&mut self, index: u64) {
        match self.highest {
            None => {
                self.highest = Some(index);
                self.seen = 1;
            }
            Some(high) if index > high => {
                let delta = index - high;
                self.seen = if delta >= 128 {
                    1
                } else {
                    (self.seen << delta) | 1
                };
                self.highest = Some(index);
            }
            Some(high) => self.seen |= 1u128 << (high - index),
        }
    }
}
fn iv(keys: &SrtpSessionKeys, ssrc: u32, index: u64) -> [u8; 16] {
    let mut iv = [0; 16];
    iv[..14].copy_from_slice(&keys.salt);
    for (output, byte) in iv[4..8].iter_mut().zip(ssrc.to_be_bytes()) {
        *output ^= byte;
    }
    for (output, byte) in iv[8..14].iter_mut().zip(&index.to_be_bytes()[2..]) {
        *output ^= byte;
    }
    iv
}
fn crypt(keys: &SrtpSessionKeys, ssrc: u32, index: u64, data: &mut [u8]) {
    AesCtr::new((&keys.cipher_key).into(), (&iv(keys, ssrc, index)).into()).apply_keystream(data);
}
fn verify(keys: &SrtpSessionKeys, data: &[u8], suffix: &[u8], tag: &[u8]) -> Result<()> {
    let mut mac = Hmac::<Sha1>::new_from_slice(&keys.auth_key)?;
    mac.update(data);
    mac.update(suffix);
    mac.verify_truncated_left(tag)
        .map_err(|_| anyhow::anyhow!("Media authentication failed"))
}
fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}
/// RTP receive contexts cannot share ROC/replay history across sources.
pub struct RtpReceiver {
    keys: SrtpSessionKeys,
    sources: BTreeMap<u32, Window>,
}
impl RtpReceiver {
    pub fn new(context: &SrtpContext) -> Self {
        Self {
            keys: context.remote_keys.clone(),
            sources: BTreeMap::new(),
        }
    }
    /// Authenticated BYE retires a known source without forgetting its replay
    /// state. Delayed RTP cannot resurrect queued speech after source exit.
    pub fn retire(&mut self, ssrc: u32) {
        if let Some(source) = self.sources.get_mut(&ssrc) {
            source.closed = true;
        }
    }
    pub fn unprotect(&mut self, data: &[u8]) -> Result<(Vec<u8>, u64)> {
        if data.len() < 22 || data.len() > 2048 || data[0] >> 6 != 2 {
            bail!("Invalid secure RTP");
        }
        let end = data.len() - TAG;
        let header = rtp::full_header_len(&data[..end])
            .ok_or_else(|| anyhow::anyhow!("Invalid secure RTP header"))?;
        let ssrc = u32_at(data, 8);
        let seq = u16::from_be_bytes([data[2], data[3]]);
        let window = self.sources.get(&ssrc).copied().unwrap_or_default();
        let index = match window.highest {
            None => seq as u64,
            Some(high) => {
                let highest = high as u16 as u32;
                let sequence = seq as u32;
                let roc = high >> 16;
                // RFC3711§3.3.1 uses strict comparisons at half the sequence
                // space. A signed16 delta mishandles the exact32768 boundary.
                let roc = if highest < 32768 && sequence > highest + 32768 {
                    roc.checked_sub(1)
                } else if highest >= 32768 && highest > sequence + 32768 {
                    roc.checked_add(1).filter(|r| *r <= u32::MAX as u64)
                } else {
                    Some(roc)
                }
                .ok_or_else(|| anyhow::anyhow!("RTP index expired"))?;
                (roc << 16) | seq as u64
            }
        };
        if !window.accepts(index) {
            bail!("RTP replay rejected");
        }
        verify(
            &self.keys,
            &data[..end],
            &((index >> 16) as u32).to_be_bytes(),
            &data[end..],
        )?;
        if !self.sources.contains_key(&ssrc) && self.sources.len() >= MAX_SOURCES {
            bail!("Too many media sources");
        }
        let mut packet = data[..end].to_vec();
        crypt(&self.keys, ssrc, index, &mut packet[header..]);
        rtp::decode(&packet)?;
        self.sources.entry(ssrc).or_default().commit(index);
        Ok((packet, index))
    }
}
/// A single outbound sender SSRC, bounded receive contexts. Never wraps index.
pub struct ControlCipher {
    local: SrtpSessionKeys,
    remote: SrtpSessionKeys,
    local_ssrc: u32,
    index: u64,
    sources: BTreeMap<u32, Window>,
}
impl ControlCipher {
    pub fn new(context: &SrtpContext, ssrc: u32) -> Self {
        Self {
            local: context.local_srtcp_keys.clone(),
            remote: context.remote_srtcp_keys.clone(),
            local_ssrc: ssrc,
            index: 0,
            sources: BTreeMap::new(),
        }
    }
    pub fn protect(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() < 8
            || packet.len() > 1400
            || packet.len() % 4 != 0
            || packet[0] >> 6 != 2
            || u32_at(packet, 4) != self.local_ssrc
            || self.index > 0x7fff_ffff
        {
            bail!("Invalid or expired SRTCP sender");
        }
        let mut output = packet.to_vec();
        crypt(&self.local, self.local_ssrc, self.index, &mut output[8..]);
        output.extend((self.index as u32 | 0x8000_0000).to_be_bytes());
        let mut mac = Hmac::<Sha1>::new_from_slice(&self.local.auth_key)?;
        mac.update(&output);
        output.extend(&mac.finalize().into_bytes()[..TAG]);
        self.index += 1;
        Ok(output)
    }
    pub fn unprotect(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() < 22
            || packet.len() > 1414
            || packet[0] >> 6 != 2
            || !(192..=223).contains(&packet[1])
        {
            bail!("Invalid SRTCP packet");
        }
        let end = packet.len() - TAG;
        let trailer = end - 4;
        if trailer % 4 != 0 {
            bail!("Invalid SRTCP length");
        }
        let ssrc = u32_at(packet, 4);
        let flags = u32_at(packet, trailer);
        let index = (flags & 0x7fff_ffff) as u64;
        let window = self.sources.get(&ssrc).copied().unwrap_or_default();
        if !window.accepts(index) {
            bail!("SRTCP replay rejected");
        }
        verify(&self.remote, &packet[..end], &[], &packet[end..])?;
        if !self.sources.contains_key(&ssrc) && self.sources.len() >= MAX_SOURCES {
            bail!("Too many control sources");
        }
        let mut output = packet[..trailer].to_vec();
        if flags & 0x8000_0000 != 0 {
            crypt(&self.remote, ssrc, index, &mut output[8..]);
        }
        // RTCP syntax is checked by the control parser; authenticated malformed
        // messages still consume their index and can never be replayed.
        self.sources.entry(ssrc).or_default().commit(index);
        Ok(output)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn context() -> SrtpContext {
        let material = super::super::srtp::SrtpKeyingMaterial {
            master_key: [0x11; 16],
            master_salt: [0x22; 14],
            tag: 1,
        };
        super::super::srtp::create_context(&material, &material).unwrap()
    }
    fn rr(ssrc: u32) -> Vec<u8> {
        let mut b = vec![0x80, 201, 0, 1];
        b.extend(ssrc.to_be_bytes());
        b
    }
    #[test]
    fn control_authentication_reordering_replay_isolation_and_expiry() {
        let mut a = ControlCipher::new(&context(), 7);
        let mut b = ControlCipher::new(&context(), 8);
        let mut rx = ControlCipher::new(&context(), 9);
        let mut packets = Vec::new();
        for _ in 0..130 {
            packets.push(a.protect(&rr(7)).unwrap());
        }
        let mut bad = packets[129].clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(rx.unprotect(&bad).is_err());
        assert_eq!(rx.unprotect(&packets[129]).unwrap(), rr(7));
        assert_eq!(rx.unprotect(&packets[128]).unwrap(), rr(7));
        assert!(rx.unprotect(&packets[128]).is_err());
        assert!(rx.unprotect(&packets[0]).is_err());
        assert_eq!(rx.unprotect(&b.protect(&rr(8)).unwrap()).unwrap(), rr(8));
        a.index = 0x7fff_ffff;
        assert!(a.protect(&rr(7)).is_ok());
        assert!(a.protect(&rr(7)).is_err());
    }
    #[test]
    fn rtp_rollover_estimation_handles_exact_half_space_boundary() {
        let mut tx = context();
        let mut rx = RtpReceiver::new(&context());
        for (seq, index) in [(32767, 32767), (65535, 65535), (0, 65536)] {
            let packet =
                super::super::srtp::protect(&mut tx, &rtp::encode(0, seq, 0, 7, &[255; 160]))
                    .unwrap();
            assert_eq!(rx.unprotect(&packet).unwrap().1, index);
        }
    }
    #[test]
    fn authenticated_source_exit_preserves_tombstone_and_other_sources() {
        let mut tx = context();
        let mut rx = RtpReceiver::new(&context());
        let first =
            super::super::srtp::protect(&mut tx, &rtp::encode(0, 0, 0, 7, &[255; 160])).unwrap();
        rx.unprotect(&first).unwrap();
        rx.retire(7);
        let late =
            super::super::srtp::protect(&mut tx, &rtp::encode(0, 1, 160, 7, &[255; 160])).unwrap();
        assert!(rx.unprotect(&late).is_err());
        let other =
            super::super::srtp::protect(&mut tx, &rtp::encode(0, 0, 0, 8, &[255; 160])).unwrap();
        assert!(rx.unprotect(&other).is_ok());
        assert_eq!(rx.sources.len(), 2);
    }
    #[test]
    fn rtp_replay_windows_have_independent_rollover_and_reject_forgery() {
        let mut a = context();
        let mut b = context();
        let mut rx = RtpReceiver::new(&context());
        let encode = |ctx: &mut SrtpContext, ssrc, seq| {
            super::super::srtp::protect(ctx, &rtp::encode(0, seq, 0, ssrc, &[0xff; 160])).unwrap()
        };
        let a_last = encode(&mut a, 7, 65535);
        let a_next = encode(&mut a, 7, 0);
        let b_first = encode(&mut b, 8, 0);
        assert_eq!(rx.unprotect(&a_last).unwrap().1, 65535);
        assert_eq!(rx.unprotect(&a_next).unwrap().1, 65536);
        assert_eq!(rx.unprotect(&b_first).unwrap().1, 0);
        assert!(rx.unprotect(&a_last).is_err());
        let next = encode(&mut b, 8, 1);
        let mut bad = next.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(rx.unprotect(&bad).is_err());
        assert_eq!(rx.unprotect(&next).unwrap().1, 1);
        assert_eq!(rx.sources.len(), 2);
    }
}

#[cfg(test)]
mod independent_tests {
    use super::*;
    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }
    fn vector() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/native-media-libsrtp.json"
        ))
        .unwrap()
    }
    fn context() -> SrtpContext {
        super::tests::context()
    }
    #[test]
    fn native_crypto_matches_independent_libsrtp_vectors() {
        let v = vector();
        let plain = bytes(v["plain_control_hex"].as_str().unwrap());
        let encrypted = bytes(v["srtcp_hex"].as_str().unwrap());
        let mut tx = ControlCipher::new(&context(), 7);
        tx.protect(&plain).unwrap(); // libSRTP begins at index1
        assert_eq!(tx.protect(&plain).unwrap(), encrypted);
        let mut rx = ControlCipher::new(&context(), 9);
        assert_eq!(rx.unprotect(&encrypted).unwrap(), plain);
        assert!(rx.unprotect(&encrypted).is_err());
        let mut rtp = RtpReceiver::new(&context());
        for (value, expected) in v["rtp_hex"]
            .as_array()
            .unwrap()
            .iter()
            .zip([65535, 65536, 0])
        {
            let (packet, index) = rtp.unprotect(&bytes(value.as_str().unwrap())).unwrap();
            assert_eq!(index, expected);
            assert_eq!(
                super::super::rtp::decode(&packet).unwrap().payload,
                vec![255; 160]
            );
        }
    }
    #[test]
    #[ignore = "requires own local helper linked to libSRTP2 in TEAMS_LINUX_SRTP2_PEER; synthetic stdio only"]
    fn independent_libsrtp_accepts_native_control_including_initial_index_zero() {
        use std::io::Write;
        let helper = std::env::var("TEAMS_LINUX_SRTP2_PEER").unwrap();
        let plain = bytes(vector()["plain_control_hex"].as_str().unwrap());
        let encrypted = ControlCipher::new(&context(), 7).protect(&plain).unwrap();
        let mut child = std::process::Command::new(helper)
            .arg("unprotect")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let text = encrypted
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{text}\n").as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            bytes(String::from_utf8(output.stdout).unwrap().trim()),
            plain
        );
    }
}
