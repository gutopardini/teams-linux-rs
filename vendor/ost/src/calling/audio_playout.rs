//! Own bounded playout: authenticated RTP in, paced 20 ms PCM out.
//! PCMU stays mono 8 kHz. Opus is decoded and mixed at mono 48 kHz.
//! No network/device access. Callers drive this at 20 ms with a monotonic clock.
use super::{
    audio_codec::{Codec, CodecSet, Decoder},
    rtp::{RtpPacket, RtpQualityStats, RtpQualityTracker},
};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
const PREFILL: Duration = Duration::from_millis(60);
const WINDOW_MS: u32 = 120;
const FRAME: usize = 160;
const MAX_STREAMS: usize = 8;
const MAX_PACKETS: usize = 64;
const INTERVAL: Duration = Duration::from_millis(20);
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutStats {
    /// Authenticated, decoded RTP reaching playout, including rejected formats.
    pub observed_packets: u64,
    pub unsupported_payload_packets: u64,
    pub invalid_packet_samples: u64,
    pub stream_limit_packets: u64,
    pub last_payload_type: u8,
    pub last_payload_bytes: usize,
    pub rendered_frames: u64,
    /// Cumulative signal levels, no audio samples or source identities retained.
    pub decoded_samples: u64,
    pub decoded_energy: u64,
    pub decoded_peak: u32,
    pub decoded_at_limit: u64,
    pub mix_peak: u32,
    pub mix_clipped_samples: u64,
    pub contributing_streams: usize,
    pub concealed_samples: u64,
    pub late_packets: u64,
    pub duplicate_packets: u64,
    pub dropped_packets: u64,
    pub decoder_errors: u64,
    pub timeline_resets: u64,
    pub speaker_dropped_frames: u64,
    /// Device-clock silence during prefill or inactive talkspurts; not PLC.
    pub device_silence_frames: u64,
    pub buffered_packets: usize,
    pub active_streams: usize,
}
struct Packet {
    sequence: u16,
    timestamp: u32,
    samples: usize,
    payload: Vec<u8>,
}
struct Stream {
    codec: Codec,
    decoder: Decoder,
    quality: RtpQualityTracker,
    queue: Vec<Packet>,
    pcm: VecDeque<i16>,
    cursor: Option<u32>,
    played_until: Option<u32>,
    due: Duration,
    started: bool,
    missing: usize,
    last_arrival: Duration,
    upsampler: RateUpsampler,
}
impl Stream {
    fn new(codec: Codec, now: Duration) -> anyhow::Result<Self> {
        let mut quality = RtpQualityTracker::default();
        quality.set_clock_rate(codec.clock_rate());
        Ok(Self {
            codec,
            decoder: Decoder::new(codec)?,
            quality,
            queue: Vec::new(),
            pcm: VecDeque::new(),
            cursor: None,
            played_until: None,
            due: now + PREFILL,
            started: false,
            missing: 0,
            last_arrival: now,
            upsampler: RateUpsampler::new(),
        })
    }
    fn frame(&self) -> usize {
        self.codec.pcm_frame_samples()
    }
    fn step(&self) -> u32 {
        self.codec.clock_rate() / self.codec.pcm_rate()
    }
    fn reset(&mut self, timestamp: u32, now: Duration, stats: &mut PlayoutStats) -> bool {
        let Ok(decoder) = Decoder::new(self.codec) else {
            stats.decoder_errors += 1;
            return false;
        };
        self.decoder = decoder;
        self.queue.clear();
        self.pcm.clear();
        self.cursor = Some(timestamp);
        self.played_until = None;
        self.due = now + PREFILL;
        self.started = false;
        self.missing = 0;
        self.upsampler.reset();
        stats.timeline_resets += 1;
        true
    }
    fn push(
        &mut self,
        packet: &RtpPacket,
        samples: usize,
        now: Duration,
        stats: &mut PlayoutStats,
    ) {
        let max_ticks = self.codec.clock_rate() * WINDOW_MS / 1000;
        if self.cursor.is_none()
            && self
                .played_until
                .is_some_and(|floor| (packet.timestamp.wrapping_sub(floor) as i32) < 0)
        {
            stats.late_packets += 1;
            return;
        }
        // Validate against queued packets before changing the prefill origin.
        // A rejected duplicate/overlap must not rewind an existing timeline.
        if self
            .queue
            .iter()
            .any(|p| p.sequence == packet.sequence_number || p.timestamp == packet.timestamp)
        {
            stats.duplicate_packets += 1;
            return;
        }
        let step = self.step() as i64;
        if self.queue.len() >= MAX_PACKETS
            || self.queue.iter().any(|p| {
                let start = p.timestamp.wrapping_sub(packet.timestamp) as i32 as i64;
                let end = start + p.samples as i64 * step;
                start < samples as i64 * step && end > 0
            })
        {
            stats.dropped_packets += 1;
            return;
        }
        if let Some(cursor) = self.cursor {
            let distance = packet.timestamp.wrapping_sub(cursor) as i32;
            if distance.unsigned_abs() % self.step() != 0
                || distance.unsigned_abs() / self.step() % 20 != 0
            {
                stats.dropped_packets += 1;
                return;
            }
            if distance < 0 {
                // Reversed first packets may establish an earlier origin before
                // playback starts, without postponing the prefill deadline.
                if !self.started && distance.unsigned_abs() <= self.codec.clock_rate() * 60 / 1000 {
                    self.cursor = Some(packet.timestamp);
                } else {
                    stats.late_packets += 1;
                    return;
                }
            } else if distance as u32 > max_ticks {
                // DTX / timestamp discontinuity starts a fresh bounded talkspurt.
                if now.saturating_sub(self.last_arrival) >= PREFILL || packet.marker {
                    if !self.reset(packet.timestamp, now, stats) {
                        return;
                    }
                } else {
                    stats.dropped_packets += 1;
                    return;
                }
            }
        } else {
            self.cursor = Some(packet.timestamp);
            self.due = now + PREFILL;
        }
        let cursor = self.cursor.unwrap();
        let start = packet.timestamp.wrapping_sub(cursor) as i32;
        let end = start + samples as i32 * self.step() as i32;
        let before = self.queue.len();
        let step = self.step();
        self.queue.retain(|p| {
            p.timestamp
                .wrapping_sub(cursor)
                .saturating_add(p.samples as u32 * step)
                <= max_ticks
        });
        stats.dropped_packets += (before - self.queue.len()) as u64;
        if end as u32 > max_ticks {
            stats.dropped_packets += 1;
            return;
        }
        self.queue.push(Packet {
            sequence: packet.sequence_number,
            timestamp: packet.timestamp,
            samples,
            payload: packet.payload.clone(),
        });
        self.queue
            .sort_unstable_by_key(|p| p.timestamp.wrapping_sub(cursor));
        self.last_arrival = now;
    }
    fn render(&mut self, now: Duration, stats: &mut PlayoutStats) -> Option<Vec<i16>> {
        self.cursor?;
        if now < self.due {
            return None;
        }
        if now.saturating_sub(self.due) > Duration::from_millis(40) {
            // Scheduler stalls must not cause a burst of stale speech. Resume
            // from recent queued media after prefill rather than racing ticks.
            if let Some(packet) = self.queue.last() {
                let timestamp = packet.timestamp;
                let packet = self.queue.pop().unwrap();
                if !self.reset(timestamp, now, stats) {
                    return None;
                }
                self.queue.push(packet);
            } else {
                self.cursor = None;
                self.pcm.clear();
            }
            return None;
        }
        self.started = true;
        self.due += INTERVAL;
        let frame_len = self.frame();
        let mut frame = vec![0i16; frame_len];
        let mut position = 0;
        while position < frame_len {
            if !self.pcm.is_empty() {
                let count = (frame_len - position).min(self.pcm.len());
                for output in &mut frame[position..position + count] {
                    *output = self.pcm.pop_front().unwrap();
                }
                self.cursor = Some(
                    self.cursor
                        .unwrap()
                        .wrapping_add(count as u32 * self.step()),
                );
                self.played_until = self.cursor;
                position += count;
                continue;
            }
            let cursor = self.cursor.unwrap();
            while self
                .queue
                .first()
                .is_some_and(|p| (p.timestamp.wrapping_sub(cursor) as i32) < 0)
            {
                self.queue.remove(0);
                stats.late_packets += 1;
            }
            if self.queue.first().is_some_and(|p| p.timestamp == cursor) {
                let packet = self.queue.remove(0);
                match self.decoder.decode(&packet.payload) {
                    Ok(pcm) => {
                        stats.decoded_samples = stats.decoded_samples.saturating_add(pcm.len() as u64);
                        for &sample in &pcm {
                            let value = i64::from(sample);
                            stats.decoded_energy = stats.decoded_energy.saturating_add((value * value) as u64);
                            stats.decoded_peak = stats.decoded_peak.max(value.unsigned_abs() as u32);
                            stats.decoded_at_limit += u64::from(sample == i16::MIN || sample == i16::MAX);
                        }
                        self.pcm.extend(pcm);
                        self.missing = 0;
                        continue;
                    }
                    Err(_) => {
                        stats.decoder_errors += 1;
                        let count = packet.samples.min((3 * frame_len).saturating_sub(self.missing));
                        if count == 0 {
                            self.cursor = None;
                            self.pcm.clear();
                            self.queue.clear();
                            self.started = false;
                            return (position > 0).then_some(frame);
                        }
                        let pcm = self
                            .decoder
                            .conceal(count)
                            .unwrap_or_else(|_| vec![0; count]);
                        self.pcm.extend(pcm);
                        self.missing += count;
                        stats.concealed_samples += count as u64;
                        continue;
                    }
                }
            }
            if self.missing >= 3 * frame_len {
                // 60 ms is the concealment cap. A packet already queued inside
                // the window is the next talkspurt and must play; deleting it
                // cuts the word that resumes. An empty queue is silence.
                if let Some(timestamp) = self.queue.first().map(|packet| packet.timestamp) {
                    self.cursor = Some(timestamp);
                    self.missing = 0;
                    self.pcm.clear();
                    stats.timeline_resets += 1;
                    continue;
                }
                self.cursor = None;
                self.pcm.clear();
                self.queue.clear();
                self.started = false;
                return (position > 0).then_some(frame);
            }
            let gap = self
                .queue
                .first()
                .map(|p| p.timestamp.wrapping_sub(cursor) / self.step())
                .unwrap_or(frame_len as u32) as usize;
            let count = gap.min(frame_len - position).min(3 * frame_len - self.missing);
            if count == 0 || count % 20 != 0 {
                stats.dropped_packets += 1;
                self.cursor = None;
                return None;
            }
            let pcm = self.decoder.conceal(count).unwrap_or_else(|_| {
                stats.decoder_errors += 1;
                vec![0; count]
            });
            frame[position..position + count].copy_from_slice(&pcm);
            self.cursor = Some(cursor.wrapping_add(count as u32 * self.step()));
            self.played_until = self.cursor;
            position += count;
            self.missing += count;
            stats.concealed_samples += count as u64;
        }
        Some(frame)
    }
}
/// Each SSRC has its own decoder/timeline. Multiple active sources mix with
/// saturating PCM output; inactive sources are retired after2s. No shared Opus
/// state between people or unbounded allocation on SSRC churn.
pub struct AudioPlayout {
    streams: BTreeMap<u32, Stream>,
    codecs: CodecSet,
    stats: PlayoutStats,
    retired: RtpQualityStats,
}
impl AudioPlayout {
    pub fn new(codecs: CodecSet) -> Self {
        Self {
            streams: BTreeMap::new(),
            codecs,
            stats: PlayoutStats::default(),
            retired: RtpQualityStats::default(),
        }
    }
    fn clock(&self) -> u32 {
        if self.codecs.opus {
            48000
        } else {
            8000
        }
    }
    fn retire(&mut self, now: Duration) {
        let old = self
            .streams
            .iter()
            .filter(|(_, s)| now.saturating_sub(s.last_arrival) > Duration::from_secs(2))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in old {
            if let Some(stream) = self.streams.remove(&id) {
                let quality = stream.quality.stats();
                self.retired.estimated_lost_packets += quality.estimated_lost_packets;
                self.retired.reordered_packets += quality.reordered_packets;
                self.retired.duplicate_packets += quality.duplicate_packets;
            }
        }
    }
    pub fn push(&mut self, packet: &RtpPacket, now: Duration) -> bool {
        self.stats.observed_packets += 1;
        self.stats.last_payload_type = packet.payload_type;
        self.stats.last_payload_bytes = packet.payload.len();
        let codec = match packet.payload_type {
            0 if self.codecs.pcmu => Codec::Pcmu,
            111 if self.codecs.opus => Codec::Opus,
            _ => {
                self.stats.unsupported_payload_packets += 1;
                self.stats.dropped_packets += 1;
                return false;
            }
        };
        let Ok(samples) = Decoder::packet_samples(codec, &packet.payload) else {
            self.stats.invalid_packet_samples += 1;
            self.stats.dropped_packets += 1;
            return false;
        };
        self.retire(now);
        let change = self
            .streams
            .get(&packet.ssrc)
            .is_some_and(|s| s.codec != codec);
        if !self.streams.contains_key(&packet.ssrc) || change {
            if !change && self.streams.len() >= MAX_STREAMS {
                self.stats.stream_limit_packets += 1;
                self.stats.dropped_packets += 1;
                return false;
            }
            let Ok(stream) = Stream::new(codec, now) else {
                self.stats.decoder_errors += 1;
                return false;
            };
            if change {
                self.stats.timeline_resets += 1;
            }
            if let Some(old) = self.streams.insert(packet.ssrc, stream) {
                let quality = old.quality.stats();
                self.retired.estimated_lost_packets += quality.estimated_lost_packets;
                self.retired.reordered_packets += quality.reordered_packets;
                self.retired.duplicate_packets += quality.duplicate_packets;
            }
        }
        let stream = self.streams.get_mut(&packet.ssrc).unwrap();
        let arrival = (now.as_nanos() * codec.clock_rate() as u128 / 1_000_000_000) as u32;
        stream
            .quality
            .observe(packet.sequence_number, packet.timestamp, arrival);
        stream.push(packet, samples, now, &mut self.stats);
        true
    }
    pub fn render(&mut self, now: Duration) -> Option<Vec<i16>> {
        self.retire(now);
        let out_len = if self.codecs.opus { 960 } else { FRAME };
        let mut mix = vec![0i32; out_len];
        let mut active = false;
        self.stats.contributing_streams = 0;
        for stream in self.streams.values_mut() {
            if let Some(native) = stream.render(now, &mut self.stats) {
                let frame = if native.len() == out_len {
                    native
                } else if native.len() * 6 == out_len {
                    stream.upsampler.expand(&native)
                } else {
                    continue;
                };
                active = true;
                self.stats.contributing_streams += 1;
                for (output, sample) in mix.iter_mut().zip(frame) {
                    *output += sample as i32;
                }
            }
        }
        if !active {
            return None;
        }
        self.stats.rendered_frames += 1;
        for &sample in &mix {
            self.stats.mix_peak = self.stats.mix_peak.max(sample.unsigned_abs());
            self.stats.mix_clipped_samples += u64::from(sample < i16::MIN as i32 || sample > i16::MAX as i32);
        }
        Some(
            mix.into_iter()
                .map(|v| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect(),
        )
    }
    pub fn remove_source(&mut self, ssrc: u32) {
        if let Some(stream) = self.streams.remove(&ssrc) {
            let quality = stream.quality.stats();
            self.retired.estimated_lost_packets += quality.estimated_lost_packets;
            self.retired.reordered_packets += quality.reordered_packets;
            self.retired.duplicate_packets += quality.duplicate_packets;
        }
    }
    pub fn stats(&self) -> PlayoutStats {
        PlayoutStats {
            buffered_packets: self.streams.values().map(|s| s.queue.len()).sum(),
            active_streams: self.streams.len(),
            ..self.stats
        }
    }
    pub fn quality(&self) -> RtpQualityStats {
        let clock = self.clock();
        let mut result = self.retired;
        result.clock_rate = clock;
        for stream in self.streams.values() {
            let s = stream.quality.stats();
            result.estimated_lost_packets += s.estimated_lost_packets;
            result.reordered_packets += s.reordered_packets;
            result.duplicate_packets += s.duplicate_packets;
            result.jitter_ticks = result
                .jitter_ticks
                .max((s.jitter_ticks as u64 * clock as u64 / s.clock_rate.max(1) as u64) as u32);
        }
        result
    }
}

/// Integer 8 kHz → 48 kHz polyphase lowpass. State stays with the PCMU source
/// so block boundaries do not click. Coefficients are designed once.
struct RateUpsampler {
    history: [f32; 8],
    cursor: usize,
}
impl RateUpsampler {
    fn new() -> Self {
        Self { history: [0.0; 8], cursor: 0 }
    }
    fn reset(&mut self) {
        self.history = [0.0; 8];
        self.cursor = 0;
    }
    fn expand(&mut self, input: &[i16]) -> Vec<i16> {
        let phases = upsample_phases();
        let mut output = Vec::with_capacity(input.len() * 6);
        for &sample in input {
            self.history[self.cursor] = sample as f32;
            self.cursor = (self.cursor + 1) % self.history.len();
            for phase in phases {
                let mut acc = 0.0f32;
                let mut pos = self.cursor;
                for &coefficient in phase {
                    pos = if pos == 0 { self.history.len() - 1 } else { pos - 1 };
                    acc += coefficient * self.history[pos];
                }
                output.push(acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            }
        }
        output
    }
}
fn upsample_phases() -> &'static [[f32; 8]; 6] {
    use std::sync::OnceLock;
    static PHASES: OnceLock<[[f32; 8]; 6]> = OnceLock::new();
    PHASES.get_or_init(|| {
        const TAPS: usize = 48;
        let mut prototype = [0f32; TAPS];
        let mid = (TAPS - 1) as f64 / 2.0;
        let cutoff = 0.5 / 6.0;
        for (index, value) in prototype.iter_mut().enumerate() {
            let x = index as f64 - mid;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
            };
            let angle = 2.0 * std::f64::consts::PI * index as f64 / (TAPS - 1) as f64;
            let window = 0.42 - 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
            *value = (sinc * window) as f32;
        }
        let sum: f32 = prototype.iter().sum();
        let scale = 6.0 / sum;
        for value in &mut prototype {
            *value *= scale;
        }
        let mut phases = [[0f32; 8]; 6];
        for phase in 0..6 {
            for tap in 0..8 {
                phases[phase][tap] = prototype[tap * 6 + phase];
            }
        }
        phases
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quality_levels_distinguish_decoded_signal_from_mix_clipping() {
        let mut m=mixer();
        for source in [1,2] { assert!(m.push(&packet(0,0,source,24000),ms(0))); }
        let frame=m.render(ms(60)).unwrap();
        let s=m.stats();
        let decoded=pcm(24000);
        assert_eq!(s.decoded_samples,320);
        assert_eq!(s.decoded_energy,320 * (i64::from(decoded)*i64::from(decoded)) as u64);
        assert_eq!(s.decoded_peak,decoded.unsigned_abs() as u32);
        assert_eq!(s.decoded_at_limit,0);
        assert_eq!(s.mix_peak,2 * decoded.unsigned_abs() as u32);
        assert_eq!(s.mix_clipped_samples,160);
        assert_eq!(s.contributing_streams,2);
        assert_eq!(frame,vec![i16::MAX;160]);
        let mut silent=mixer();
        silent.push(&packet(0,0,1,0),ms(0));silent.render(ms(60));
        assert_eq!(silent.stats().decoded_energy,0);
        assert_eq!(silent.stats().mix_clipped_samples,0);
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    fn packet(seq: u16, ts: u32, ssrc: u32, value: i16) -> RtpPacket {
        super::super::rtp::decode(&super::super::rtp::encode(
            0,
            seq,
            ts,
            ssrc,
            &vec![super::super::rtp::linear_to_ulaw(value); 160],
        ))
        .unwrap()
    }
    fn pcm(value: i16) -> i16 {
        super::super::rtp::ulaw_to_linear(super::super::rtp::linear_to_ulaw(value))
    }
    fn mixer() -> AudioPlayout {
        AudioPlayout::new(CodecSet::single(Codec::Pcmu))
    }
    #[test]
    fn pcmu_upsampler_holds_dc_and_does_not_click_between_blocks() {
        let mut upsampler = RateUpsampler::new();
        let mut previous = upsampler.expand(&[8_000; 160]);
        assert_eq!(previous.len(), 960);
        for _ in 0..6 {
            let block = upsampler.expand(&[8_000; 160]);
            let jump = (i32::from(block[0]) - i32::from(*previous.last().unwrap())).unsigned_abs();
            assert!(jump < 400, "boundary jump {jump}");
            previous = block;
        }
        let mean = previous.iter().map(|sample| i64::from(*sample)).sum::<i64>() / previous.len() as i64;
        assert!((7_600..8_400).contains(&mean), "mean {mean}");
    }
    #[test]
    fn reorder_including_reversed_first_packet_plays_in_order_after_bounded_prefill() {
        let mut m = mixer();
        m.push(&packet(2, 320, 7, 9000), ms(0));
        m.push(&packet(0, 0, 7, 3000), ms(10));
        m.push(&packet(1, 160, 7, 6000), ms(20));
        assert!(m.render(ms(59)).is_none());
        for (time, value) in [(60, 3000), (80, 6000), (100, 9000)] {
            assert_eq!(m.render(ms(time)).unwrap(), vec![pcm(value); 160]);
        }
        assert_eq!(m.stats().concealed_samples, 0);
        assert_eq!(m.quality().estimated_lost_packets, 0);
        assert!(m.quality().reordered_packets >= 1);
    }
    #[test]
    fn duplicate_sequence_timestamp_wrap_and_late_packet_never_replay_speech() {
        let mut m = mixer();
        let a = packet(u16::MAX, u32::MAX - 159, 7, 6000);
        m.push(&a, ms(0));
        m.push(&a, ms(1));
        m.push(&packet(0, 0, 7, 9000), ms(20));
        assert_eq!(m.render(ms(60)).unwrap(), vec![pcm(6000); 160]);
        assert_eq!(m.render(ms(80)).unwrap(), vec![pcm(9000); 160]);
        m.push(&a, ms(90));
        assert_eq!(m.stats().duplicate_packets, 1);
        assert_eq!(m.stats().late_packets, 1);
        assert_eq!(m.quality().estimated_lost_packets, 0);
    }
    #[test]
    fn rejected_reversed_duplicate_or_overlap_cannot_rewind_prefill() {
        let mut m = mixer();
        m.push(&packet(10, 160, 7, 9000), ms(0));
        m.push(&packet(10, 0, 7, 3000), ms(1));
        m.push(&packet(9, 80, 7, 6000), ms(2));
        assert_eq!(m.render(ms(60)).unwrap(), vec![pcm(9000); 160]);
        assert_eq!(m.stats().duplicate_packets, 1);
        assert_eq!(m.stats().dropped_packets, 1);
        assert_eq!(m.stats().concealed_samples, 0);
    }
    #[test]
    fn missing_packet_fades_then_recovers_and_dtx_stops_after_three_frames() {
        let mut m = mixer();
        m.push(&packet(10, 0, 7, 9000), ms(0));
        m.push(&packet(12, 320, 7, -9000), ms(40));
        m.render(ms(60)).unwrap();
        let gap = m.render(ms(80)).unwrap();
        assert!(gap[0] > 0);
        assert_eq!(gap[159], 0);
        assert_eq!(m.render(ms(100)).unwrap(), vec![pcm(-9000); 160]);
        for time in [120, 140, 160] {
            m.render(ms(time)).unwrap();
        }
        assert!(m.render(ms(180)).is_none());
        assert!(m.render(ms(200)).is_none());
        m.push(&packet(11, 160, 7, 9000), ms(201));
        assert!(m.render(ms(220)).is_none());
        m.push(&packet(13, 960, 7, 3000), ms(230));
        assert!(m.render(ms(289)).is_none());
        assert_eq!(m.render(ms(290)).unwrap(), vec![pcm(3000); 160]);
        assert_eq!(m.stats().concealed_samples, 640);
        assert_eq!(m.stats().late_packets, 1);
    }
    #[test]
    fn queued_packet_beyond_sixty_milliseconds_plays_instead_of_being_discarded() {
        let mut m = mixer();
        m.push(&packet(0, 0, 7, 3000), ms(0));
        m.push(&packet(5, 800, 7, 9000), ms(10));
        assert_eq!(m.render(ms(60)).unwrap(), vec![pcm(3000); 160]);
        for time in [80, 100, 120] {
            assert!(m.render(ms(time)).is_some());
        }
        assert_eq!(m.stats().concealed_samples, 480);
        assert_eq!(m.render(ms(140)).unwrap(), vec![pcm(9000); 160]);
        assert_eq!(m.stats().concealed_samples, 480);
        assert_eq!(m.stats().dropped_packets, 0);
        assert_eq!(m.stats().late_packets, 0);
        assert!(m.stats().timeline_resets >= 1);
    }
    #[test]
    fn multiple_ssrcs_mix_saturate_and_retire_without_unbounded_states() {
        let mut m = mixer();
        for ssrc in 0..8 {
            assert!(m.push(&packet(0, 0, ssrc, 12000), ms(0)));
        }
        assert!(!m.push(&packet(0, 0, 9, 12000), ms(1)));
        assert_eq!(m.render(ms(60)).unwrap(), vec![i16::MAX; 160]);
        assert_eq!(m.stats().active_streams, 8);
        assert!(m.push(&packet(1, 160, 9, -6000), ms(2100)));
        assert_eq!(m.stats().active_streams, 1);
        assert_eq!(m.render(ms(2160)).unwrap(), vec![pcm(-6000); 160]);
    }
    #[test]
    fn malformed_unknown_overlapping_and_far_future_packets_stay_bounded() {
        let mut m = mixer();
        let mut bad = packet(0, 0, 7, 1);
        bad.payload.pop();
        assert!(!m.push(&bad, ms(0)));
        assert_eq!(m.stats().invalid_packet_samples, 1);
        bad = packet(0, 0, 7, 1);
        bad.payload_type = 101;
        assert!(!m.push(&bad, ms(0)));
        assert_eq!(m.stats().observed_packets, 2);
        assert_eq!(m.stats().unsupported_payload_packets, 1);
        assert_eq!(m.stats().last_payload_type, 101);
        assert_eq!(m.stats().last_payload_bytes, 160);
        assert_eq!(m.stats().active_streams, 0);
        m.push(&packet(0, 0, 7, 3000), ms(0));
        m.push(&packet(1, 80, 7, 12000), ms(1)); // overlap
        for index in 1..1000 {
            m.push(&packet(index, index as u32 * 160, 7, 12000), ms(2));
        }
        assert!(m.stats().buffered_packets <= 6);
        assert!(m.stats().dropped_packets > 990);
        assert_eq!(m.render(ms(60)).unwrap(), vec![pcm(3000); 160]);
    }
    #[test]
    fn scheduler_stall_rebuffers_recent_media_without_catchup_burst() {
        let mut m = mixer();
        m.push(&packet(0, 0, 7, 3000), ms(0));
        m.push(&packet(1, 160, 7, 9000), ms(20));
        assert!(m.render(ms(200)).is_none());
        assert!(m.render(ms(259)).is_none());
        assert_eq!(m.render(ms(260)).unwrap(), vec![pcm(9000); 160]);
        assert!(m.render(ms(260)).is_none());
        assert_eq!(m.stats().timeline_resets, 1);
    }
    #[test]
    fn sustained_jitter_and_reordering_preserve_timeline_with_bounded_memory() {
        let mut m = mixer();
        let count = 5000u64;
        let mut arrivals = (0..count)
            .map(|n| {
                let jitter = if n % 2 == 0 { -15 } else { 15 };
                ((n * 20 + 50) as i64 + jitter, n)
            })
            .collect::<Vec<_>>();
        arrivals.sort_unstable();
        let mut next = 0;
        let mut played = 0;
        for now in (0..(100 + count * 20)).step_by(20) {
            while next < arrivals.len() && arrivals[next].0 <= now as i64 {
                let n = arrivals[next].1;
                m.push(
                    &packet(n as u16, n as u32 * 160, 7, 1000 + (n % 20) as i16 * 1000),
                    ms(now),
                );
                next += 1;
            }
            if let Some(frame) = m.render(ms(now)) {
                assert_eq!(frame, vec![pcm(1000 + (played % 20) as i16 * 1000); 160]);
                played += 1;
            }
            assert!(m.stats().buffered_packets <= 6);
        }
        assert_eq!(played, count);
        assert_eq!(m.stats().concealed_samples, 0);
        assert_eq!(m.stats().late_packets, 0);
        assert!(m.quality().reordered_packets > 0);
    }
    #[test]
    fn long_timestamp_discontinuity_is_a_new_talkspurt_not_minutes_of_plc() {
        let mut m = mixer();
        m.push(&packet(0, 0, 7, 3000), ms(0));
        m.render(ms(60)).unwrap();
        m.push(&packet(1, 80000, 7, -6000), ms(100));
        assert!(m.render(ms(159)).is_none());
        assert_eq!(m.render(ms(160)).unwrap(), vec![pcm(-6000); 160]);
        assert_eq!(m.stats().timeline_resets, 1);
        assert_eq!(m.stats().concealed_samples, 0);
    }
    #[test]
    fn corrupt_opus_payload_cannot_extend_concealment_indefinitely() {
        if !super::super::audio_codec::opus_available() {
            return;
        }
        let bad = (0..=255)
            .find_map(|byte| {
                let payload = vec![byte];
                Decoder::packet_samples(Codec::Opus, &payload)
                    .ok()
                    .and_then(|samples| {
                        Decoder::new(Codec::Opus)
                            .unwrap()
                            .decode(&payload)
                            .is_err()
                            .then_some((payload, samples))
                    })
            })
            .expect("an invalid Opus frame with a valid duration header");
        let mut m = AudioPlayout::new(CodecSet::single(Codec::Opus));
        let samples = bad.1;
        let packets = 3 * 960 / samples;
        for index in 0..packets {
            let packet = super::super::rtp::decode(&super::super::rtp::encode(
                111,
                index as u16,
                index as u32 * samples as u32,
                7,
                &bad.0,
            ))
            .unwrap();
            m.push(&packet, ms(0));
        }
        for time in [60, 80, 100] {
            m.render(ms(time)).unwrap();
        }
        let concealed = m.stats().concealed_samples;
        assert_eq!(concealed, 2880);
        for time in [120, 140, 160, 180] {
            assert!(m.render(ms(time)).is_none());
        }
        assert_eq!(m.stats().concealed_samples, concealed);
        assert!(m.stats().decoder_errors > 0);
    }
    #[test]
    fn opus_decoders_are_per_ssrc_and_short_frames_coalesce_before_device_output() {
        if !super::super::audio_codec::opus_available() {
            return;
        }
        let mut m = AudioPlayout::new(CodecSet::single(Codec::Opus));
        let mut expected = vec![0i32; 960];
        for ssrc in [1, 2] {
            let frames = vec![
                vec![ssrc as i16 * 3000; 80],
                vec![ssrc as i16 * 3000; 40],
                vec![ssrc as i16 * 3000; 40],
            ];
            let payloads = super::super::audio_codec::tests::fixture_frames(&frames);
            let mut decoder = Decoder::new(Codec::Opus).unwrap();
            let mut offset = 0;
            for (index, payload) in payloads.into_iter().enumerate() {
                let count = Decoder::packet_samples(Codec::Opus, &payload).unwrap();
                let decoded = decoder.decode(&payload).unwrap();
                for (output, sample) in expected[offset..offset + count].iter_mut().zip(decoded) {
                    *output += sample as i32;
                }
                let packet = super::super::rtp::decode(&super::super::rtp::encode(
                    111,
                    index as u16,
                    offset as u32,
                    ssrc,
                    &payload,
                ))
                .unwrap();
                m.push(&packet, ms(index as u64 * 5));
                offset += count;
            }
        }
        let frame = m.render(ms(60)).unwrap();
        assert_eq!(
            frame,
            expected
                .into_iter()
                .map(|v| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect::<Vec<_>>()
        );
        assert_eq!(m.stats().concealed_samples, 0);
        assert_eq!(m.quality().clock_rate, 48000);
    }
}
