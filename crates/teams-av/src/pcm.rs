//! Bounded PCM I/O for application-owned devices. WebRTC decodes each remote
//! stream and its native mixer combines the 10 ms frames for one speaker clock.
use crate::{
    error::{Error, Result},
    rtc::{
        audio_frame::AudioFrame,
        audio_source::native::NativeAudioSource,
        audio_stream::native::NativeAudioStream,
        audio_track::RtcAudioTrack,
        native::audio_mixer::{AudioMixer, AudioMixerSource},
    },
};
use futures_util::{FutureExt, StreamExt};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc as std_mpsc,
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};

const PLAYBACK_RATE: u32 = 48_000;
const PLAYBACK_SAMPLES: usize = 480;
const MAX_QUEUED_FRAMES: usize = 6;
const MAX_TRACKS: usize = 16;
type Frames = Arc<Mutex<VecDeque<Vec<i16>>>>;

struct GateState {
    enabled: bool,
    revision: u64,
}

/// The UI can cut the native track synchronously; stale queued microphone
/// frames are discarded before a new unmute revision is accepted.
#[derive(Clone)]
pub struct MicrophoneGate {
    track: RtcAudioTrack,
    source: NativeAudioSource,
    state: Arc<Mutex<GateState>>,
}
impl MicrophoneGate {
    pub(crate) fn new(track: RtcAudioTrack, source: NativeAudioSource) -> Self {
        track.set_enabled(false);
        Self {
            track,
            source,
            state: Arc::new(Mutex::new(GateState {
                enabled: false,
                revision: 0,
            })),
        }
    }
    pub fn set_enabled(&self, enabled: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(revision) = state.revision.checked_add(1) else {
            state.enabled = false;
            self.track.set_enabled(false);
            self.source.clear_buffer();
            return false;
        };
        state.revision = revision;
        if !enabled {
            self.track.set_enabled(false);
        }
        self.source.clear_buffer();
        if enabled {
            self.track.set_enabled(true);
        }
        let applied = self.track.enabled() == enabled;
        state.enabled = enabled && applied;
        applied
    }
    pub fn enabled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enabled
    }
    fn revision(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revision
    }
    fn send(&self, samples: Vec<i16>, revision: u64) -> Result<()> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.enabled || state.revision != revision {
            return Ok(());
        }
        let sample_rate = self.source.sample_rate();
        let frame = AudioFrame {
            samples_per_channel: samples.len() as u32,
            data: samples.into(),
            sample_rate,
            num_channels: 1,
        };
        // The source is configured for direct 10 ms frames, which complete
        // synchronously. Never wait on a future while holding the mute lock.
        match self.source.capture_frame(&frame).now_or_never() {
            Some(Ok(())) => Ok(()),
            _ => Err(Error::Engine),
        }
    }
}

struct MixerInput {
    id: i32,
    frames: Frames,
}
impl AudioMixerSource for MixerInput {
    fn ssrc(&self) -> i32 {
        self.id
    }
    fn preferred_sample_rate(&self) -> u32 {
        PLAYBACK_RATE
    }
    fn get_audio_frame_with_info(&self, sample_rate: u32) -> Option<AudioFrame<'_>> {
        if sample_rate != PLAYBACK_RATE {
            return None;
        }
        let samples = self
            .frames
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()?;
        if samples.len() != PLAYBACK_SAMPLES {
            return None;
        }
        Some(AudioFrame {
            data: samples.into(),
            sample_rate,
            num_channels: 1,
            samples_per_channel: PLAYBACK_SAMPLES as u32,
        })
    }
}
enum MixerCommand {
    Add(MixerInput),
    Remove(i32),
}

/// Silence keeps the device open only before any remote track exists. With a
/// track attached, an empty queue waits for the decoded frame.
fn playback_due(active: usize, pending: bool) -> bool {
    active == 0 || pending
}

fn frames_pending(sources: &[(i32, Frames)]) -> bool {
    sources.iter().any(|(_, frames)| {
        !frames
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    })
}

#[derive(Debug)]
enum SpeakerQueue {
    Queued,
    Retry(Vec<i16>),
    Closed,
}

