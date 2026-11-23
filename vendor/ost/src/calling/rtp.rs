//! RTP packet encoding/decoding and G.711 mu-law (PCMU) codec.
//!
//! RTP header format (RFC 3550):
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|X|  CC   |M|     PT      |       sequence number         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                           timestamp                           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |           synchronization source (SSRC) identifier            |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use anyhow::{bail, Result};

/// Minimum RTP header size in bytes (no CSRC, no extension).
pub const RTP_HEADER_SIZE: usize = 12;

/// Compute the full RTP header length from raw packet bytes.
///
/// Accounts for CSRC entries and header extensions (RFC 3550 §5.3.1).
/// Returns `None` if the packet is too short.
pub fn full_header_len(data: &[u8]) -> Option<usize> {
    if data.len() < RTP_HEADER_SIZE {
        return None;
    }
    let csrc_count = (data[0] & 0x0F) as usize;
    let has_extension = (data[0] >> 4) & 0x01 != 0;
    let mut len = RTP_HEADER_SIZE + csrc_count * 4;
    if data.len() < len {
        return None;
    }
    if has_extension {
        // Extension header: 2 bytes profile + 2 bytes length (in 32-bit words)
        if data.len() < len + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([data[len + 2], data[len + 3]]) as usize;
        len += 4 + ext_words * 4;
        if data.len() < len {
            return None;
        }
    }
    Some(len)
}

/// PCMU payload type (RFC 3551).
pub const PT_PCMU: u8 = 0;

/// Samples per 20ms packet at 8000 Hz.
pub const SAMPLES_PER_PACKET: usize = 160;

/// Packet interval in milliseconds.
pub const PACKET_INTERVAL_MS: u64 = 20;

/// Timestamp increment per packet (8000 Hz * 20ms = 160).
pub const TIMESTAMP_INCREMENT: u32 = 160;

/// Parsed RTP packet.
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

/// Bounded quality snapshot for a single RTP stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtpQualityStats {
    /// Outstanding missing sequence numbers within the active stream estimate.
    pub estimated_lost_packets: u64,
    /// Packets received behind the highest sequence number observed.
    pub reordered_packets: u64,
    /// Repeated sequence numbers observed inside the 64-packet tracking window.
    pub duplicate_packets: u64,
    /// RFC 3550 interarrival jitter in RTP clock ticks (8 kHz for PCMU).
    pub jitter_ticks: u32,
    /// RTP timestamp clock; zero preserves legacy PCMU snapshots.
    pub clock_rate:u32,
}

impl RtpQualityStats {
    /// Convert the negotiated RTP clock to milliseconds.
    pub fn jitter_millis(self) -> f32 {
        self.jitter_ticks as f32 *1000.0 / if self.clock_rate==0{8000.0}else{self.clock_rate as f32}
    }
}

/// Tracks a bounded sequence window and RFC 3550 interarrival jitter.
///
/// The 64-bit bitmap keeps duplicate/reorder accounting constant-space. A
/// packet that arrives late inside the window reduces the current loss
/// estimate; older packets remain diagnostic reorder events without changing
/// the estimate. A forward jump above `MAX_FORWARD_GAP` starts a new sequence
/// baseline instead of reporting a sender restart as thousands of lost packets.
#[derive(Debug, Default)]
pub struct RtpQualityTracker {
    highest_sequence: Option<u16>,
    received_window: u64,
    stats: RtpQualityStats,
    previous_transit: Option<i32>,
}

impl RtpQualityTracker {
    pub fn with_clock_rate(rate:u32)->Self{Self{stats:RtpQualityStats{clock_rate:rate,..Default::default()},..Default::default()}}
    pub fn set_clock_rate(&mut self,rate:u32){if self.stats.clock_rate!=rate{self.stats.clock_rate=rate;self.stats.jitter_ticks=0;self.previous_transit=None;}}
    const MAX_FORWARD_GAP: u16 = 3_000;

    /// Record one authenticated RTP packet and return the current quality view.
    /// `arrival_clock` must use the same clock as the RTP timestamp.
    pub fn observe(
        &mut self,
        sequence_number: u16,
        rtp_timestamp: u32,
        arrival_clock: u32,
    ) -> RtpQualityStats {
        self.observe_sequence(sequence_number);
        self.observe_jitter(rtp_timestamp, arrival_clock);
        self.stats
    }

    pub fn stats(&self) -> RtpQualityStats {
        self.stats
    }

