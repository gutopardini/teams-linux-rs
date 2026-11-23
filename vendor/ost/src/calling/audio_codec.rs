//! Own codec boundary. Opus uses the system library, with one owned state per
//! stream. Native PCM remains mono8kHz/20ms; Opus RTP always clocks at48kHz.
use super::rtp;
use anyhow::{bail, Result};
use std::{
    ffi::{c_int, c_void},
    sync::{Arc, OnceLock},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Pcmu,
    Opus,
}
impl Codec {
    pub fn payload_type(self) -> u8 {
        match self {
            Self::Pcmu => 0,
            Self::Opus => 111,
        }
    }
    pub fn clock_rate(self) -> u32 {
        match self {
            Self::Pcmu => 8000,
            Self::Opus => 48000,
        }
    }
    pub fn timestamp_increment(self) -> u32 {
        self.clock_rate() / 50
    }
    /// PCM produced or consumed for this codec. Opus RTP stays at 48 kHz, and
    /// the decoded PCM now matches that clock. PCMU capture and transmission
    /// stay at 8 kHz.
    pub fn pcm_rate(self) -> u32 {
        match self {
            Self::Pcmu => 8000,
            Self::Opus => 48000,
        }
    }
    pub fn pcm_frame_samples(self) -> usize {
        (self.pcm_rate() / 50) as usize
    }
}
#[derive(Clone, Copy, Debug)]
pub struct CodecSet {
    pub send: Codec,
    pub pcmu: bool,
    pub opus: bool,
    pub max_bitrate: Option<u32>,
}
impl CodecSet {
    pub fn single(codec: Codec) -> Self {
        Self {
            send: codec,
            pcmu: codec == Codec::Pcmu,
            opus: codec == Codec::Opus,
            max_bitrate: None,
        }
    }
    pub fn restrict(mut self, opus: bool) -> Option<Self> {
        self.opus &= opus;
        if !self.pcmu && !self.opus {
            return None;
        }
        if self.send == Codec::Opus && !self.opus {
            self.send = Codec::Pcmu;
        }
        Some(self)
    }
}
type CreateEncoder = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type CreateDecoder = unsafe extern "C" fn(i32, c_int, *mut c_int) -> *mut c_void;
type Encode = unsafe extern "C" fn(*mut c_void, *const i16, c_int, *mut u8, i32) -> i32;
type Decode = unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut i16, c_int, c_int) -> c_int;
type PacketSamples = unsafe extern "C" fn(*const u8, i32, i32) -> i32;
type Control = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);
struct Api {
    _library: libloading::Library,
    create_encoder: CreateEncoder,
    create_decoder: CreateDecoder,
    encode: Encode,
    control: Control,
    decode: Decode,
    packet_samples: PacketSamples,
    destroy_encoder: Destroy,
    destroy_decoder: Destroy,
}
fn api() -> Option<Arc<Api>> {
    static API: OnceLock<Option<Arc<Api>>> = OnceLock::new();
    API.get_or_init(|| {
        // Symbols and their signatures are from the stable public libopus C API.
        let library = unsafe { libloading::Library::new("libopus.so.0") }.ok()?;
        unsafe {
            Some(Arc::new(Api {
                create_encoder: *library.get(b"opus_encoder_create\0").ok()?,
                create_decoder: *library.get(b"opus_decoder_create\0").ok()?,
                encode: *library.get(b"opus_encode\0").ok()?,
                control: *library.get(b"opus_encoder_ctl\0").ok()?,
                decode: *library.get(b"opus_decode\0").ok()?,
                packet_samples: *library.get(b"opus_packet_get_nb_samples\0").ok()?,
                destroy_encoder: *library.get(b"opus_encoder_destroy\0").ok()?,
                destroy_decoder: *library.get(b"opus_decoder_destroy\0").ok()?,
                _library: library,
            }))
        }
    })
    .clone()
}
pub fn opus_available() -> bool {
    api().is_some()
}
struct State {
    pointer: *mut c_void,
    api: Arc<Api>,
    encoder: bool,
}
// The state is uniquely owned by one send/receive task and never shared. libopus
// permits moving a state between threads; concurrent access is not performed.
unsafe impl Send for State {}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            if self.encoder {
                (self.api.destroy_encoder)(self.pointer)
            } else {
                (self.api.destroy_decoder)(self.pointer)
            }
        }
    }
}
pub struct Encoder {
    codec: Codec,
    state: Option<State>,
}
pub struct Decoder {
    codec: Codec,
    state: Option<State>,
    last_sample: i16,
}
impl Encoder {
    pub fn new(codec: Codec) -> Result<Self> {
        let state = if codec == Codec::Opus {
            let api = api().ok_or_else(|| anyhow::anyhow!("Opus library unavailable"))?;
            let mut error = 0;
            let pointer = unsafe { (api.create_encoder)(8000, 1, 2048, &mut error) };
            if pointer.is_null() || error != 0 {
                bail!("Opus encoder unavailable");
            }
            Some(State {
                pointer,
                api,
                encoder: true,
            })
        } else {
            None
        };
        Ok(Self { codec, state })
    }
    pub fn limit_bitrate(&mut self, limit: Option<u32>) -> Result<()> {
        if let Some(state) = &self.state {
            let bitrate = limit.unwrap_or(16000).clamp(6000, 16000);
            if unsafe { (state.api.control)(state.pointer, 4002, bitrate as c_int) } != 0 {
                bail!("Opus bitrate unavailable");
            }
        }
        Ok(())
    }
    pub fn codec(&self) -> Codec {
        self.codec
    }
    pub fn encode(&mut self, samples: Option<Vec<i16>>, muted: bool) -> Result<Vec<u8>> {
        let mut frame = [0i16; 160];
        if !muted {
            if let Some(samples) = samples {
                for (output, input) in frame.iter_mut().zip(samples) {
                    *output = input;
                }
            }
        }
        match &self.state {
            None => Ok(frame
                .iter()
                .map(|sample| rtp::linear_to_ulaw(*sample))
                .collect()),
            Some(state) => {
                let mut output = vec![0; 1275];
                let size = unsafe {
                    (state.api.encode)(
                        state.pointer,
                        frame.as_ptr(),
                        160,
                        output.as_mut_ptr(),
                        output.len() as i32,
                    )
                };
                if size <= 0 || size as usize > output.len() {
                    bail!("Opus encode failed");
                }
                output.truncate(size as usize);
                Ok(output)
            }
        }
    }
}
impl Decoder {
    pub fn new(codec: Codec) -> Result<Self> {
        let state = if codec == Codec::Opus {
            let api = api().ok_or_else(|| anyhow::anyhow!("Opus library unavailable"))?;
            let mut error = 0;
            let pointer = unsafe { (api.create_decoder)(48_000, 1, &mut error) };
            if pointer.is_null() || error != 0 {
                bail!("Opus decoder unavailable");
            }
            Some(State {
                pointer,
                api,
                encoder: false,
            })
        } else {
            None
        };
        Ok(Self {
            codec,
            state,
            last_sample: 0,
        })
    }
    pub fn codec(&self) -> Codec {
        self.codec
    }
    /// Inspect duration without mutating the stateful decoder. PCMU is one
    /// 20 ms frame. Opus accepts 2.5/5/10/20 ms at the 48 kHz output clock.
    pub fn packet_samples(codec: Codec, payload: &[u8]) -> Result<usize> {
        if codec == Codec::Pcmu {
            if payload.len() != 160 {
                bail!("PCMU packet duration unsupported");
            }
            return Ok(160);
        }
        if payload.is_empty() || payload.len() > 1275 {
            bail!("Opus packet invalid");
        }
        let api = api().ok_or_else(|| anyhow::anyhow!("Opus library unavailable"))?;
        // Stable libopus API, bounded nonempty input slice valid for this call.
        let count = unsafe { (api.packet_samples)(payload.as_ptr(), payload.len() as i32, 48_000) };
        if count <= 0 || count > 960 || count % 120 != 0 {
            bail!("Opus packet duration unsupported");
        }
        Ok(count as usize)
    }
    /// Opus PLC uses the public null-data decoder contract. PCMU uses a short
    /// fade of the last sample to zero; this is not G.711 Appendix I concealment.
    pub fn conceal(&mut self, count: usize) -> Result<Vec<i16>> {
        let valid = if self.codec == Codec::Opus {
            count > 0 && count <= 960 && count % 120 == 0
        } else {
            count > 0 && count <= 160 && count % 20 == 0
        };
        if !valid {
            bail!("Invalid audio gap duration");
        }
        let mut output = vec![0; count];
        if let Some(state) = &self.state {
            let decoded = unsafe {
                (state.api.decode)(
                    state.pointer,
                    std::ptr::null(),
                    0,
                    output.as_mut_ptr(),
                    count as i32,
                    0,
                )
            };
            if decoded != count as i32 {
                bail!("Opus concealment failed");
            }
        } else {
            for (index, sample) in output.iter_mut().enumerate() {
                *sample =
                    (self.last_sample as i32 * (count - index - 1) as i32 / count as i32) as i16;
            }
        }
        self.last_sample = output.last().copied().unwrap_or(0);
        Ok(output)
    }
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>> {
        let expected = Self::packet_samples(self.codec, payload)?;
        let output = self.decode_packet(payload)?;
        if output.len() != expected {
            bail!("Unexpected audio packet duration");
        }
        self.last_sample = output.last().copied().unwrap_or(0);
        Ok(output)
    }
    fn decode_packet(&mut self, payload: &[u8]) -> Result<Vec<i16>> {
        match &self.state {
            None => {
                if payload.len() != 160 {
                    bail!("PCMU packet duration unsupported");
                }
                Ok(payload
                    .iter()
                    .map(|value| rtp::ulaw_to_linear(*value))
                    .collect())
            }
            Some(state) => {
                if payload.is_empty() || payload.len() > 1275 {
                    bail!("Opus packet invalid");
                }
                let mut output = vec![0i16; 960];
                let count = unsafe {
                    (state.api.decode)(
                        state.pointer,
                        payload.as_ptr(),
                        payload.len() as i32,
                        output.as_mut_ptr(),
                        960,
                        0,
                    )
                };
                if count <= 0 || count > 960 || count % 120 != 0 {
                    bail!("Opus packet duration unsupported");
                }
                output.truncate(count as usize);
                Ok(output)
            }
        }
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn fixture_frames(frames: &[Vec<i16>]) -> Vec<Vec<u8>> {
        let encoder = Encoder::new(Codec::Opus).unwrap();
        let state = encoder.state.as_ref().unwrap();
        frames
            .iter()
            .map(|frame| {
                let mut payload = vec![0; 1275];
                let size = unsafe {
                    (state.api.encode)(
                        state.pointer,
                        frame.as_ptr(),
                        frame.len() as i32,
                        payload.as_mut_ptr(),
                        1275,
                    )
                };
                assert!(size > 0);
                payload.truncate(size as usize);
                payload
            })
            .collect()
    }
    #[test]
    fn concealment_uses_bounded_opus_plc_or_pcmu_fade_and_recovers() {
        for codec in [Codec::Pcmu, Codec::Opus] {
            if codec == Codec::Opus && !opus_available() {
                continue;
            }
            let mut encoder = Encoder::new(codec).unwrap();
            let mut decoder = Decoder::new(codec).unwrap();
            let frame = if codec == Codec::Opus { 960 } else { 160 };
            for _ in 0..5 {
                let payload = encoder.encode(Some(vec![9000; 160]), false).unwrap();
                assert_eq!(Decoder::packet_samples(codec, &payload).unwrap(), frame);
                decoder.decode(&payload).unwrap();
            }
            let accepted: &[usize] = if codec == Codec::Opus {
                &[120, 240, 480, 960]
            } else {
                &[20, 40, 80, 160]
            };
            let rejected: &[usize] = if codec == Codec::Opus {
                &[0, 20, 160, 1000]
            } else {
                &[0, 19, 161, 1000]
            };
            for count in accepted {
                assert_eq!(decoder.conceal(*count).unwrap().len(), *count);
            }
            for count in rejected {
                assert!(decoder.conceal(*count).is_err());
            }
            let payload = encoder.encode(Some(vec![-9000; 160]), false).unwrap();
            assert_eq!(decoder.decode(&payload).unwrap().len(), frame);
        }
    }
    #[test]
    fn pcmu_codec_keeps_existing_mute_and_frame_contract() {
        let mut encoder = Encoder::new(Codec::Pcmu).unwrap();
        let payload = encoder.encode(Some(vec![9000; 160]), true).unwrap();
        assert_eq!(payload, vec![255; 160]);
        let mut decoder = Decoder::new(Codec::Pcmu).unwrap();
        assert!(decoder.decode(&[0; 159]).is_err());
        assert_eq!(decoder.decode(&payload).unwrap(), vec![0; 160]);
    }
    #[test]
    fn opus_accepts_short_frames_and_negotiated_low_bitrate_without_exceeding_buffers() {
        if !opus_available() {
            return;
        }
        let mut encoder = Encoder::new(Codec::Opus).unwrap();
        encoder.limit_bitrate(Some(6000)).unwrap();
        let state = encoder.state.as_ref().unwrap();
        let mut output = [0u8; 1275];
        let frame = [4000i16; 80];
        let size = unsafe {
            (state.api.encode)(state.pointer, frame.as_ptr(), 80, output.as_mut_ptr(), 1275)
        };
        assert!(size > 0);
        let mut decoder = Decoder::new(Codec::Opus).unwrap();
        assert_eq!(decoder.decode(&output[..size as usize]).unwrap().len(), 480);
    }
    #[test]
    fn opus_keeps_owned_states_mute_and_48khz_rtp_clock() {
        if !opus_available() {
            return;
        }
        let mut encoder = Encoder::new(Codec::Opus).unwrap();
        let mut decoder = Decoder::new(Codec::Opus).unwrap();
        let mut peak = 0i16;
        for index in 0..10 {
            let frame = (0..160)
                .map(|sample| {
                    ((2.0 * std::f32::consts::PI * 440.0 * (index * 160 + sample) as f32 / 8000.0)
                        .sin()
                        * 9000.0) as i16
                })
                .collect();
            let payload = encoder.encode(Some(frame), false).unwrap();
            let decoded = decoder.decode(&payload).unwrap();
            assert_eq!(decoded.len(), 960);
            peak = peak.max(decoded.iter().map(|v| v.abs()).max().unwrap());
        }
        assert!(peak > 3000);
        assert_eq!(Codec::Opus.timestamp_increment(), 960);
        assert_eq!(Codec::Pcmu.timestamp_increment(), 160);
        for _ in 0..30 {
            let payload = encoder.encode(Some(vec![9000; 160]), true).unwrap();
            let decoded = decoder.decode(&payload).unwrap();
            peak = decoded.iter().map(|v| v.abs()).max().unwrap();
        }
        assert!(peak < 100);
        assert!(decoder.decode(&[]).is_err());
        assert!(decoder.decode(&vec![0; 1276]).is_err());
    }
    #[test]
    fn opus_48k_decode_keeps_a_6khz_tone_that_8k_playback_cannot_represent() {
        if !opus_available() {
            return;
        }
        let mut previous = Vec::new();
        let mut decoded = Vec::new();
        let mut decoder = Decoder::new(Codec::Opus).unwrap();
        for index in 0..6 {
            let frame: Vec<i16> = (0..960)
                .map(|sample| {
                    let n = index * 960 + sample;
                    ((2.0 * std::f32::consts::PI * 6000.0 * n as f32 / 48000.0).sin() * 12000.0) as i16
                })
                .collect();
            let payload = opus_encode_at(48_000, &frame);
            assert_eq!(Decoder::packet_samples(Codec::Opus, &payload).unwrap(), 960);
            previous = decoded;
            decoded = decoder.decode(&payload).unwrap();
        }
        assert_eq!(decoded.len(), 960);
        let crossings = zero_crossings(&decoded);
        assert!((200..280).contains(&crossings), "crossings {crossings}");
        let peak = decoded.iter().map(|sample| sample.abs()).max().unwrap();
        let reference = previous.iter().map(|sample| sample.abs()).max().unwrap().max(1);
        let ratio = peak as f32 / reference as f32;
        assert!((0.89..1.12).contains(&ratio), "peak {peak} reference {reference}");
        assert!(peak > 3000, "peak {peak}");
    }
    fn opus_encode_at(rate: i32, pcm: &[i16]) -> Vec<u8> {
        let api = api().expect("libopus");
        let mut error = 0;
        let pointer = unsafe { (api.create_encoder)(rate, 1, 2048, &mut error) };
        assert!(!pointer.is_null() && error == 0);
        let mut output = vec![0u8; 1275];
        let size = unsafe {
            (api.encode)(
                pointer,
                pcm.as_ptr(),
                pcm.len() as i32,
                output.as_mut_ptr(),
                1275,
            )
        };
        unsafe { (api.destroy_encoder)(pointer) };
        assert!(size > 0);
        output.truncate(size as usize);
        output
    }
    fn zero_crossings(samples: &[i16]) -> usize {
        samples.windows(2).filter(|pair| (pair[0] ^ pair[1]) < 0).count()
    }
}