fn queue_speaker(speaker: &std_mpsc::SyncSender<Vec<i16>>, frame: Vec<i16>) -> SpeakerQueue {
    match speaker.try_send(frame) {
        Ok(()) => SpeakerQueue::Queued,
        Err(std_mpsc::TrySendError::Full(frame)) => SpeakerQueue::Retry(frame),
        Err(std_mpsc::TrySendError::Disconnected(_)) => SpeakerQueue::Closed,
    }
}

pub struct PcmBridge {
    commands: mpsc::UnboundedSender<MixerCommand>,
    capture: JoinHandle<()>,
    mixer: JoinHandle<()>,
    readers: Vec<JoinHandle<()>>,
    next_id: i32,
    failed: Arc<AtomicBool>,
    volume: Arc<AtomicU8>,
}
impl PcmBridge {
    /// Channels are supplied by the app; this never opens physical devices.
    pub fn start(
        gate: MicrophoneGate,
        microphone: std_mpsc::Receiver<Vec<i16>>,
        speaker: std_mpsc::SyncSender<Vec<i16>>,
    ) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU8::new(100));
        let capture_failed = failed.clone();
            let capture = tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(5));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut revision = gate.revision();
            loop {
                tick.tick().await;
                let current = gate.revision();
                if revision != current {
                    revision = current;
                    while microphone.try_recv().is_ok() {}
                    continue;
                }
                match microphone.try_recv() {
                    Ok(samples) => {
                        let ten_ms = gate.source.sample_rate() as usize / 100;
                        if samples.len() != ten_ms * 2 {
                            capture_failed.store(true, Ordering::Release);
                            break;
                        }
                        if gate.send(samples[..ten_ms].to_vec(), revision).is_err() {
                            capture_failed.store(true, Ordering::Release);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        if gate.send(samples[ten_ms..].to_vec(), revision).is_err() {
                            capture_failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                    Err(std_mpsc::TryRecvError::Empty) => {}
                    Err(std_mpsc::TryRecvError::Disconnected) => {
                        capture_failed.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        });
        let (commands, mut receive) = mpsc::unbounded_channel();
        let mixer_failed = failed.clone();
        let speaker_volume = volume.clone();
        let mixer = tokio::spawn(async move {
            let mut mixer = AudioMixer::new();
            let mut sources: Vec<(i32, Frames)> = Vec::new();
            let mut active = 0usize;
            let mut output = Vec::with_capacity(PLAYBACK_SAMPLES * 2);
            let mut held: Option<Vec<i16>> = None;
            let mut tick = interval(Duration::from_millis(10));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    Some(command) = receive.recv() => match command {
                        MixerCommand::Add(input) => {
                            if active == 0 { output.clear(); }
                            sources.push((input.id, input.frames.clone()));
                            mixer.add_source(input);
                            active += 1;
                        }
                        MixerCommand::Remove(id) => {
                            sources.retain(|(source_id, _)| *source_id != id);
                            mixer.remove_source(id);
                            active = active.saturating_sub(1);
                        }
                    },
                    _ = tokio::time::sleep(Duration::from_millis(2)), if held.is_some() => {
                        let frame = held.take().expect("held frame");
                        match queue_speaker(&speaker, frame) {
                            SpeakerQueue::Queued => {}
                            SpeakerQueue::Retry(frame) => held = Some(frame),
                            SpeakerQueue::Closed => {
                                mixer_failed.store(true, Ordering::Release);
                                break;
                            }
                        }
                    },
                    _ = tick.tick(), if held.is_none() => {
                        let pending = frames_pending(&sources);
                        if !playback_due(active, pending) { continue; }
                        if active == 0 { output.extend(std::iter::repeat_n(0, PLAYBACK_SAMPLES)); }
                        else {
                            let samples = mixer.mix(1);
                            if samples.len() != PLAYBACK_SAMPLES {
                                mixer_failed.store(true, Ordering::Release); break;
                            }
                            let gain = i32::from(speaker_volume.load(Ordering::Acquire).min(100));
                            output.extend(samples.iter().map(|sample| ((i32::from(*sample) * gain) / 100) as i16));
                        }
                        if output.len() == PLAYBACK_SAMPLES * 2 {
                            let frame = std::mem::replace(&mut output, Vec::with_capacity(PLAYBACK_SAMPLES * 2));
                            match queue_speaker(&speaker, frame) {
                                SpeakerQueue::Queued => {}
                                SpeakerQueue::Retry(frame) => held = Some(frame),
                                SpeakerQueue::Closed => {
                                    mixer_failed.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
        Self {
            commands,
            capture,
            mixer,
            readers: Vec::new(),
            next_id: 0,
            failed,
            volume,
        }
    }

    pub fn add_remote_track(&mut self, track: RtcAudioTrack) -> Result<()> {
        self.readers.retain(|reader| !reader.is_finished());
        if self.readers.len() >= MAX_TRACKS {
            return Err(Error::Engine);
        }
        self.next_id = self.next_id.checked_add(1).ok_or(Error::Engine)?;
        let id = self.next_id;
        let frames: Frames = Arc::new(Mutex::new(VecDeque::new()));
        self.commands
            .send(MixerCommand::Add(MixerInput {
                id,
                frames: frames.clone(),
            }))
            .map_err(|_| Error::Engine)?;
        let commands = self.commands.clone();
        self.readers.push(tokio::spawn(async move {
            let mut stream = NativeAudioStream::new(track, PLAYBACK_RATE as i32, 1);
            while let Some(frame) = stream.next().await {
                if frame.data.len() != PLAYBACK_SAMPLES {
                    continue;
                }
                let mut queue = frames.lock().unwrap_or_else(|error| error.into_inner());
                if queue.len() == MAX_QUEUED_FRAMES {
                    queue.pop_front();
                }
                queue.push_back(frame.data.into_owned());
            }
            let _ = commands.send(MixerCommand::Remove(id));
        }));
        Ok(())
    }

    pub fn set_volume(&self, percent: u8) {
        self.volume.store(percent.min(100), Ordering::Release);
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    pub async fn stop(mut self) {
        self.abort();
        let _ = (&mut self.capture).await;
        let _ = (&mut self.mixer).await;
        for reader in &mut self.readers {
            let _ = reader.await;
        }
    }
    fn abort(&self) {
        self.capture.abort();
        self.mixer.abort();
        for reader in &self.readers {
            reader.abort();
        }
    }
}
impl Drop for PcmBridge {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mixer_combines_remote_frames_and_removes_a_source() {
        let one: Frames = Arc::new(Mutex::new(VecDeque::new()));
        let two: Frames = Arc::new(Mutex::new(VecDeque::new()));
        let mut mixer = AudioMixer::new();
        mixer.add_source(MixerInput {
            id: 1,
            frames: one.clone(),
        });
        mixer.add_source(MixerInput {
            id: 2,
            frames: two.clone(),
        });
        for _ in 0..4 {
            one.lock().unwrap().push_back(vec![1_000; PLAYBACK_SAMPLES]);
            two.lock().unwrap().push_back(vec![2_000; PLAYBACK_SAMPLES]);
        }
        let both = mixer
            .mix(1)
            .iter()
            .map(|sample| i64::from(sample.abs()))
            .sum::<i64>();
        assert_eq!(mixer.mix(1).len(), PLAYBACK_SAMPLES);
        mixer.remove_source(2);
        let single = mixer
            .mix(1)
            .iter()
            .map(|sample| i64::from(sample.abs()))
            .sum::<i64>();
        assert!(both > single, "both {both}, single {single}");
    }
    #[test]
    fn native_mixer_keeps_the_playback_clock_when_a_track_has_no_frame() {
        let frames: Frames = Arc::new(Mutex::new(VecDeque::new()));
        let mut mixer = AudioMixer::new();
        mixer.add_source(MixerInput { id: 1, frames });
        assert_eq!(mixer.mix(1).len(), PLAYBACK_SAMPLES);
    }

    #[test]
    fn playback_waits_for_a_decoded_frame_and_keeps_a_full_speaker_frame() {
        assert!(playback_due(0, false));
        assert!(!playback_due(1, false));
        assert!(!playback_due(2, false));
        assert!(playback_due(2, true));
        let (sender, _receiver) = std_mpsc::sync_channel(1);
        assert!(matches!(queue_speaker(&sender, vec![1]), SpeakerQueue::Queued));
        match queue_speaker(&sender, vec![7, 8]) {
            SpeakerQueue::Retry(frame) => assert_eq!(frame, vec![7, 8]),
            other => panic!("the full frame must be retried, got a drop path: {other:?}"),
        }
    }
}