    fn observe_sequence(&mut self, sequence_number: u16) {
        let Some(highest) = self.highest_sequence else {
            self.highest_sequence = Some(sequence_number);
            self.received_window = 1;
            return;
        };

        let forward = sequence_number.wrapping_sub(highest);
        if forward == 0 {
            self.stats.duplicate_packets = self.stats.duplicate_packets.saturating_add(1);
            return;
        }
        if forward < (u16::MAX / 2) {
            if forward > Self::MAX_FORWARD_GAP {
                self.highest_sequence = Some(sequence_number);
                self.received_window = 1;
                self.stats.estimated_lost_packets = 0;
                return;
            }

            self.stats.estimated_lost_packets = self
                .stats
                .estimated_lost_packets
                .saturating_add(u64::from(forward - 1));
            self.received_window = if forward >= 64 {
                1
            } else {
                (self.received_window << forward) | 1
            };
            self.highest_sequence = Some(sequence_number);
            return;
        }

        let behind = highest.wrapping_sub(sequence_number);
        self.stats.reordered_packets = self.stats.reordered_packets.saturating_add(1);
        if behind >= 64 {
            return;
        }
        let bit = 1u64 << behind;
        if self.received_window & bit != 0 {
            self.stats.duplicate_packets = self.stats.duplicate_packets.saturating_add(1);
        } else {
            self.received_window |= bit;
            self.stats.estimated_lost_packets = self.stats.estimated_lost_packets.saturating_sub(1);
        }
    }

    fn observe_jitter(&mut self, rtp_timestamp: u32, arrival_clock: u32) {
        let transit = arrival_clock.wrapping_sub(rtp_timestamp) as i32;
        if let Some(previous) = self.previous_transit {
            let difference = transit.wrapping_sub(previous).unsigned_abs();
            let adjustment = (i64::from(difference) - i64::from(self.stats.jitter_ticks)) / 16;
            self.stats.jitter_ticks = (i64::from(self.stats.jitter_ticks) + adjustment)
                .clamp(0, i64::from(u32::MAX)) as u32;
        }
        self.previous_transit = Some(transit);
    }
}

/// Encode an RTP packet into bytes.
pub fn encode(payload_type: u8, seq: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RTP_HEADER_SIZE + payload.len());

    // Byte 0: V=2, P=0, X=0, CC=0 -> 0x80
    buf.push(0x80);
    // Byte 1: M=0, PT
    buf.push(payload_type & 0x7F);
    // Bytes 2-3: sequence number (big-endian)
    buf.extend_from_slice(&seq.to_be_bytes());
    // Bytes 4-7: timestamp (big-endian)
    buf.extend_from_slice(&timestamp.to_be_bytes());
    // Bytes 8-11: SSRC (big-endian)
    buf.extend_from_slice(&ssrc.to_be_bytes());
    // Payload
    buf.extend_from_slice(payload);

    buf
}

/// Decode bytes into an RTP packet.
pub fn decode(data: &[u8]) -> Result<RtpPacket> {
    if data.len() < RTP_HEADER_SIZE {
        bail!("RTP packet too short: {} bytes", data.len());
    }

    let version = (data[0] >> 6) & 0x03;
    if version != 2 {
        bail!("Unsupported RTP version: {}", version);
    }

    let padding = (data[0] >> 5) & 0x01 != 0;
    let extension = (data[0] >> 4) & 0x01 != 0;
    let csrc_count = data[0] & 0x0F;
    let marker = (data[1] >> 7) & 0x01 != 0;
    let payload_type = data[1] & 0x7F;
    let sequence_number = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    // RFC3550: extension words follow the CSRC list and are not codec bytes.
    // SRTP uses the same boundary when authenticating/decrypting this packet.
    let header_len = full_header_len(data)
        .ok_or_else(|| anyhow::anyhow!("Truncated RTP header or extension"))?;
    let mut payload_end = data.len();
    if padding {
        let count = usize::from(*data.last().unwrap());
        if count == 0 || count > data.len() - header_len {
            bail!("Invalid RTP padding");
        }
        payload_end -= count;
    }
    let payload = data[header_len..payload_end].to_vec();

    Ok(RtpPacket {
        version,
        padding,
        extension,
        csrc_count,
        marker,
        payload_type,
        sequence_number,
        timestamp,
        ssrc,
        payload,
    })
}

// --- G.711 mu-law codec (PCMU) ---
//
// ITU-T G.711. Encodes 16-bit linear PCM to 8-bit mu-law and back.
// The mu-law companding curve: F(x) = sgn(x) * ln(1 + mu*|x|) / ln(1 + mu)
// where mu = 255.
//
// In practice this is implemented with a segment/quantization lookup.

