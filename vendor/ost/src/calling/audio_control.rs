//! Own bounded compound RTCP reports for the native audio path (RFC3550/5761).
//! Input must already be SRTCP authenticated and replay checked.
use super::rtp::{RtpPacket, RtpQualityTracker};
use anyhow::{bail, Result};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
const MAX_PACKET: usize = 1400;
const MAX_SOURCES: usize = 8;
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ControlStats {
    pub reports_sent: u64,
    pub reports_received: u64,
    pub rejected_packets: u64,
    pub bye_sources_received: u64,
    pub bye_packets_sent: u64,
    pub remote_fraction_lost: Option<u8>,
    pub remote_cumulative_lost: Option<i32>,
    pub remote_jitter_ticks: Option<u32>,
    pub round_trip_ms: Option<u32>,
}
#[derive(Debug, Clone, Copy)]
pub struct ReceptionReport {
    pub ssrc: u32,
    pub fraction_lost: u8,
    pub cumulative_lost: i32,
    pub highest_sequence: u32,
    pub jitter: u32,
    pub last_sr: u32,
    pub delay_since_sr: u32,
}
#[derive(Debug)]
pub struct Compound {
    pub sender: u32,
    pub sender_reports: Vec<(u32, u64)>,
    pub receptions: Vec<ReceptionReport>,
    pub bye: Vec<u32>,
}
fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}
/// Reserved RTCP type range, including feedback/XR. PCMU/Opus PTs cannot collide.
pub fn is_control(data: &[u8]) -> bool {
    data.len() >= 8 && data[0] >> 6 == 2 && (192..=223).contains(&data[1])
}
pub fn parse(data: &[u8]) -> Result<Compound> {
    if data.len() < 8 || data.len() > MAX_PACKET || data.len() % 4 != 0 {
        bail!("Invalid compound RTCP length");
    }
    let sender = word(data, 4);
    let mut parsed = Compound {
        sender,
        sender_reports: Vec::new(),
        receptions: Vec::new(),
        bye: Vec::new(),
    };
    let mut offset = 0;
    let mut blocks = 0;
    let mut cname = false;
    let mut first_type = 0;
    while offset < data.len() {
        blocks += 1;
        if blocks > 16 || offset + 4 > data.len() {
            bail!("Invalid RTCP blocks");
        }
        let packet = &data[offset..];
        let count = (packet[0] & 31) as usize;
        if packet[0] >> 6 != 2 || !(192..=223).contains(&packet[1]) {
            bail!("Invalid RTCP header");
        }
        let size = (u16::from_be_bytes([packet[2], packet[3]]) as usize + 1) * 4;
        if size < 4 || size > packet.len() {
            bail!("Truncated RTCP block");
        }
        if offset == 0 { first_type = packet[1]; }
        let mut end = size;
        if packet[0] & 0x20 != 0 {
            let padding = packet[size - 1] as usize;
            if offset + size != data.len() || padding == 0 || padding > size - 4 {
                bail!("Invalid RTCP padding");
            }
            end -= padding;
        }
        let packet = &packet[..end];
        match packet[1] {
            200 | 201 => {
                let head = if packet[1] == 200 { 28 } else { 8 };
                if packet.len() < head + count * 24 {
                    bail!("Invalid RTCP report count");
                }
                if packet[1] == 200 {
                    parsed.sender_reports.push((
                        word(packet, 4),
                        ((word(packet, 8) as u64) << 32) | word(packet, 12) as u64,
                    ));
                }
                let reports_end = head + count * 24;
                validate_extensions(&packet[reports_end..])?;
                for block in packet[head..reports_end].chunks_exact(24) {
                    parsed.receptions.push(ReceptionReport {
                        ssrc: word(block, 0),
                        fraction_lost: block[4],
                        cumulative_lost: 0,
                        highest_sequence: 0,
                        jitter: 0,
                        last_sr: 0,
                        delay_since_sr: 0,
                    });
                    // Report source occupies bytes0..4; loss fraction is byte4.
                    let report = parsed.receptions.last_mut().unwrap();
                    let loss =
                        ((block[5] as u32) << 16) | ((block[6] as u32) << 8) | block[7] as u32;
                    report.cumulative_lost = ((loss << 8) as i32) >> 8;
                    report.highest_sequence = word(block, 8);
                    report.jitter = word(block, 12);
                    report.last_sr = word(block, 16);
                    report.delay_since_sr = word(block, 20);
                }
            }
            202 => {
                let mut cursor = 4;
                for _ in 0..count {
                    if cursor + 4 > packet.len() {
                        bail!("Invalid SDES source");
                    }
                    let source = word(packet, cursor);
                    cursor += 4;
                    let mut ended = false;
                    while cursor < packet.len() {
                        let kind = packet[cursor];
                        cursor += 1;
                        if kind == 0 {
                            ended = true;
                            break;
                        }
                        if cursor >= packet.len() {
                            bail!("Invalid SDES item");
                        }
                        let len = packet[cursor] as usize;
                        cursor += 1;
                        if cursor + len > packet.len() {
                            bail!("Truncated SDES item");
                        }
                        if kind == 1 {
                            if len == 0 {
                                bail!("Empty CNAME");
                            }
                            if source == sender {
                                cname = true;
                            }
                        }
                        cursor += len;
                    }
                    if !ended {
                        bail!("SDES missing end");
                    }
                    while cursor % 4 != 0 {
                        if packet.get(cursor) != Some(&0) {
                            bail!("Invalid SDES alignment");
                        }
                        cursor += 1;
                    }
                }
                if cursor != packet.len() {
                    bail!("Trailing SDES data");
                }
            }
            203 => {
                if count == 0 || packet.len() < 4 + count * 4 {
                    bail!("Invalid BYE sources");
                }
                for offset in (4..4 + count * 4).step_by(4) {
                    parsed.bye.push(word(packet, offset));
                }
                let tail = &packet[4 + count * 4..];
                if !tail.is_empty() {
                    let len = tail[0] as usize;
                    if len + 1 > tail.len()
                        || tail.len() - len - 1 > 3
                        || tail[len + 1..].iter().any(|v| *v != 0)
                    {
                        bail!("Invalid BYE reason");
                    }
                }
            }
            205 | 206 if packet.len() < 12 => bail!("Truncated RTCP feedback"),
            _ => {}
        }
        offset += size;
    }
    // MS-RTP 2.2.2 permits these four standalone packets. Compound
    // packets retain the RFC3550 first-report and CNAME requirements.
    if blocks == 1 {
        if !matches!(first_type, 200..=203) { bail!("Unsupported standalone RTCP"); }
    } else if !matches!(first_type, 200 | 201) || !cname {
        bail!("Compound RTCP requires first SR/RR and sender CNAME");
    }
    Ok(parsed)
}
// MS-RTP 2.2.11: two-byte type, two-byte length including the header.
fn validate_extensions(mut tail: &[u8]) -> Result<()> {
    let mut count = 0;
    while !tail.is_empty() {
        count += 1;
        if count > 20 || tail.len() < 4 { bail!("Invalid RTCP extensions"); }
        let length = u16::from_be_bytes([tail[2], tail[3]]) as usize;
        if length < 4 || length % 4 != 0 || length > tail.len() {
            bail!("Invalid RTCP extension length");
        }
        tail = &tail[length..];
    }
    Ok(())
}
struct Source {
    base: Option<u64>,
    highest: u64,
    received: u64,
    expected_prior: u64,
    received_prior: u64,
    quality: RtpQualityTracker,
    last_sr: u32,
    sr_received: Option<Duration>,
    last_seen: Duration,
}
impl Source {
    fn new(now: Duration) -> Self {
        Self {
            base: None,
            highest: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            quality: RtpQualityTracker::default(),
            last_sr: 0,
            sr_received: None,
            last_seen: now,
        }
    }
    fn expected(&self) -> u64 {
        self.base.map(|base| self.highest - base + 1).unwrap_or(0)
    }
    fn report(&self, ssrc: u32, now: Duration) -> ReceptionReport {
        let expected = self.expected();
        let interval = expected.saturating_sub(self.expected_prior);
        let received = self.received.saturating_sub(self.received_prior);
        let lost = interval.saturating_sub(received);
        ReceptionReport {
            ssrc,
            fraction_lost: if interval == 0 {
                0
            } else {
                (lost.saturating_mul(256) / interval).min(255) as u8
            },
            cumulative_lost: (expected as i64 - self.received as i64).clamp(-8388608, 8388607)
                as i32,
            highest_sequence: self.highest as u32,
            jitter: self.quality.stats().jitter_ticks,
            last_sr: self.last_sr,
            delay_since_sr: self
                .sr_received
                .map(|t| {
                    (now.saturating_sub(t).as_micros() * 65536 / 1_000_000).min(u32::MAX as u128)
                        as u32
                })
                .unwrap_or(0),
        }
    }
}
pub struct ReportPlan {
    pub bytes: Vec<u8>,
    intervals: Vec<(u32, u64, u64)>,
    sr: Option<u32>,
    created: Duration,
    pub bye: bool,
}
pub struct Reports {
    ssrc: u32,
    clock: u32,
    bandwidth: f64,
    cname: String,
    sent: u64,
    octets: u64,
    sources: BTreeMap<u32, Source>,
    history: VecDeque<(u32, Duration)>,
    stats: ControlStats,
    average_size: f64,
}
impl Reports {
    pub fn new(ssrc: u32, clock: u32, bandwidth_bits: u32, cname: String) -> Result<Self> {
        if !matches!(clock, 8000 | 48000)
            || bandwidth_bits < 6000
            || cname.is_empty()
            || cname.len() > 255
            || !cname.bytes().all(|c| c.is_ascii_graphic())
        {
            bail!("Invalid native RTCP profile");
        }
        Ok(Self {
            ssrc,
            clock,
            bandwidth: bandwidth_bits as f64 * 0.05 / 8.0,
            cname,
            sent: 0,
            octets: 0,
            sources: BTreeMap::new(),
            history: VecDeque::new(),
            stats: ControlStats::default(),
            average_size: 128.0,
        })
    }
    pub fn sent_rtp(&mut self, payload_len: usize) {
        self.sent += 1;
        self.octets += payload_len as u64;
    }
    pub fn receive_rtp(&mut self, packet: &RtpPacket, index: u64, clock: u32, now: Duration) {
        if index >= (1u64 << 48) || !matches!(clock, 8000 | 48000) {
            return;
        }
        if !self.sources.contains_key(&packet.ssrc) && self.sources.len() >= MAX_SOURCES {
            return;
        }
        let source = self
            .sources
            .entry(packet.ssrc)
            .or_insert_with(|| Source::new(now));
        source.base = Some(source.base.map(|old| old.min(index)).unwrap_or(index));
        source.highest = source.highest.max(index);
        source.received += 1;
        source.quality.set_clock_rate(clock);
        source.quality.observe(
            packet.sequence_number,
            packet.timestamp,
            (now.as_nanos() * clock as u128 / 1_000_000_000) as u32,
        );
        source.last_seen = now;
    }
    pub fn receive_control(&mut self, data: &[u8], now: Duration) -> Result<Vec<u32>> {
        let compound = parse(data)?;
        self.stats.reports_received += 1;
        self.average_size = (15.0 * self.average_size + data.len() as f64 + 42.0) / 16.0;
        for (ssrc, ntp) in compound.sender_reports {
            if !self.sources.contains_key(&ssrc) && self.sources.len() >= MAX_SOURCES {
                continue;
            }
            let source = self.sources.entry(ssrc).or_insert_with(|| Source::new(now));
            source.last_sr = (ntp >> 16) as u32;
            source.sr_received = Some(now);
            source.last_seen = now;
        }
        for report in compound
            .receptions
            .into_iter()
            .filter(|r| r.ssrc == self.ssrc)
        {
            self.stats.remote_fraction_lost = Some(report.fraction_lost);
            self.stats.remote_cumulative_lost = Some(report.cumulative_lost);
            self.stats.remote_jitter_ticks = Some(report.jitter);
            if report.last_sr != 0 {
                if let Some((_, sent)) = self.history.iter().find(|(id, _)| *id == report.last_sr) {
                    let delay =
                        Duration::from_micros(report.delay_since_sr as u64 * 1_000_000 / 65536);
                    if let Some(rtt) = now
                        .checked_sub(*sent)
                        .and_then(|t| t.checked_sub(delay))
                        .filter(|t| *t <= Duration::from_secs(60))
                    {
                        self.stats.round_trip_ms = Some(rtt.as_millis() as u32);
                    }
                }
            }
        }
        for ssrc in &compound.bye {
            self.sources.remove(ssrc);
            self.stats.bye_sources_received += 1;
        }
        Ok(compound.bye)
    }
    pub fn rejected(&mut self) {
        self.stats.rejected_packets += 1;
    }
    pub fn stats(&self) -> ControlStats {
        self.stats
    }
    /// RFC3550 interval: randomized, initial half minimum, bandwidth estimate
    /// and compensation. The native profile has no negotiated rtcp-rsize.
    pub fn interval(&self, initial: bool, random: f64) -> Duration {
        let members = (self.sources.len() + 1) as f64;
        let random = if random.is_finite() {
            random.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let minimum = if initial { 2.5 } else { 5.0 };
        let base = (self.average_size * members / self.bandwidth).max(minimum);
        Duration::from_secs_f64((base * (0.5 + random) / 1.21828).max(0.5))
    }
    pub fn plan(&mut self, now: Duration, ntp: u64, bye: bool) -> ReportPlan {
        self.sources
            .retain(|_, s| now.saturating_sub(s.last_seen) < Duration::from_secs(60));
        let selected = self
            .sources
            .iter()
            .filter(|(_, s)| s.base.is_some())
            .collect::<Vec<_>>();
        let mut bytes = Vec::with_capacity(512);
        let count = selected.len() as u8;
        let sender = self.sent > 0;
        bytes.extend([0x80 | count, if sender { 200 } else { 201 }, 0, 0]);
        bytes.extend(self.ssrc.to_be_bytes());
        let sr = if sender {
            bytes.extend(ntp.to_be_bytes());
            let timestamp = (now.as_nanos() * self.clock as u128 / 1_000_000_000) as u32;
            bytes.extend(timestamp.to_be_bytes());
            bytes.extend((self.sent as u32).to_be_bytes());
            bytes.extend((self.octets as u32).to_be_bytes());
            Some((ntp >> 16) as u32)
        } else {
            None
        };
        let mut intervals = Vec::new();
        for (ssrc, source) in selected {
            let report = source.report(*ssrc, now);
            bytes.extend(ssrc.to_be_bytes());
            bytes.push(report.fraction_lost);
            let loss = (report.cumulative_lost as u32).to_be_bytes();
            bytes.extend(&loss[1..]);
            for value in [
                report.highest_sequence,
                report.jitter,
                report.last_sr,
                report.delay_since_sr,
            ] {
                bytes.extend(value.to_be_bytes());
            }
            intervals.push((*ssrc, source.expected(), source.received));
        }
        let size = (bytes.len() / 4 - 1) as u16;
        bytes[2..4].copy_from_slice(&size.to_be_bytes());
        let start = bytes.len();
        bytes.extend([0x81, 202, 0, 0]);
        bytes.extend(self.ssrc.to_be_bytes());
        bytes.extend([1, self.cname.len() as u8]);
        bytes.extend(self.cname.as_bytes());
        bytes.push(0);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        let size = ((bytes.len() - start) / 4 - 1) as u16;
        bytes[start + 2..start + 4].copy_from_slice(&size.to_be_bytes());
        if bye {
            bytes.extend([0x81, 203, 0, 1]);
            bytes.extend(self.ssrc.to_be_bytes());
        }
        ReportPlan {
            bytes,
            intervals,
            sr,
            created: now,
            bye,
        }
    }
    pub fn commit(&mut self, plan: &ReportPlan) {
        self.stats.reports_sent += 1;
        if plan.bye {
            self.stats.bye_packets_sent += 1;
        }
        for (ssrc, expected, received) in &plan.intervals {
            if let Some(source) = self.sources.get_mut(ssrc) {
                source.expected_prior = *expected;
                source.received_prior = *received;
            }
        }
        if let Some(id) = plan.sr {
            self.history
                .retain(|(_, at)| plan.created.saturating_sub(*at) < Duration::from_secs(60));
            if self.history.len() >= 8 {
                self.history.pop_front();
            }
            self.history.push_back((id, plan.created));
        }
        self.average_size = (15.0 * self.average_size + plan.bytes.len() as f64 + 42.0) / 16.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reports(ssrc: u32) -> Reports {
        Reports::new(ssrc, 48000, 16000, "fixture@native.invalid".into()).unwrap()
    }
    fn packet(seq: u16) -> RtpPacket {
        super::super::rtp::decode(&super::super::rtp::encode(
            111,
            seq,
            seq as u32 * 960,
            7,
            &[0],
        ))
        .unwrap()
    }
    #[test]
    fn reports_use_base_sequence_interval_loss_and_recovery_with_wrap() {
        let mut source = reports(9);
        for (seq, index) in [(65534, 65534), (0, 65536)] {
            source.receive_rtp(&packet(seq), index, 48000, Duration::from_millis(index));
        }
        let first = source.plan(Duration::from_secs(100), 1, false);
        let parsed = parse(&first.bytes).unwrap();
        assert_eq!(parsed.receptions.len(), 1);
        let r = parsed.receptions[0];
        assert_eq!(r.highest_sequence, 65536);
        assert_eq!(r.cumulative_lost, 1);
        assert_eq!(r.fraction_lost, 85);
        source.commit(&first);
        source.receive_rtp(&packet(65535), 65535, 48000, Duration::from_secs(100));
        let second = source.plan(Duration::from_secs(100), 2, false);
        let r = parse(&second.bytes).unwrap().receptions[0];
        assert_eq!(r.cumulative_lost, 0);
        assert_eq!(r.fraction_lost, 0);
        // Building an unsent report cannot advance the interval baseline.
        source.receive_rtp(&packet(2), 65538, 48000, Duration::from_secs(100));
        let unsent = source.plan(Duration::from_secs(100), 3, false);
        let repeated = source.plan(Duration::from_secs(100), 4, false);
        assert_eq!(
            parse(&unsent.bytes).unwrap().receptions[0].fraction_lost,
            parse(&repeated.bytes).unwrap().receptions[0].fraction_lost
        );
    }
    #[test]
    fn sender_report_clock_counts_lsr_dlsr_rtt_and_bye_are_consistent() {
        let mut a = reports(7);
        let mut b = reports(9);
        a.sent_rtp(100);
        a.sent_rtp(120);
        let ntp = 0xe000000012340000u64;
        let sr = a.plan(Duration::from_secs(1), ntp, false);
        a.commit(&sr);
        assert_eq!(word(&sr.bytes, 16), 48000);
        assert_eq!(word(&sr.bytes, 20), 2);
        assert_eq!(word(&sr.bytes, 24), 220);
        b.receive_control(&sr.bytes, Duration::from_millis(1100))
            .unwrap();
        b.receive_rtp(&packet(400), 400, 48000, Duration::from_millis(1100));
        let rr = b.plan(Duration::from_millis(1600), ntp + 1, false);
        let r = parse(&rr.bytes).unwrap().receptions[0];
        assert_eq!(r.last_sr, (ntp >> 16) as u32);
        assert_eq!(r.delay_since_sr, 32768);
        a.receive_control(&rr.bytes, Duration::from_millis(1700))
            .unwrap();
        assert_eq!(a.stats().round_trip_ms, Some(200));
        let bye = b.plan(Duration::from_millis(1800), ntp + 2, true);
        assert_eq!(
            a.receive_control(&bye.bytes, Duration::from_millis(1900))
                .unwrap(),
            vec![9]
        );
        assert_eq!(a.stats().bye_sources_received, 1);
        b.commit(&bye);
        assert_eq!(b.stats().bye_packets_sent, 1);
    }
    #[test]
    fn unknown_or_impossible_rtt_never_becomes_a_latency_measurement() {
        let mut a = reports(7);
        let mut b = reports(9);
        a.sent_rtp(160);
        let sr = a.plan(Duration::ZERO, 0x1234567890000000, false);
        b.receive_control(&sr.bytes, Duration::ZERO).unwrap();
        b.receive_rtp(&packet(0), 0, 48000, Duration::ZERO);
        let rr = b.plan(Duration::from_secs(10), 1, false);
        a.receive_control(&rr.bytes, Duration::from_secs(1))
            .unwrap();
        assert_eq!(a.stats().round_trip_ms, None);
        a.commit(&sr);
        a.receive_control(&rr.bytes, Duration::from_secs(1))
            .unwrap();
        assert_eq!(a.stats().round_trip_ms, None);
        a.receive_control(&rr.bytes, Duration::from_secs(100))
            .unwrap();
        assert_eq!(a.stats().round_trip_ms, None);
    }
    #[test]
    fn compound_parser_rejects_truncation_padding_counts_cname_and_trailing_data() {
        let valid = reports(9).plan(Duration::ZERO, 1, false).bytes;
        for length in 0..valid.len() {
            if length != 8 { assert!(parse(&valid[..length]).is_err(), "length{length}"); }
        }
        for mutation in 0..5 {
            let mut packet = valid.clone();
            match mutation {
                0 => packet[0] = 0x40,
                1 => packet[0] |= 1,
                2 => packet[2] = 0xff,
                3 => packet[0] |= 0x20,
                _ => packet.extend([0, 0, 0, 0]),
            }
            assert!(parse(&packet).is_err());
        }
        assert!(parse(&valid[..8]).is_ok()); // Microsoft standalone RR
        let mut negative = reports(9);
        negative.receive_rtp(&packet(0), 0, 48000, Duration::ZERO);
        let mut plan = negative.plan(Duration::ZERO, 1, false);
        plan.bytes[13..16].copy_from_slice(&[0xff, 0xff, 0xfe]);
        assert_eq!(
            parse(&plan.bytes).unwrap().receptions[0].cumulative_lost,
            -2
        );
    }
    #[test]
    fn scheduler_randomizes_obeys_bandwidth_and_requires_safe_profile() {
        let r = reports(7);
        let early = r.interval(true, 0.0);
        let late = r.interval(true, 1.0);
        assert!(early < late);
        assert!(early >= Duration::from_millis(500));
        assert!(r.interval(false, 0.5) > r.interval(true, 0.5));
        assert_eq!(r.interval(false, f64::NAN), r.interval(false, 0.5));
        assert!(Reports::new(7, 1, 16000, "test".into()).is_err());
        assert!(Reports::new(7, 48000, 0, "test".into()).is_err());
        assert!(Reports::new(7, 48000, 16000, "".into()).is_err());
    }
    #[test]
    fn microsoft_standalone_and_profile_extensions() {
        fn hex(s: &str) -> Vec<u8> {
            (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2],16).unwrap()).collect()
        }
        assert!(parse(&hex("80c900010000000781ca0003000000070105746573743000")).is_ok(), "compound_rr_cname");
        assert!(parse(&hex("80c9000100000007")).is_ok(), "simple_rr");
        assert!(parse(&hex("80c80006000000070000000000000000000000000000000000000000")).is_ok(), "simple_sr");
        assert!(parse(&hex("81ca0003000000070105746573743000")).is_ok(), "simple_sdes");
        assert!(parse(&hex("81cb000100000007")).is_ok(), "simple_bye");
        assert!(parse(&hex("80c90004000000070008000c000000000001f40081ca0003000000070105746573743000")).is_ok(), "rr_profile_extension_cname");
        for length in [0u16, 2, 5, 100] {
            let mut packet = hex("80c90004000000070008000c000000000001f400");
            packet[10..12].copy_from_slice(&length.to_be_bytes());
            assert!(parse(&packet).is_err());
        }
        let mut too_many = hex("80c9001600000007");
        for _ in 0..21 { too_many.extend([0, 99, 0, 4]); }
        assert!(parse(&too_many).is_err());
        let mut compound_without_cname = hex("80c9000100000007");
        compound_without_cname.extend(hex("81cb000100000007"));
        assert!(parse(&compound_without_cname).is_err());
    }

}
