//! Local microphone authority. User mute is synchronous; HTTP acknowledgements
//! may only enable speech for the exact intent that they were sent for.
use ost::calling::media::AudioControls;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    pub muted: bool,
    revision: u64,
}
impl Default for Command {
    fn default() -> Self {
        Self {
            muted: true,
            revision: 0,
        }
    }
}
#[derive(Default)]
struct State {
    desired: Command,
    controls: Option<AudioControls>,
    #[cfg(feature = "libwebrtc-av")]
    webrtc: Option<teams_av::pcm::MicrophoneGate>,
    closed: bool,
    server_blocked: bool,
}
#[derive(Clone, Default)]
pub(crate) struct Microphone(Arc<Mutex<State>>);
impl Microphone {
    pub fn request(&self, muted: bool) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return false;
        }
        let Some(revision) = state.desired.revision.checked_add(1) else {
            state.closed = true;
            state.desired.muted = true;
            if let Some(controls) = &state.controls {
                controls.set_microphone_muted(true);
            }
            #[cfg(feature = "libwebrtc-av")]
            if let Some(gate) = &state.webrtc { gate.set_enabled(false); }
            return false;
        };
        state.desired = Command { muted, revision };
        if muted {
            if let Some(controls) = &state.controls {
                controls.set_microphone_muted(true);
            }
            #[cfg(feature = "libwebrtc-av")]
            if let Some(gate) = &state.webrtc { gate.set_enabled(false); }
        }
        true
    }
    pub fn latest(&self) -> Command {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .desired
    }
    pub fn attach(&self, controls: AudioControls) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        controls.set_microphone_muted(true);
        if !state.closed {
            #[cfg(feature = "libwebrtc-av")]
            if let Some(gate) = state.webrtc.take() { gate.set_enabled(false); }
            state.controls = Some(controls);
        }
    }
    #[cfg(feature = "libwebrtc-av")]
    pub fn attach_webrtc(&self, gate: teams_av::pcm::MicrophoneGate) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        gate.set_enabled(false);
        if !state.closed {
            if let Some(controls) = state.controls.take() { controls.set_microphone_muted(true); }
            if let Some(previous) = state.webrtc.replace(gate) { previous.set_enabled(false); }
        }
    }
    pub fn confirm(&self, command: Command, succeeded: bool) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        #[allow(unused_mut)]
        let mut muted = state.closed || state.server_blocked || !succeeded || command.muted || state.desired != command;
        if let Some(controls) = &state.controls {
            controls.set_microphone_muted(muted);
        }
        #[cfg(feature = "libwebrtc-av")]
        if let Some(gate) = &state.webrtc {
            if !gate.set_enabled(!muted) { muted = true; }
        }
        muted
    }
    pub fn server_blocked(&self, blocked: bool) {
        let mut state = self.0.lock().unwrap_or_else(|error|error.into_inner());
        state.server_blocked = blocked;
        if blocked {
            if let Some(controls)=&state.controls { controls.set_microphone_muted(true); }
            #[cfg(feature = "libwebrtc-av")]
            if let Some(gate)=&state.webrtc { if gate.enabled() { gate.set_enabled(false); } }
        }
    }
    pub fn is_muted(&self) -> bool {
        self.view().0
    }
    pub fn view(&self) -> (bool, bool) {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        #[cfg(feature = "libwebrtc-av")]
        if let Some(gate) = &state.webrtc { return (state.closed || !gate.enabled(), state.desired.muted); }
        (state.closed || state.controls.as_ref().is_none_or(AudioControls::microphone_muted), state.desired.muted)
    }
    pub fn close(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.desired.muted = true;
        if let Some(controls) = state.controls.take() {
            controls.set_microphone_muted(true);
        }
        #[cfg(feature = "libwebrtc-av")]
        if let Some(gate) = state.webrtc.take() { gate.set_enabled(false); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "libwebrtc-av")]
    #[test]
    fn webrtc_microphone_obeys_the_same_mute_revisions() {
        crate::graph::runtime().unwrap().block_on(async {
            let engine = teams_av::engine::Engine::prepare().await.unwrap();
            let microphone = Microphone::default();
            let gate = engine.microphone_gate();
            microphone.attach_webrtc(gate.clone());
            assert!(!gate.enabled());
            assert!(microphone.request(false));
            let old = microphone.latest();
            assert!(!gate.enabled());
            assert!(!microphone.confirm(old, true));
            assert!(gate.enabled());
            assert!(microphone.request(true));
            assert!(!gate.enabled());
            assert!(microphone.confirm(old, true));
            microphone.close();
            assert!(!gate.enabled());
        });
    }
    #[test]
    fn mute_cuts_local_audio_and_old_ack_cannot_reopen_it() {
        let microphone = Microphone::default();
        let controls = AudioControls::default();
        microphone.attach(controls.clone());
        assert!(controls.microphone_muted());
        microphone.request(false);
        let old = microphone.latest();
        assert!(controls.microphone_muted());
        microphone.confirm(old, true);
        assert!(!controls.microphone_muted());
        microphone.request(true);
        assert!(controls.microphone_muted());
        assert!(microphone.confirm(old, true));
        // Even a quick mute/unmute cycle requires the new unmute's response.
        microphone.request(false);
        assert!(microphone.confirm(old, true));
        assert!(!microphone.confirm(microphone.latest(), true));
        microphone.close();
        assert!(controls.microphone_muted());
        assert!(microphone.confirm(microphone.latest(), true));
        assert!(!microphone.request(false));
        let late = AudioControls::default();
        microphone.attach(late.clone());
        assert!(late.microphone_muted());
    }
    #[test]
    fn failed_response_and_revision_exhaustion_fail_closed() {
        let microphone = Microphone::default();
        let controls = AudioControls::default();
        microphone.attach(controls.clone());
        microphone.request(false);
        assert!(microphone.confirm(microphone.latest(), false));
        microphone.0.lock().unwrap().desired.revision = u64::MAX;
        assert!(!microphone.request(false));
        assert!(controls.microphone_muted());
    }
    #[test]
    fn server_block_overrides_local_ack_and_release_does_not_open_the_gate() {
        let microphone=Microphone::default();let controls=AudioControls::default();
        microphone.attach(controls.clone());microphone.request(false);
        let command=microphone.latest();microphone.confirm(command,true);
        assert!(!controls.microphone_muted());
        microphone.server_blocked(true);
        assert!(controls.microphone_muted());
        assert!(microphone.confirm(command,true));
        microphone.server_blocked(false);
        assert!(controls.microphone_muted());
        microphone.request(true);
        assert!(microphone.confirm(command,true));
        microphone.request(false);
        assert!(!microphone.confirm(microphone.latest(),true));
        microphone.close();microphone.server_blocked(false);
        assert!(microphone.confirm(microphone.latest(),true));
    }

}