/// Encode a 16-bit linear PCM sample to 8-bit mu-law (ITU-T G.711).
///
/// Uses the standard algorithm from the G.711 reference code (Sun Microsystems).
pub fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i16 = 0x84; // 132
    const CLIP: i16 = 32635;

    // Get the sign and the magnitude of the value.
    let sign: i16;
    let mut mag: i16;

    if sample < 0 {
        // Careful: -(-32768) overflows, but we clip before it matters.
        mag = if sample == i16::MIN { CLIP } else { -sample };
        sign = 0x80;
    } else {
        mag = sample;
        sign = 0;
    }

    if mag > CLIP {
        mag = CLIP;
    }
    mag += BIAS;

    // Find the segment (exponent). The biased magnitude fits in 14 bits + sign.
    let mut exponent: u8 = 7;
    let mut exp_mask: i16 = 0x4000;
    while exponent > 0 && (mag & exp_mask) == 0 {
        exponent -= 1;
        exp_mask >>= 1;
    }

    // Extract 4-bit mantissa
    let mantissa = ((mag >> (exponent as i16 + 3)) & 0x0F) as u8;

    // Compose the byte and complement it (mu-law convention)
    let mut ulaw_byte = (sign as u8) | (exponent << 4) | mantissa;
    ulaw_byte = !ulaw_byte;
    ulaw_byte
}

/// Decode an 8-bit mu-law sample to 16-bit linear PCM (ITU-T G.711).
///
/// Inverse of `linear_to_ulaw`. The encoder adds BIAS=132 to the magnitude,
/// finds the segment (exponent) as the position of the leading bit, and
/// extracts 4 mantissa bits. This decoder reconstructs the midpoint of the
/// quantization step.
pub fn ulaw_to_linear(sample: u8) -> i16 {
    // Complement to recover the encoded sign/exponent/mantissa byte
    let ulaw = !sample;
    let sign = (ulaw & 0x80) != 0;
    let exponent = ((ulaw >> 4) & 0x07) as u32;
    let mantissa = (ulaw & 0x0F) as i32;

    // Reconstruct the biased magnitude then subtract bias.
    // The encoder extracts mantissa = (biased >> (exp+3)) & 0xF, discarding the
    // leading 1 bit. To decode, restore the leading 1 (0x10), combine with mantissa,
    // shift back, and add half an LSB for midpoint reconstruction.
    let biased = ((mantissa | 0x10) << (exponent + 3)) + (1i32 << (exponent + 2));
    let mag = biased - 132;
    let mag = mag.max(0);

    if sign {
        -(mag as i16)
    } else {
        mag as i16
    }
}

/// Generate a silence payload for PCMU (160 bytes of mu-law silence = 0xFF).
/// 0xFF in mu-law decodes to approximately 0 (very small positive value near zero).
pub fn silence_payload() -> Vec<u8> {
    vec![0xFF; SAMPLES_PER_PACKET]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiated_rtp_clock_converts_opus_jitter_and_resets_only_clock_history(){
        let mut tracker=RtpQualityTracker::with_clock_rate(48000);
        tracker.observe(1,0,0);let stats=tracker.observe(2,960,1008);assert_eq!(stats.clock_rate,48000);assert_eq!(stats.jitter_ticks,3);assert_eq!(stats.jitter_millis(),0.0625);
        tracker.set_clock_rate(8000);let stats=tracker.observe(3,320,320);assert_eq!(stats.clock_rate,8000);assert_eq!(stats.jitter_ticks,0);assert_eq!(stats.estimated_lost_packets,0);
    }
    #[test]
    fn test_encode_decode_roundtrip() {
        let payload = vec![0xFF; 160];
        let encoded = encode(PT_PCMU, 1, 160, 0x12345678, &payload);
        assert_eq!(encoded.len(), RTP_HEADER_SIZE + 160);

        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.payload_type, PT_PCMU);
        assert_eq!(decoded.sequence_number, 1);
        assert_eq!(decoded.timestamp, 160);
        assert_eq!(decoded.ssrc, 0x12345678);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn decoder_separates_csrc_extension_and_padding_from_audio() {
        let payload = vec![0xff; 160];
        let base = encode(PT_PCMU, 1, 160, 42, &payload);
        let mut bytes = base[..RTP_HEADER_SIZE].to_vec();
        bytes[0] |= 0x32; // Padding, extension, two CSRC entries.
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0xbe, 0xde, 0, 1, 0x10, 0, 0, 0]);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&[0, 0, 0, 4]);
        let decoded = decode(&bytes).unwrap();
        assert!(decoded.extension && decoded.padding);
        assert_eq!(decoded.csrc_count, 2);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn decoder_rejects_truncated_extensions_and_invalid_padding() {
        let mut bytes = encode(PT_PCMU, 1, 160, 42, &[1, 2, 3, 4]);
        bytes[0] |= 0x10;
        // Extension claims more bytes than the entire packet contains.
        bytes[14..16].copy_from_slice(&16u16.to_be_bytes());
        assert!(decode(&bytes).is_err());
        for count in [0, 5, 255] {
            let mut bytes = encode(PT_PCMU, 1, 160, 42, &[1, 2, 3, count]);
            bytes[0] |= 0x20;
            assert!(decode(&bytes).is_err());
        }
    }

    #[test]
    fn test_decode_too_short() {
        assert!(decode(&[0x80, 0x00]).is_err());
    }

    #[test]
    fn test_decode_wrong_version() {
        let mut data = [0u8; 12];
        data[0] = 0x00; // version 0
        assert!(decode(&data).is_err());
    }

    #[test]
    fn quality_tracker_counts_gaps_and_recovers_late_packets_within_its_window() {
        let mut quality = RtpQualityTracker::default();
        quality.observe(10, 0, 0);
        assert_eq!(quality.observe(13, 480, 480).estimated_lost_packets, 2);

        let after_first_late = quality.observe(11, 160, 640);
        assert_eq!(after_first_late.estimated_lost_packets, 1);
        let after_second_late = quality.observe(12, 320, 800);
        assert_eq!(after_second_late.estimated_lost_packets, 0);
        assert_eq!(after_second_late.reordered_packets, 2);
    }

    #[test]
    fn quality_tracker_handles_sequence_wrap_duplicates_and_sender_restart() {
        let mut quality = RtpQualityTracker::default();
        for sequence in [u16::MAX - 1, u16::MAX, 0, 1] {
            quality.observe(sequence, 0, 0);
        }
        assert_eq!(quality.stats().estimated_lost_packets, 0);

        let duplicate = quality.observe(1, 0, 0);
        assert_eq!(duplicate.duplicate_packets, 1);
        assert_eq!(duplicate.estimated_lost_packets, 0);

        let restarted = quality.observe(5_000, 0, 0);
        assert_eq!(restarted.estimated_lost_packets, 0);
    }

    #[test]
    fn quality_tracker_keeps_old_packets_out_of_the_loss_recovery_window() {
        let mut quality = RtpQualityTracker::default();
        quality.observe(100, 0, 0);
        let after_gap = quality.observe(200, 16_000, 16_000);
        assert_eq!(after_gap.estimated_lost_packets, 99);

        let old_packet = quality.observe(100, 0, 16_160);
        assert_eq!(old_packet.estimated_lost_packets, 99);
        assert_eq!(old_packet.reordered_packets, 1);
    }

    #[test]
    fn quality_tracker_updates_jitter_from_zero_transit_and_clock_wrap() {
        let mut quality = RtpQualityTracker::default();
        quality.observe(1, 0, 0);
        let jitter = quality.observe(2, 160, 200);
        assert_eq!(jitter.jitter_ticks, 2);
        assert_eq!(jitter.jitter_millis(), 0.25);

        let mut wrapped = RtpQualityTracker::default();
        wrapped.observe(1, u32::MAX, 0);
        let stable_transit = wrapped.observe(2, 159, 160);
        assert_eq!(stable_transit.jitter_ticks, 0);
    }

    #[test]
    fn test_ulaw_roundtrip_zero() {
        let encoded = linear_to_ulaw(0);
        let decoded = ulaw_to_linear(encoded);
        // mu-law zero is not exactly zero, but close
        assert!(decoded.abs() < 4, "decoded zero: {}", decoded);
    }

    #[test]
    fn test_ulaw_roundtrip_positive() {
        // mu-law is lossy with ~33dB SNR. For small values the quantization
        // error can be large relative to the value. For large values it's
        // proportionally smaller. We just verify the decoded value has the
        // same sign and the right order of magnitude.
        for &sample in &[100i16, 1000, 10000, 30000] {
            let encoded = linear_to_ulaw(sample);
            let decoded = ulaw_to_linear(encoded);
            assert!(
                decoded > 0,
                "sign mismatch for sample={}: decoded={}",
                sample,
                decoded
            );
            let s = sample as i32;
            let d = decoded as i32;
            assert!(
                d >= s / 8 && d <= s * 2,
                "sample={}, decoded={} out of range",
                sample,
                decoded,
            );
        }
    }

    #[test]
    fn test_ulaw_roundtrip_negative() {
        for &sample in &[-100i16, -1000, -10000, -30000] {
            let encoded = linear_to_ulaw(sample);
            let decoded = ulaw_to_linear(encoded);
            assert!(
                decoded < 0,
                "sign mismatch for sample={}: decoded={}",
                sample,
                decoded
            );
            let abs_sample = sample.unsigned_abs() as i32;
            let abs_decoded = decoded.unsigned_abs() as i32;
            assert!(
                abs_decoded >= abs_sample / 8 && abs_decoded <= abs_sample * 2,
                "sample={}, decoded={} out of range",
                sample,
                decoded,
            );
        }
    }

    #[test]
    fn test_silence_payload() {
        let silence = silence_payload();
        assert_eq!(silence.len(), SAMPLES_PER_PACKET);
        assert!(silence.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn test_ulaw_silence_decodes_near_zero() {
        let decoded = ulaw_to_linear(0xFF);
        assert!(decoded.abs() < 4, "silence decoded to: {}", decoded);
    }
}
