//! Own native meeting coordinator. One bounded subscription, one controller
//! and explicit PCM devices per call. No browser or implicit auto-answer path.
use crate::{
    auth::TeamsSession,
    call_audio::LiveAudio,
    call_context::{CallCallbacks, CallContext, CallbackKind, native_id},
    call_hub::{CallDelivery, CallHub, CallStop, CallSubscription, StopReason},
    call_join::{self, CallRoute, KeepAlive, StartMeeting, StartResult, WaitingMeeting},
    call_lifecycle::CallKey,
    call_media::PreparedAudio,
    call_session::{CallTransport, ControlError, LocalParticipant},
    meeting::JoinLink,
};
use std::{
    collections::VecDeque,
    future::Future,
    net::{IpAddr, Ipv4Addr},
    pin::Pin,
    time::Duration,
};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Joining,
    Waiting,
    Connecting,
    Connected,
    Ending,
    Ended,
    Failed,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdmissionState {
    #[default]
    Idle,
    Sending,
    WaitingForConfirmation,
    Admitted,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraFailure {
    NoDevice,
    Unsupported,
    Prepare,
    Request,
    Answer,
    Connect,
    Timeout,
    TransportEnded,
}
impl CameraFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::NoDevice => "Selecione uma câmera antes de ingressar na chamada.",
            Self::Unsupported => "A chamada não forneceu uma rota para publicar o vídeo. A prévia continua local.",
            Self::Prepare => "Não foi possível preparar a conexão de vídeo. A prévia continua local.",
            Self::Request => "Não foi possível enviar a solicitação de vídeo. A prévia continua local.",
            Self::Answer => "O serviço não aceitou uma descrição de vídeo compatível. A prévia continua local.",
            Self::Connect => "Não foi possível conectar o transporte de vídeo. A prévia continua local.",
            Self::Timeout => "O serviço não confirmou o vídeo a tempo. A prévia continua local.",
            Self::TransportEnded => "A conexão de vídeo foi interrompida. A prévia continua local.",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CameraCommand {
    generation: u64,
    enabled: bool,
}
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Failed)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolIssue { Request, Controller, Acceptance, Response, Origin, Limit, State, Update }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Session,
    Route,
    Identity,
    Transport,
    Busy,
    Audio,
    AecUnavailable,
    AecFailed,
    Media,
    Protocol,
    Negotiation(ProtocolIssue),
    Http(u16),
    Codec,
    Protection,
    Timeout,
    Unsupported,
    Consent,
    Worker,
    RemoteBeforeAcceptance,
    Invitation,
}
fn audio_fault(error: crate::call_audio::AudioFailure) -> Fault {
    match error {
        crate::call_audio::AudioFailure::AecUnavailable => Fault::AecUnavailable,
        crate::call_audio::AudioFailure::AecFailed => Fault::AecFailed,
        _ => Fault::Audio,
    }
}
impl Fault {
    pub fn message(self) -> &'static str {
        match self {
            Self::Session => "A sessão atual não forneceu a credencial de chamada.",
            Self::Route => "A sessão não forneceu uma rota regional de chamada válida.",
            Self::Identity => "Não foi possível preparar sua identidade para esta chamada.",
            Self::Transport => "A conexão de sinalização da chamada foi interrompida.",
            Self::Busy => "Aguarde o encerramento da chamada anterior.",
            Self::AecUnavailable => "O cancelamento de eco acústico não está disponível. Verifique PipeWire, pw-cli e o backend WebRTC.",
            Self::AecFailed => "O cancelamento de eco acústico falhou ao conectar ou manter os dispositivos selecionados.",
            Self::Audio => {
                "Não foi possível iniciar ou manter os dispositivos de áudio selecionados."
            }
            Self::Media => "Não foi possível estabelecer a conexão de mídia nativa.",
            Self::Protocol => "A resposta da reunião não é compatível com este cliente.",
            Self::Negotiation(ProtocolIssue::Request) => "Não foi possível montar a solicitação de entrada nativa.",
            Self::Negotiation(ProtocolIssue::Controller) => "A resposta de criação da chamada não forneceu um controlador válido.",
            Self::Negotiation(ProtocolIssue::Acceptance) => "O aceite da chamada veio em um formato ainda não suportado.",
            Self::Negotiation(ProtocolIssue::Response) => "O serviço de chamadas retornou uma resposta fora do formato esperado.",
            Self::Negotiation(ProtocolIssue::Origin) => "O serviço retornou um endereço de controle de chamada não aceito.",
            Self::Negotiation(ProtocolIssue::Limit) => "A resposta do serviço de chamadas excedeu o limite do cliente.",
            Self::Negotiation(ProtocolIssue::State) => "O estado da negociação não é compatível com esta etapa da chamada.",
            Self::Negotiation(ProtocolIssue::Update) => "A atualização do controlador da chamada veio em um formato não aceito.",
            Self::Codec => {
                "A reunião selecionou um codec de áudio ainda não suportado pelo cliente."
            }
            Self::Protection => {
                "A reunião selecionou um modo de proteção de mídia ainda não suportado."
            }
            Self::Http(401) => "O serviço de chamadas não aceitou a credencial da sessão.",
            Self::Http(403) => "O serviço não autorizou a entrada nesta reunião.",
            Self::Http(404) => "A reunião ou o serviço de chamadas não foi encontrado.",
            Self::Http(409) => "O serviço informou um conflito ao entrar na reunião.",
            Self::Http(429) => {
                "O serviço limitou as tentativas. Aguarde antes de tentar novamente."
            }
            Self::Http(500..=599) => "O serviço de chamadas está temporariamente indisponível.",
            Self::Http(_) => "O serviço recusou a solicitação de chamada.",
            Self::Timeout => "A reunião não respondeu a tempo. Tente entrar novamente.",
            Self::Unsupported => {
                "Esta reunião solicitou um recurso de mídia ainda não implementado."
            }
            Self::Consent => {
                "Esta reunião exige uma confirmação adicional ainda não suportada pelo cliente."
            }
            Self::Invitation => "Não foi possível resolver o convite da reunião. Confira se o link está completo e tente novamente.",
            Self::Worker => "O serviço nativo da chamada foi interrompido.",
            Self::RemoteBeforeAcceptance => "O serviço encerrou a tentativa antes de aceitar sua entrada na reunião.",
        }
    }
    pub fn user_message(self) -> String {
        match self {
            Self::Http(code) => format!("{} (HTTP {code})", self.message()),
            _ => self.message().into(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub phase: Phase,
    pub muted: bool,
    pub requested_muted: bool,
    pub speaker_volume: u8,
    pub microphone_level: u8,
    pub mute_pending: bool,
    pub can_mute: bool,
    pub fault: Option<Fault>,
    pub command_failed: bool,
    pub camera_available: bool,
    pub camera_enabled: bool,
    pub camera_sending: bool,
    pub camera_failed: bool,
    pub camera_receiving: bool,
    pub camera_capture_ready: bool,
    pub camera_capture_failed: bool,
    pub camera_negotiation_pending: bool,
    pub camera_negotiation_failure: Option<CameraFailure>,
    pub screen_sharing: bool,
    pub roster: Vec<crate::call_roster::RosterFace>,
    pub admission_state: AdmissionState,
    pub chat_target: Option<crate::meeting::ChatTarget>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            phase: Phase::Preparing,
            muted: true,
            requested_muted: true,
            speaker_volume: 100,
            microphone_level: 0,
            mute_pending: false,
            can_mute: false,
            fault: None,
            command_failed: false,
            camera_available:false,camera_enabled:false,camera_sending:false,camera_failed:false,camera_receiving:false,
            camera_capture_ready: false,
            camera_capture_failed: false,
            camera_negotiation_pending: false,
            camera_negotiation_failure: None,
            screen_sharing: false,
            roster: Vec::new(),
            admission_state: AdmissionState::Idle,
            chat_target: None,
        }
    }
}
impl Snapshot {
    pub fn microphone_action(&self) -> Option<bool> {
        if self.phase != Phase::Connected { return None; }
        if self.mute_pending {
            // An activation in flight can always be overridden by local mute.
            return (!self.requested_muted).then_some(true);
        }
        if !self.muted { Some(true) } else { self.can_mute.then_some(false) }
    }
    pub fn label(&self) -> &'static str {
        self.phase.status(false)
    }
}
impl Phase {
    /// A direct call waits for the other person. A meeting, including one
    /// from the owner's calendar, waits for the service to confirm media.
    /// Those are different waits and must not share the call wording.
    pub fn status(self, direct_call: bool) -> &'static str {
        crate::i18n::tr(if direct_call {
            match self {
                Phase::Preparing => "Preparing the call…",
                Phase::Joining => "Calling…",
                Phase::Waiting => "Ringing…",
                Phase::Connecting => "Connecting the call…",
                Phase::Connected => "Connected",
                Phase::Ending => "Ending the call…",
                Phase::Ended => "The call ended.",
                Phase::Failed => "The call could not continue",
            }
        } else {
            match self {
                Phase::Preparing => "Preparing the meeting…",
                Phase::Joining => "Joining the meeting…",
                Phase::Waiting => "Confirming the meeting…",
                Phase::Connecting => "Connecting meeting audio…",
                Phase::Connected => "Connected to the meeting",
                Phase::Ending => "Leaving the meeting…",
                Phase::Ended => "The meeting ended.",
                Phase::Failed => "Could not stay in the meeting",
            }
        })
    }
}

#[derive(Clone, Copy)]
enum OfferProfile { MacSdes, Dtls }

struct PreparedCall {
    offer_profile: OfferProfile,
    resolved_invitation: Option<JoinLink>,
    route: CallRoute,
    transport: CallTransport,
    identity: LocalParticipant,
    addresses: Vec<IpAddr>,
    stun_service: Option<String>,
    relay_request: Option<crate::call_relay::RelayRequest>,
}
type Prep = Pin<Box<dyn Future<Output = Result<PreparedCall, Fault>> + Send>>;
type AudioFactory =
    Box<dyn FnOnce() -> Result<(LiveAudio, ost::calling::media::PcmAudioIo), Fault> + Send>;
type Request = Pin<Box<dyn Future<Output = Result<(), ControlError>> + Send>>;

pub struct Client {
    pub video:crate::call_video::Control,
    pub key: CallKey,
    stop: CallStop,
    updates: watch::Receiver<Snapshot>,
    mute: watch::Sender<bool>,
    volume: watch::Sender<u8>,
    admit_commands: mpsc::Sender<String>,
    camera_commands: watch::Sender<CameraCommand>,
    share_commands: watch::Sender<bool>,
    microphone: crate::call_microphone::Microphone,
}
impl Client {
    pub fn start(
        session: TeamsSession,
        hub: CallHub,
        key: CallKey,
        link: JoinLink,
        user_id: &str,
        name: &str,
        input: String,
        output: String,
    ) -> Result<Self, Fault> {
        let person = crate::avatars::person_id(user_id).ok_or(Fault::Identity)?;
        let identity = LocalParticipant {
            id: format!("8:orgid:{person}"),
            display_name: name.into(),
            endpoint_id: hub.agent_endpoint(),
            participant_id: native_id(),
            language_id: "pt-BR".into(),
        };
        identity.validate().map_err(|_| Fault::Identity)?;
        let stun_service = session.native_stun_service();
        let resolving_link = link.clone();
        let prep: Prep = Box::pin(async move {
            let resolved = resolving_link.resolve_thread().await.map_err(|_| Fault::Invitation)?;
            tokio::task::spawn_blocking(move || {
                let (route, transport) = session
                    .native_meeting_transport()
                    .map_err(|_| Fault::Session)?;
                let relay_request = session.native_relay_request().ok();
                Ok(PreparedCall {
                    resolved_invitation: Some(resolved),
                    offer_profile: OfferProfile::MacSdes,
                    route,
                    transport,
                    identity,
                    addresses: host_addresses()?,
                    stun_service,
                    relay_request,
                })
            })
            .await
            .map_err(|_| Fault::Worker)?
        });
        let wide_capture = webrtc_requested(&link);
        let wide_playback = wide_capture || offers_wide_receive(
            ost::calling::audio_codec::opus_available(),
            link.callee().is_some(),
        );
        let audio: AudioFactory = Box::new(move || {
            LiveAudio::start(
                (!input.is_empty()).then_some(input.as_str()),
                (!output.is_empty()).then_some(output.as_str()),
                true,
                wide_playback,
                wide_capture,
            )
            .map_err(audio_fault)
        });
        Self::spawn(hub, key, link, prep, audio)
    }
    fn spawn(
        hub: CallHub,
        key: CallKey,
        link: JoinLink,
        prep: Prep,
        audio: AudioFactory,
    ) -> Result<Self, Fault> {
        let context = hub.context().ok_or(Fault::Transport)?;
        let callbacks = context
            .prepare_callbacks(key.clone())
            .ok_or(Fault::Transport)?;
        let subscription = hub
            .subscribe(key.clone(), callbacks.clone())
            .map_err(|e| match e {
                crate::call_hub::SubscribeError::Busy => Fault::Busy,
                _ => Fault::Transport,
            })?;
        let video=crate::call_video::Control::default();
        if let Some((device,enabled))=&link.camera{video.configure(device.clone(),*enabled);}
        let worker_video=video.clone();
        let stop = subscription.stop.clone();
        let (updates, receiver) = watch::channel(Snapshot::default());
        crate::call_diagnostics::phase(Phase::Preparing);
        let (mute, commands) = watch::channel(true);
        let (volume, volume_commands) = watch::channel(100);
        let (admit_commands, admit_requests) = mpsc::channel(4);
        let (camera_commands, camera_requests) = watch::channel(CameraCommand {
            generation: u64::from(video.enabled()), enabled: video.enabled(),
        });
        let (share_commands, share_requests) = watch::channel(false);
        let microphone = crate::call_microphone::Microphone::default();
        let worker_microphone = microphone.clone();
        let call_key = key.clone();
        std::thread::Builder::new()
            .name("teams-native-call".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    publish(&updates, Phase::Failed, Some(Fault::Worker));
                    return;
                };
                let result = runtime.block_on(drive(
                    hub,
                    subscription,
                    call_key,
                    context,
                    callbacks,
                    link,
                    prep,
                    audio,
                    commands,
                    volume_commands,
                    admit_requests,
                    camera_requests,
                    share_requests,
                    &updates,
                    &worker_microphone,
                    &worker_video,
                ));
                worker_video.set_enabled(false);
                worker_microphone.close();
                match result {
                    Ok(()) => publish(&updates, Phase::Ended, None),
                    Err(fault) => publish(&updates, Phase::Failed, Some(fault)),
                }
            })
            .map_err(|_| Fault::Worker)?;
        Ok(Self {
            key,
            video,
            stop,
            updates: receiver,
            mute,
            volume,
            admit_commands,
            camera_commands,
            share_commands,
            microphone,
        })
    }
    pub fn snapshot(&mut self) -> Snapshot {
        let mut snapshot = self.updates.borrow_and_update().clone();
        if self.updates.has_changed().is_err() && !snapshot.phase.terminal() {
            // The worker may publish its final state and close the watch
            // channel between borrow_and_update() and has_changed(). A closed
            // receiver reports Err even when that final value is available.
            if !preserve_terminal_after_close(&mut snapshot, self.updates.borrow().clone()) {
                self.microphone.close();
                snapshot.muted = true;
                snapshot.phase = Phase::Failed;
                snapshot.fault = Some(Fault::Worker);
            }
        }
        (snapshot.muted, snapshot.requested_muted) = self.microphone.view();
        if snapshot.muted || snapshot.requested_muted { snapshot.microphone_level = 0; }
        snapshot.camera_available=self.video.available();snapshot.camera_enabled=self.video.enabled();
        snapshot.camera_sending=self.video.sending();snapshot.camera_failed=self.video.failed();
        snapshot.camera_receiving=self.video.receiving();
        snapshot.camera_capture_ready=self.video.capture_ready();
        snapshot.camera_capture_failed=self.video.capture_failed();
        snapshot
    }
    pub fn changes(&self) -> watch::Receiver<Snapshot> {
        self.updates.clone()
    }
    pub fn cancel(&self) {
        self.video.set_enabled(false);
        self.microphone.close();
        self.stop.cancel();
    }
    pub fn set_muted(&self, muted: bool) {
        if self.microphone.request(muted) { let _ = self.mute.send(muted); }
    }
    pub fn set_speaker_volume(&self, percent: u8) {
        let _ = self.volume.send(percent.min(100));
    }
    pub fn request_camera(&self) {
        self.set_camera_enabled(true);
    }
    pub fn set_screen_sharing(&self, enabled: bool) {
        self.share_commands.send_replace(enabled);
    }
    pub fn set_camera_enabled(&self, enabled: bool) {
        if enabled { self.video.prepare_send(); } else { self.video.set_enabled(false); }
        self.camera_commands.send_modify(|command| {
            command.generation = command.generation.wrapping_add(1).max(1);
            command.enabled = enabled;
        });
    }
    pub fn admit_participant(&self, participant_id: &str) -> bool {
        if participant_id.trim().is_empty()
            || participant_id.len() > 512
            || participant_id.chars().any(char::is_control)
        {
            return false;
        }
        self.admit_commands
            .try_send(participant_id.to_owned())
            .is_ok()
    }
    pub async fn completed(mut updates: watch::Receiver<Snapshot>) {
        loop {
            if updates.borrow_and_update().phase.terminal() {
                return;
            }
            if updates.changed().await.is_err() {
                return;
            }
        }
    }
}

fn preserve_terminal_after_close(snapshot: &mut Snapshot, latest: Snapshot) -> bool {
    if latest.phase.terminal() {
        *snapshot = latest;
        true
    } else {
        false
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.video.set_enabled(false);
        self.microphone.close();
        self.stop.cancel();
    }
}
fn publish(updates: &watch::Sender<Snapshot>, phase: Phase, fault: Option<Fault>) {
    crate::call_diagnostics::phase(phase);
    if let Some(fault) = fault { crate::call_diagnostics::fault(fault); }
    updates.send_modify(|state| {
        state.phase = phase;
        state.fault = fault;
    });
}
fn stopped(subscription: &CallSubscription) -> Result<(), Fault> {
    crate::live_trace::record("call_stopped",0,subscription.stop.reason() as usize,0);
    match subscription.stop.reason() {
        StopReason::User | StopReason::OwnerReleased | StopReason::RemoteEnded => Ok(()),
        _ => Err(Fault::Transport),
    }
}
fn join_fault(error: call_join::JoinError) -> Fault {
    match error {
        call_join::JoinError::InteractionRequired => Fault::Consent,
        call_join::JoinError::Media(crate::call_media::MediaError::UnsupportedCodec) => {
            Fault::Codec
        }
        call_join::JoinError::Media(crate::call_media::MediaError::UnsupportedCrypto) => {
            Fault::Protection
        }
        call_join::JoinError::Media(_) => Fault::Media,
        call_join::JoinError::Control(ControlError::Http(status)) => Fault::Http(status),
        call_join::JoinError::Control(ControlError::TimedOut) => Fault::Timeout,
        call_join::JoinError::Control(ControlError::Network) => Fault::Transport,
        call_join::JoinError::StaleTransport => Fault::Transport,
        call_join::JoinError::InvalidRequest | call_join::JoinError::Control(ControlError::InvalidRequest) => Fault::Negotiation(ProtocolIssue::Request),
        call_join::JoinError::InvalidController => Fault::Negotiation(ProtocolIssue::Controller),
        call_join::JoinError::InvalidAcceptance => Fault::Negotiation(ProtocolIssue::Acceptance),
        call_join::JoinError::InvalidState | call_join::JoinError::Cancelled => Fault::Negotiation(ProtocolIssue::State),
        call_join::JoinError::Control(ControlError::InvalidResponse) => Fault::Negotiation(ProtocolIssue::Response),
        call_join::JoinError::Control(ControlError::InvalidOrigin) => Fault::Negotiation(ProtocolIssue::Origin),
        call_join::JoinError::Control(ControlError::TooLarge) => Fault::Negotiation(ProtocolIssue::Limit),
    }
}
async fn cleanup(
    waiting: &mut WaitingMeeting,
    key: &CallKey,
    live: &mut Option<LiveAudio>,
    remote: bool,
    updates: &watch::Sender<Snapshot>,
) {
    waiting.stop_video().await;
    publish(updates, Phase::Ending, None);
    if let Some(live) = live.take() {
        live.request_stop();
        let _ = tokio::task::spawn_blocking(move || live.stop()).await;
    }
    if remote {
        let _ = waiting.session.release_locally(key).await;
    } else {
        let _ = waiting.cancel().await;
    }
}

async fn drive(
    hub: CallHub,
    mut subscription: CallSubscription,
    key: CallKey,
    context: CallContext,
    callbacks: CallCallbacks,
    link: JoinLink,
    prep: Prep,
    audio: AudioFactory,
    mut commands: watch::Receiver<bool>,
    mut volume_commands: watch::Receiver<u8>,
    mut admit_commands: mpsc::Receiver<String>,
    mut camera_commands: watch::Receiver<CameraCommand>,
    mut share_commands: watch::Receiver<bool>,
    updates: &watch::Sender<Snapshot>,
    microphone: &crate::call_microphone::Microphone,
    video_control:&crate::call_video::Control,
) -> Result<(), Fault> {
    if subscription.stop.reason() != StopReason::Running {
        return stopped(&subscription);
    }
    let mut preparation_cancel = subscription.cancellation();
    let prepared = tokio::select! {
        biased;
        _ = preparation_cancel.changed() => return stopped(&subscription),
        result = prep => result,
    };
    if subscription.stop.reason() != StopReason::Running {
        return stopped(&subscription);
    }
    let PreparedCall {
        resolved_invitation,
        offer_profile,
        route,
        transport,
        identity,
        addresses,
        stun_service,
        relay_request,
    } = prepared?;
    let link = resolved_invitation.unwrap_or(link);
    let chat_target = link.chat_target();
    updates.send_modify(|state| state.chat_target = chat_target);
    let mut cancel = subscription.cancellation();
    let offered = if webrtc_requested(&link) {
        #[cfg(feature = "libwebrtc-av")]
        {
            let request = relay_request.ok_or(Fault::Media)?;
            let grant = tokio::select! { biased;
                _ = cancel.changed() => return stopped(&subscription),
                result = request.fetch() => result.map_err(|_| Fault::Media)?,
            };
            let ice = grant.into_webrtc_server().map_err(|_| Fault::Media)?;
            let gallery = if link.callee().is_none() { 9 } else { 0 };
            let engine = tokio::select! { biased;
                _ = cancel.changed() => return stopped(&subscription),
                result = teams_av::engine::Engine::prepare_layout(vec![ice], 48_000, gallery) => result.map_err(|_| Fault::Media)?,
            };
            call_join::PreparedMedia::WebRtc(engine)
        }
        #[cfg(not(feature = "libwebrtc-av"))]
        unreachable!("libwebrtc is unavailable without the build feature")
    } else {
    let mut offered = PreparedAudio::bind("0.0.0.0:0".parse().unwrap(), &addresses)
        .await
        .map_err(|_| Fault::Media)?;
    let mut relay_selected = false;
    if let Some(request) = relay_request {
        let result = tokio::select! {biased;
            _=cancel.changed()=>return stopped(&subscription),
            result=request.prepare()=>result,
        };
        if let Ok(allocation) = result {
            offered.use_relay(allocation).map_err(|_| Fault::Media)?;
            relay_selected = true;
        }
    }
    if !relay_selected {
        if let Some(service) = stun_service {
            tokio::select! {biased;
                _=cancel.changed()=>return stopped(&subscription),
                _=offered.gather_stun(&service)=>{},
            }
        }
    }
    // The 11/11 official capture of this headset is 48 kHz playback. A direct
    // call uses that receive clock too. Capture stays 8 kHz, so the offer keeps
    // sprop-maxcapturerate=8000 and PCMU remains the send codec.
    if offers_wide_receive(ost::calling::audio_codec::opus_available(), link.callee().is_some()) {
        let enabled = offered.enable_opus();
        crate::live_trace::record("call_opus_offer_v1",0,usize::from(ost::calling::audio_codec::opus_available()),usize::from(enabled));
    }
    crate::live_trace::record("call_audio_codec", 0, usize::from(link.callee().is_none()), 0);
    // Select protection before signaling; never retry a failed DTLS call
    // with SDES. Mac-compatible meetings use SDES over authenticated HTTPS.
    if matches!(offer_profile, OfferProfile::Dtls) && offered.enable_dtls().is_err() {
        offered.close().await;
        return Err(Fault::Protection);
    }
    crate::live_trace::record("call_offer_profile", 0, match offer_profile {
        OfferProfile::MacSdes => 0, OfferProfile::Dtls => 1,
    }, 0);
    // messages=2 means the offer stays audio-only. Bundled video
    // sections, including the compressed offer, were ended before
    // acceptance with 410/301000 and an SDP phrase.
    crate::live_trace::record("call_video_offer_direction", 0, 2, 0);
    call_join::PreparedMedia::Native(offered)
    };
    publish(updates, Phase::Joining, None);
    let self_mri = identity.id.clone();
    let created = call_join::start_meeting(
        route,
        transport,
        StartMeeting {
            key: key.clone(),
            identity,
            invitation: link,
            callbacks: callbacks.clone(),
            context,
            muted: true,
        },
        offered,
        cancel.clone(),
    )
    .await
    .map_err(join_fault)?;
    let StartResult::Waiting(mut waiting) = created else {
        return stopped(&subscription);
    };
    waiting.video_control=Some(video_control.clone());
    crate::live_trace::record("call_create_validated",0,0,0);
    let mut live = None;
    let result=async {
        let mut roster = waiting.initial_roster.take().unwrap_or_else(|| crate::call_roster::Roster::new(self_mri));
        publish_roster(&roster, updates);
        publish(updates,Phase::Waiting,None);
        let deadline=tokio::time::sleep(Duration::from_secs(120));tokio::pin!(deadline);
        let mut early_acceptance=None;
        while let Some(event)=subscription.try_next() {
            note_roster(&mut roster, updates, &event);
            match waiting_signal(&mut waiting,&key,event,&hub,&callbacks,cancel.clone(),false).await {
                WaitingSignal::Continue=>{}
                WaitingSignal::Accepted(media)=>{early_acceptance=Some(media);break;}
                WaitingSignal::Cancelled=>return stopped(&subscription),
                WaitingSignal::Failed(fault)=>return Err(fault),
            }
        }
        let accepted=if let Some(media)=early_acceptance { media } else { loop {
            tokio::select! {biased;
                _=cancel.changed()=>return stopped(&subscription),
                _=&mut deadline=>return Err(Fault::Timeout),
                event=subscription.next()=>{
                    let Some(event)=event else{return stopped(&subscription);};
                    note_roster(&mut roster, updates, &event);
                    match waiting_signal(&mut waiting,&key,event,&hub,&callbacks,cancel.clone(),false).await {
                        WaitingSignal::Continue=>{}
                        WaitingSignal::Accepted(media)=>break media,
                        WaitingSignal::Cancelled=>return stopped(&subscription),
                        WaitingSignal::Failed(fault)=>return Err(fault),
                    }
                }
            }
        }};
        let keepalive=accepted.keepalive.clone().ok_or(Fault::Negotiation(ProtocolIssue::Acceptance))?;
        publish(updates,Phase::Connecting,None);
        let capture_stop=subscription.stop.clone();
        let audio_task=tokio::task::spawn_blocking(move || {
            if capture_stop.reason()!=StopReason::Running {return Ok(None);}
            audio().map(Some)
        });
        tokio::pin!(audio_task);
        let mut deferred=Vec::new();
        let mut deferred_bytes=0usize;
        let (ack_result, audio_result) = {
            let ack=waiting.acknowledge(&accepted,&mut cancel);
            tokio::pin!(ack);
            let mut ack_result=None;
            let mut audio_result=None;
            loop {
                if ack_result.is_some() && audio_result.is_some() { break; }
                tokio::select! {
                    biased;
                    event = subscription.next() => {
                        match event {
                            Some(event) if matches!(event.kind, CallbackKind::CallEnd | CallbackKind::ConversationEnd) => {
                                crate::call_diagnostics::remote_end(&event.body, false);
                                crate::live_trace::record("call_remote_end_connecting", 0, event.kind as usize, 0);
                                subscription.stop.remote_ended();
                            }
                            Some(event) if deferred.len() < 32 && deferred_bytes.saturating_add(event.bytes()) <= 2 * 1024 * 1024 => {
                                deferred_bytes += event.bytes();
                                deferred.push(event);
                            }
                            Some(_) => subscription.stop.overflow(),
                            None => {},
                        }
                    }
                    result = &mut ack, if ack_result.is_none() => ack_result = Some(result),
                    result = &mut audio_task, if audio_result.is_none() => audio_result = Some(result),
                }
                if subscription.stop.reason() != StopReason::Running { break; }
            }
            if ack_result.is_none() {
                ack_result = Some(ack.await);
            }
            if audio_result.is_none() {
                audio_result = Some(audio_task.await);
            }
            (ack_result, audio_result)
        };
        let opened = match audio_result.unwrap() {
            Ok(Ok(device)) => device,
            Ok(Err(fault)) => return Err(fault),
            Err(_) => return Err(Fault::Worker),
        };
        if subscription.stop.reason() != StopReason::Running {
            if let Some((device, _)) = opened {
                device.request_stop();
                let _ = tokio::task::spawn_blocking(move || device.stop()).await;
            }
            return stopped(&subscription);
        }
        if let Err(error) = ack_result.unwrap() {
            if let Some((device, _)) = opened {
                device.request_stop();
                let _ = tokio::task::spawn_blocking(move || device.stop()).await;
            }
            return Err(join_fault(error));
        }
        let Some((device, io)) = opened else { return stopped(&subscription); };
        live=Some(device);
        let ready_until=tokio::time::Instant::now()+Duration::from_secs(3);
        while !live.as_ref().unwrap().is_ready(){
            if live.as_ref().unwrap().is_finished(){return Err(live.as_ref().unwrap().failure().map(audio_fault).unwrap_or(Fault::Audio));}
            if tokio::time::Instant::now()>=ready_until{return Err(Fault::Audio);}
            tokio::select!{biased;_=cancel.changed()=>return stopped(&subscription),_=tokio::time::sleep(Duration::from_millis(10))=>{}}
        }
        let media_deferred = watch_join(
            waiting.connect_media(accepted, io, &mut cancel),
            &mut subscription,
            &mut deferred,
            &mut deferred_bytes,
        ).await;
        if subscription.stop.reason() != StopReason::Running { return stopped(&subscription); }
        media_deferred.map_err(join_fault)?;
        let mut pending_negotiations = VecDeque::new();
        let mut pending_negotiation_bytes = 0usize;
        for event in deferred {
            note_roster(&mut roster, updates, &event);
            match event.kind {
                CallbackKind::ConversationUpdate => apply_update(&mut waiting, &key, &event.body)?,
                CallbackKind::MediaRenegotiation => {
                    pending_negotiation_bytes += event.bytes();
                    pending_negotiations.push_back(event);
                }
                _ => {},
            }
        }
        if subscription.stop.reason()!=StopReason::Running{return stopped(&subscription);}
        if let Some(controls) = waiting.session.audio_controls(&key) { microphone.attach(controls); }
        #[cfg(feature = "libwebrtc-av")]
        if let Some(gate) = waiting.session.webrtc_gate(&key) { microphone.attach_webrtc(gate); }
        crate::call_diagnostics::phase(Phase::Connected);
        updates.send_modify(|state|{state.phase=Phase::Connected;state.can_mute=waiting.session.mute_supported();});
        active(&hub,&mut subscription,&mut waiting,&key,live.as_ref().unwrap(),keepalive,&mut commands,&mut volume_commands,&mut admit_commands,&mut camera_commands,&mut share_commands,&addresses,&callbacks,updates,microphone,&mut roster,pending_negotiations,pending_negotiation_bytes).await
    }.await;
    cleanup(&mut waiting, &key, &mut live, false, updates).await;
    if subscription.stop.reason() != StopReason::Running {
        stopped(&subscription)
    } else {
        result
    }
}

/// Do not publish Connected while a remote CallEnd/ConversationEnd is already
/// queued during the media handshake. The existing stop watch cancels the
/// in-flight join and its compensation path exactly once.
async fn watch_join(
    pending: impl Future<Output = Result<(), call_join::JoinError>>,
    subscription: &mut CallSubscription,
    deferred: &mut Vec<CallDelivery>,
    deferred_bytes: &mut usize,
) -> Result<(), call_join::JoinError> {
    let pending = pending;
    tokio::pin!(pending);
    loop {
        tokio::select! {
            biased;
            event = subscription.next() => {
                match event {
                    Some(event) if matches!(event.kind, CallbackKind::CallEnd | CallbackKind::ConversationEnd) => {
                        crate::call_diagnostics::remote_end(&event.body, false);
                        crate::live_trace::record("call_remote_end_connecting", 0, event.kind as usize, 0);
                        subscription.stop.remote_ended();
                    }
                    Some(event) if deferred.len() < 32 && deferred_bytes.saturating_add(event.bytes()) <= 2 * 1024 * 1024 => {
                        *deferred_bytes += event.bytes();
                        deferred.push(event);
                    }
                    Some(_) => subscription.stop.overflow(),
                    None => {},
                }
                if subscription.stop.reason() != StopReason::Running {
                    return pending.await;
                }
            }
            result = &mut pending => return result,
        }
    }
}
enum WaitingSignal {
    Continue,
    Accepted(call_join::AcceptedMedia),
    Cancelled,
    Failed(Fault),
}

async fn waiting_signal(
    waiting: &mut WaitingMeeting,
    key: &CallKey,
    event: CallDelivery,
    hub: &CallHub,
    callbacks: &CallCallbacks,
    cancel: watch::Receiver<bool>,
    dial: bool,
) -> WaitingSignal {
    match event.kind {
        CallbackKind::Acceptance => {
            let Some(current) = hub.context() else { return WaitingSignal::Failed(Fault::Transport) };
            let Ok(bytes) = serde_json::to_vec(&event.body) else { return WaitingSignal::Failed(Fault::Protocol) };
            match waiting.acceptance(&current, callbacks.link(CallbackKind::Acceptance), &bytes) {
                Ok(media) => WaitingSignal::Accepted(media),
                Err(error) => WaitingSignal::Failed(join_fault(error)),
            }
        }
        CallbackKind::CallEnd | CallbackKind::ConversationEnd => {
            crate::call_diagnostics::remote_end(&event.body, true);
            crate::live_trace::record("call_remote_end_before_acceptance", 0, event.kind as usize, 0);
            let _ = waiting.session.release_locally(key).await;
            WaitingSignal::Failed(Fault::RemoteBeforeAcceptance)
        }
        CallbackKind::AddParticipantFailure => {
            crate::call_diagnostics::invite_failure(&event.body);
            WaitingSignal::Failed(Fault::RemoteBeforeAcceptance)
        }
        CallbackKind::Redirection | CallbackKind::MediaRenegotiation => WaitingSignal::Failed(Fault::Unsupported),
        CallbackKind::ConversationUpdate => {
            let update = event.body.get("conversationUpdate").cloned().unwrap_or(event.body);
            if let Err(error) = apply_update(waiting, key, &update) {
                return WaitingSignal::Failed(error);
            }
            if dial && waiting.awaits_direct_ring() && update.get("conversationController").is_some() {
                match waiting.ring(Some(&update), cancel).await {
                    Ok(true) => WaitingSignal::Cancelled,
                    Ok(false) => WaitingSignal::Continue,
                    Err(error) => WaitingSignal::Failed(join_fault(error)),
                }
            } else {
                WaitingSignal::Continue
            }
        }
        _ => WaitingSignal::Continue,
    }
}

fn note_roster(
    roster: &mut crate::call_roster::Roster,
    updates: &watch::Sender<Snapshot>,
    event: &CallDelivery,
) {
    if event.kind != CallbackKind::RosterUpdate || !roster.apply(&event.body) {
        return;
    }
    publish_roster(roster, updates);
}

fn publish_roster(roster: &crate::call_roster::Roster, updates: &watch::Sender<Snapshot>) {
    let faces = roster.faces();
    let cameras = faces.iter().filter(|face| face.video_on).count();
    let waiting = faces.iter().filter(|face| face.is_waiting && !face.is_self).count();
    crate::live_trace::record("call_roster", 0, faces.len(), cameras);
    let without_endpoints = faces.iter()
        .filter(|face| !face.is_self && !face.admission_known && !face.has_endpoints).count();
    crate::live_trace::record("call_roster_candidates", 0, waiting, without_endpoints);
    updates.send_modify(|state| state.roster = faces);
}

fn apply_update(
    waiting: &mut WaitingMeeting,
    key: &CallKey,
    body: &serde_json::Value,
) -> Result<(), Fault> {
    let body = body.get("conversationUpdate").unwrap_or(body);
    if body.get("conversationController").is_none() {
        return Ok(());
    }
    let projected = serde_json::json!({"conversationController":body.get("conversationController"),"sequenceNumber":body.get("sequenceNumber"),"links":{
        "leave":body.pointer("/links/leave"),"updateEndpointState":body.pointer("/links/updateEndpointState"),"unmute":body.pointer("/links/unmute")}});
    let mut projected = projected;
    if projected["sequenceNumber"].is_null() {
        projected.as_object_mut().unwrap().remove("sequenceNumber");
    }
    waiting
        .session
        .apply_conversation_update_detailed(
            key,
            &serde_json::to_vec(&projected).map_err(|_| Fault::Negotiation(ProtocolIssue::Update))?,
        )
        .map_err(|error| {
            crate::call_diagnostics::rejected(crate::call_diagnostics::Phase::Update,error,body,None);
            Fault::Negotiation(ProtocolIssue::Update)
        })?;
    Ok(())
}
async fn request(job: &mut Option<Request>) -> Result<(), ControlError> {
    match job {
        Some(job) => job.await,
        None => std::future::pending().await,
    }
}

fn renegotiation_request(waiting: &WaitingMeeting, key: &CallKey, callbacks: &CallCallbacks, body: &serde_json::Value) -> Option<Request> {
    #[cfg(feature = "libwebrtc-av")]
    if let Some(session) = waiting.session.webrtc(key) {
        let negotiation = body.get("mediaNegotiation").unwrap_or(body);
        let transport = waiting.session.transport().clone();
        let answer_url = negotiation.pointer("/links/mediaAnswer").and_then(serde_json::Value::as_str)
            .filter(|url| transport.validate_response_link(url).is_ok()).map(str::to_owned);
        let rejection_url = negotiation.pointer("/links/rejection").and_then(serde_json::Value::as_str)
            .filter(|url| transport.validate_response_link(url).is_ok()).map(str::to_owned);
        if answer_url.is_none() && rejection_url.is_none() { return None; }
        let content_type = negotiation.pointer("/mediaContent/contentType").and_then(serde_json::Value::as_str).unwrap_or("");
        let blob = negotiation.pointer("/mediaContent/blob").and_then(serde_json::Value::as_str).unwrap_or("");
        let valid_content = answer_url.is_some() && matches!(content_type, "application/sdp" | "application/sdp-ngc-1.0")
            && !blob.is_empty() && blob.len() <= crate::call_media::MAX_SDP_BYTES;
        let blob = if valid_content { blob.to_owned() } else { String::new() };
        let participant = waiting.session.participant().clone();
        let acknowledgement = callbacks.link(CallbackKind::MediaAcknowledgement).to_owned();
        let video_control = callbacks.link(CallbackKind::ControlVideoStreaming).to_owned();
        let modalities = call_join::answer_modalities(negotiation.get("callModalities"), true);
        let keep = session.sharing().then(|| session.share_mid()).flatten();
        let negotiator = session.negotiator();
        return Some(Box::pin(async move {
            let answer = if valid_content {
                match ost::calling::sdp_compress::decompress_sdp_with_limit(&blob, crate::call_media::MAX_SDP_BYTES) {
                    Ok(text) => negotiator.answer_keeping(&text, keep.as_deref()).await.map_err(|_| ()),
                    Err(_) => Err(()),
                }
            } else { Err(()) };
            match answer {
                Ok(answer) => {
                    let payload = serde_json::json!({"mediaAnswer":{
                        "callModalities":modalities,"sender":participant,
                        "links":{"mediaAcknowledgement":acknowledgement},
                        "clientContentForMediaController":{"controlVideoStreaming":video_control},
                        "mediaContent":{"contentType":"application/sdp-ngc-1.0","blob":answer}
                    }});
                    transport.post_json(answer_url.as_deref().ok_or(ControlError::InvalidResponse)?, &payload).await.map(|_| ())
                }
                Err(_) => {
                    if let Some(url) = rejection_url {
                        let payload = serde_json::json!({"mediaNegotiationFailure":{"sender":participant,
                            "code":488,"subCode":3102,"phrase":"CallEndReasonMediaAnswerError"}});
                        let _ = transport.post_json(&url, &payload).await;
                    }
                    Err(ControlError::InvalidResponse)
                }
            }
        }));
    }
    #[cfg(not(feature = "libwebrtc-av"))]
    let _ = (key, callbacks);
    match waiting.renegotiation_answer(body) {
        call_join::RenegotiationDecision::Send(request) => {
            crate::live_trace::record("call_media_renegotiation_answer", 0, request.trace, 0);
            let transport = waiting.session.transport().clone();
            Some(Box::pin(async move {
                transport.post_json(&request.url, &request.payload).await.map(|_| ())
            }))
        }
        call_join::RenegotiationDecision::Ignore => {
            crate::live_trace::record("call_media_renegotiation_while_connected", 0, 0, 0);
            None
        }
    }
}
type CameraOfferJob = Pin<Box<dyn Future<Output = Result<(
    crate::call_video::Prepared,
    String,
    Result<serde_json::Value, ControlError>,
), ()>> + Send>>;
type CameraConnectJob = Pin<Box<dyn Future<Output = Result<crate::call_video::Session, ()>> + Send>>;

#[cfg(feature = "libwebrtc-av")]
fn camera_source_request(mid: &str, stream_id: u32, source: Option<u32>, sequence: u32, screen: bool) -> serde_json::Value {
    let (max_fs, max_mbps, max_fps) = if screen { (8160, 135_000, 1500) } else { (3600, 108_000, 3000) };
    let control = serde_json::json!({"controlVideoStreaming": {"sequenceNumber":sequence,"controlInfo":{
        "sourceId":source.map_or(-1, i64::from),"streamMsid":stream_id,
        "fmtParams":[{"max-fs":max_fs,"max-mbps":max_mbps,"max-fps":max_fps,"profile-level-id":"64001f"}]
    }}});
    serde_json::json!({"applyChannelParameters":{"multiChannelParameter":{
        "mids":[mid],"mediaParameter":control.to_string()
    }}})
}

#[cfg(feature = "libwebrtc-av")]
fn camera_capabilities_request(mid: &str, sequence: u32) -> serde_json::Value {
    let capabilities = serde_json::json!({"maxVideoSendCapabilities":{
        "max-width":640,"max-height":640,"max-fps":30,"max-streams":3,
        "max-layers":2,"sequence-number":sequence
    }});
    serde_json::json!({"applyChannelParameters":{"multiChannelParameter":{
        "mids":[mid],"mediaParameter":capabilities.to_string()
    }}})
}

#[cfg(feature = "libwebrtc-av")]
fn rtc_camera_needs_request(intent: CameraCommand, applied: bool, started: bool, failed: Option<u64>) -> bool {
    (intent.enabled != applied || (intent.enabled && !started)) && failed != Some(intent.generation)
}

#[cfg(feature = "libwebrtc-av")]
fn rtc_camera_may_start(intent: CameraCommand, sent: CameraCommand) -> bool {
    intent == sent && intent.enabled
}

#[derive(Default)]
struct CameraOperation {
    generation: Option<u64>,
    deadline: Option<tokio::time::Instant>,
    http: Option<CameraOfferJob>,
    prepared: Option<crate::call_video::Prepared>,
    offer: Option<String>,
    reply: Option<call_join::CameraOfferReply>,
    connect: Option<CameraConnectJob>,
}
impl CameraOperation {
    fn clear(&mut self, hub: &CallHub, key: &CallKey) {
        if let Some(generation) = self.generation {
            hub.clear_camera_callbacks(key, generation);
        }
        *self = Self::default();
    }
    fn fail(&mut self, hub: &CallHub, key: &CallKey, waiting: &WaitingMeeting,
        updates: &watch::Sender<Snapshot>, failure: CameraFailure,
    ) {
        self.clear(hub, key);
        if let Some(control) = &waiting.video_control { control.fail(); }
        updates.send_modify(|state| {
            state.camera_negotiation_pending = false;
            state.camera_negotiation_failure = Some(failure);
        });
        crate::live_trace::record("call_camera_negotiation_failed_v1", 0, failure as usize, 0);
    }
    fn receive(&mut self, event: &CallDelivery) -> bool {
        if self.generation.is_none() || event.camera_generation != self.generation
            || self.connect.is_some() || (self.http.is_none() && self.prepared.is_none())
            || self.reply.is_some()
        { return false; }
        self.reply = Some(match event.kind {
            CallbackKind::MediaAnswer => match call_join::camera_offer_reply(&event.body) {
                answer @ call_join::CameraOfferReply::Answer { .. } => answer,
                _ => call_join::CameraOfferReply::Rejected,
            },
            CallbackKind::MediaRejection => call_join::CameraOfferReply::Rejected,
            _ => return false,
        });
        true
    }
}

fn remember_mixer_leg(waiting: &mut WaitingMeeting, keepalive: &mut KeepAlive, body: &serde_json::Value) {
    let links = call_join::mixer_leg_links(body);
    let transport = waiting.session.transport().clone();
    let retained = waiting.retain_mixer_leg(&links, |url| transport.validate_response_link(url).is_ok());
    let replaced = retained.call_leg.is_some();
    if let Some(url) = retained.call_leg {
        waiting.call_leg = Some(url.clone());
        keepalive.url = url;
    }
    if retained.stored > 0 {
        crate::live_trace::record("call_mixer_leg_v1", 0, retained.stored, usize::from(replaced));
    }
}

async fn active(
    hub: &CallHub,
    subscription: &mut CallSubscription,
    waiting: &mut WaitingMeeting,
    key: &CallKey,
    live: &LiveAudio,
    mut keepalive: KeepAlive,
    commands: &mut watch::Receiver<bool>,
    volume_commands: &mut watch::Receiver<u8>,
    admit_commands: &mut mpsc::Receiver<String>,
    camera_commands: &mut watch::Receiver<CameraCommand>,
    share_commands: &mut watch::Receiver<bool>,
    addresses: &[std::net::IpAddr],
    callbacks: &CallCallbacks,
    updates: &watch::Sender<Snapshot>,
    microphone: &crate::call_microphone::Microphone,
    roster: &mut crate::call_roster::Roster,
    mut pending_negotiations: VecDeque<CallDelivery>,
    mut pending_negotiation_bytes: usize,
) -> Result<(), Fault> {
    let mut cancel = subscription.cancellation();
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + keepalive.interval,
        keepalive.interval,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut health = tokio::time::interval(Duration::from_secs(1));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut monitor = tokio::time::interval(Duration::from_millis(100));
    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keepalive_job = None;
    let mut mute_job = None;
    let mut server_unmute_job: Option<Pin<Box<dyn Future<Output=Result<serde_json::Value,ControlError>> + Send>>> = None;
    let mut unmute_confirmation_deadline: Option<tokio::time::Instant> = None;
    let mut own_audio_seen = false;
    let mut server_blocked = false;
    let mut renegotiation_job = None;
    let mut camera = CameraOperation::default();
    let mut camera_closed = false;
    #[allow(unused_mut, unused_variables)]
    let mut rtc_job: Option<Request> = None;
    #[allow(unused_mut, unused_variables)]
    let mut rtc_source_job: Option<Request> = None;
    #[allow(unused_mut, unused_variables)]
    let mut rtc_screen_job: Option<Request> = None;
    #[allow(unused_mut, unused_variables)]
    let mut rtc_share_job: Option<Request> = None;
    #[allow(unused_mut, unused_variables)]
    let mut rtc_capability_job: Option<Request> = None;
    #[cfg(feature = "libwebrtc-av")]
    let rtc_active = waiting.session.webrtc_gate(key).is_some();
    #[cfg(feature = "libwebrtc-av")]
    let (mut rtc_intent, mut rtc_sent, mut rtc_applied, mut rtc_started, mut rtc_failed, mut rtc_sequence) =
        (CameraCommand::default(), CameraCommand::default(), false, false, None, 1u32);
    #[cfg(feature = "libwebrtc-av")]
    let (mut rtc_source_applied, mut rtc_source_sent, mut rtc_source_failed, mut rtc_source_sequence) =
        (Some(None::<u32>), None::<u32>, None::<Option<u32>>, 0u32);
    #[cfg(feature = "libwebrtc-av")]
    let mut rtc_source_seen: Option<Option<u32>> = None;
    #[cfg(feature = "libwebrtc-av")]
    let (mut rtc_screen_applied, mut rtc_screen_sent, mut rtc_screen_failed, mut rtc_screen_sequence) =
        (Some(None::<u32>), None::<u32>, None::<Option<u32>>, 0u32);
    #[cfg(feature = "libwebrtc-av")]
    let mut rtc_screen_seen: Option<Option<u32>> = None;
    #[cfg(feature = "libwebrtc-av")]
    let mut share_pump: Option<crate::call_webrtc::ScreenPump> = None;
    #[cfg(feature = "libwebrtc-av")]
    let (mut share_failed, mut share_size, mut share_want, mut share_posted, mut share_inflight) =
        (false, None::<(u32, u32)>, None::<bool>, false, None::<bool>);
    #[cfg(feature = "libwebrtc-av")]
    let mut share_sequence = 0u32;
    let mut share_tick = tokio::time::interval(Duration::from_millis(66));
    share_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    if camera_commands.borrow().enabled { camera_commands.mark_changed(); }
    let mut admission_job = None;
    let mut admission_pending = false;
    let mut in_flight = crate::call_microphone::Command::default();
    let mut confirmed = Some(in_flight);
    loop {
        #[cfg(feature = "libwebrtc-av")]
        if rtc_active && rtc_source_job.is_none() {
            let desired = roster.first_remote_video_source();
            if rtc_source_seen != Some(desired) { rtc_source_seen = Some(desired); rtc_source_failed = None; }
            if rtc_source_applied != Some(desired) && rtc_source_failed != Some(desired) {
                let slot = waiting.session.webrtc_mut(key).and_then(|session| session.remote_camera_slot());
                if let (Some(url), Some((mid, stream_id))) = (waiting.apply_channel_parameters.clone(), slot) {
                    let Some(next) = rtc_source_sequence.checked_add(1) else { return Err(Fault::Protocol); };
                    rtc_source_sequence = next;
                    rtc_source_sent = desired;
                    let payload = camera_source_request(&mid, stream_id, desired, rtc_source_sequence, false);
                    let transport = waiting.session.transport().clone();
                    rtc_source_job = Some(Box::pin(async move { transport.post_json(&url, &payload).await.map(|_| ()) }));
                }
            }
        }
        #[cfg(feature = "libwebrtc-av")]
        if rtc_active && rtc_screen_job.is_none() {
            let desired = roster.first_remote_screen_source();
            if rtc_screen_seen != Some(desired) { rtc_screen_seen = Some(desired); rtc_screen_failed = None; }
            if rtc_screen_applied != Some(desired) && rtc_screen_failed != Some(desired) {
                let slot = waiting.session.webrtc_mut(key).and_then(|session| session.screen_slot());
                if let (Some(url), Some((mid, stream_id))) = (waiting.apply_channel_parameters.clone(), slot) {
                    let Some(next) = rtc_screen_sequence.checked_add(1) else { return Err(Fault::Protocol); };
                    rtc_screen_sequence = next;
                    rtc_screen_sent = desired;
                    let payload = camera_source_request(&mid, stream_id, desired, rtc_screen_sequence, true);
                    let transport = waiting.session.transport().clone();
                    rtc_screen_job = Some(Box::pin(async move { transport.post_json(&url, &payload).await.map(|_| ()) }));
                }
            }
        }
        #[cfg(feature = "libwebrtc-av")]
        if rtc_active {
            let enabled = *share_commands.borrow();
            if enabled && share_pump.is_none() && !share_failed {
                share_pump = crate::call_webrtc::ScreenPump::start();
                share_failed = share_pump.is_none();
                updates.send_modify(|state| state.screen_sharing = share_pump.is_some());
            }
            if !enabled && (share_pump.is_some() || share_posted) {
                share_pump = None;
                if let Some(session) = waiting.session.webrtc_mut(key) { session.stop_share(); }
                if share_posted { share_want = Some(false); }
                share_posted = false;
                share_size = None;
                share_failed = false;
                updates.send_modify(|state| state.screen_sharing = false);
            }
            if let Some(pump) = share_pump.as_ref() {
                if pump.finished() && share_size.is_none() {
                    share_failed = true;
                    share_pump = None;
                    updates.send_modify(|state| state.screen_sharing = false);
                } else if let Some(frame) = pump.latest() {
                    let size = (frame.width, frame.height);
                    if let Some(session) = waiting.session.webrtc_mut(key) {
                        if share_size != Some(size) && session.start_share(frame.width, frame.height).is_ok() {
                            share_size = Some(size);
                            share_want = Some(true);
                            crate::live_trace::record("call_screen_share", 0, 1, 0);
                        }
                        session.push_share(frame.buffer);
                    }
                }
            }
            if rtc_share_job.is_none() && share_want.is_some() && share_want != Some(share_posted) {
                let sending = share_want == Some(true);
                let url = waiting.update_media_descriptions.clone();
                let mid = waiting.session.webrtc_mut(key).and_then(|session| session.share_mid());
                if let (Some(url), Some(mid)) = (url, mid) {
                    let Some(next) = share_sequence.checked_add(1) else { return Err(Fault::Protocol); };
                    share_sequence = next;
                    let mut description = serde_json::json!({"mid": mid, "direction": if sending { "sendrecv" } else { "recvonly" }});
                    if sending { description["label"] = serde_json::json!("applicationsharing-video"); }
                    let payload = serde_json::json!({"UpdateMediaDescriptions":{"mediaDescriptions":{
                        "descriptions":[description],"requestId":share_sequence}}});
                    let transport = waiting.session.transport().clone();
                    share_inflight = Some(sending);
                    rtc_share_job = Some(Box::pin(async move { transport.post_json(&url, &payload).await.map(|_| ()) }));
                }
            }
        }
        let share_live = *share_commands.borrow() && {
            #[cfg(feature = "libwebrtc-av")]
            { share_pump.is_some() }
            #[cfg(not(feature = "libwebrtc-av"))]
            { false }
        };
        #[cfg(feature = "libwebrtc-av")]
        if rtc_active && rtc_job.is_none() && rtc_camera_needs_request(rtc_intent, rtc_applied, rtc_started, rtc_failed) {
            let valid_device = waiting.video_control.as_ref().is_some_and(|control| control.has_device());
            let url = waiting.update_media_descriptions.clone();
            let mid = waiting.session.webrtc_mut(key).and_then(|session| session.camera_mid());
            if rtc_intent.enabled && !valid_device {
                rtc_failed = Some(rtc_intent.generation);
                camera.fail(hub, key, waiting, updates, CameraFailure::NoDevice);
            } else if let (Some(url), Some(mid)) = (url, mid) {
                let Some(next) = rtc_sequence.checked_add(1) else { return Err(Fault::Protocol); };
                rtc_sequence = next;
                let mut description = serde_json::json!({"mid":mid,"direction":if rtc_intent.enabled {"sendrecv"} else {"recvonly"}});
                if rtc_intent.enabled { description["label"] = serde_json::json!("main-video"); }
                let payload = serde_json::json!({"UpdateMediaDescriptions":{"mediaDescriptions":{
                    "descriptions":[description],"requestId":rtc_sequence}}});
                let transport = waiting.session.transport().clone();
                rtc_sent = rtc_intent;
                updates.send_modify(|state| state.camera_negotiation_pending = true);
                rtc_job = Some(Box::pin(async move { transport.post_json(&url, &payload).await.map(|_| ()) }));
            } else if rtc_intent.enabled {
                rtc_failed = Some(rtc_intent.generation);
                camera.fail(hub, key, waiting, updates, CameraFailure::Unsupported);
            } else { rtc_applied = false; }
        }
        if camera.prepared.is_some() && camera.reply.is_some() {
            let prepared = camera.prepared.take().unwrap();
            match camera.reply.take().unwrap() {
                call_join::CameraOfferReply::Answer { content_type, blob, acknowledgement } => {
                    match crate::call_video::Remote::parse(&content_type, &blob) {
                        Ok(Some(remote)) => {
                            let transport = waiting.session.transport().clone();
                            let acknowledgement = acknowledgement.filter(|url| transport.validate_response_link(url).is_ok());
                            let control = waiting.video_control.clone().unwrap_or_default();
                            camera.connect = Some(Box::pin(async move {
                                if let Some(url) = acknowledgement {
                                    transport.post_json(&url, &serde_json::Value::Null).await.map_err(|_| ())?;
                                }
                                prepared.connect(remote, control, true).await
                            }));
                            crate::live_trace::record("call_camera_answer", 0, 1, 0);
                        }
                        _ => {
                            drop(prepared);
                            camera.fail(hub, key, waiting, updates, CameraFailure::Answer);
                        }
                    }
                }
                _ => {
                    drop(prepared);
                    camera.fail(hub, key, waiting, updates, CameraFailure::Answer);
                }
            }
        }
        let own = roster.own_audio(&waiting.session.participant().endpoint_id, &waiting.session.participant().participant_id);
        own_audio_seen |= own.is_some();
        let blocked = own.map_or(own_audio_seen, |state|
            state.admission != crate::call_roster::Admission::InCall || state.server_muted == Some(true));
        microphone.server_blocked(blocked);
        if server_blocked != blocked {
            updates.send_modify(|s|s.muted=microphone.is_muted());
            crate::live_trace::record("call_server_audio_gate_v1",0,usize::from(blocked),usize::from(own_audio_seen));
        }
        if server_blocked && !blocked {
            unmute_confirmation_deadline=None;
            updates.send_modify(|s|s.mute_pending=false);
        }
        if server_blocked && !blocked && !microphone.latest().muted {
            confirmed = None;
            commands.mark_changed();
        }
        server_blocked = blocked;
        // Offers received while ACK or ICE was pending must be answered once
        // the live media exists, in the order in which they were delivered.
        while renegotiation_job.is_none() {
            let Some(offer) = pending_negotiations.pop_front() else { break; };
            pending_negotiation_bytes -= offer.bytes();
            renegotiation_job = renegotiation_request(waiting, key, callbacks, &offer.body);
        }
        tokio::select! {biased;
            _=cancel.changed()=>return stopped(subscription),
            event=subscription.next()=>{
                let Some(event)=event else{return stopped(subscription);};
                note_roster(roster, updates, &event);
                match event.kind {
                    CallbackKind::CallEnd|CallbackKind::ConversationEnd=>{crate::call_diagnostics::remote_end(&event.body,false);crate::live_trace::record("call_remote_end_connected",0,event.kind as usize,0);live.request_stop();let _=waiting.session.release_locally(key).await;return Ok(());},
                    CallbackKind::ConversationUpdate=>apply_update(waiting,key,&event.body)?,
                    CallbackKind::AdmitSuccess if admission_pending => {
                        admission_pending = false;
                        crate::live_trace::record("call_admit_callback",0,1,0);
                        updates.send_modify(|state| state.admission_state = AdmissionState::Admitted);
                    },
                    CallbackKind::AdmitFailure if admission_pending => {
                        admission_pending = false;
                        crate::live_trace::record("call_admit_callback",0,2,0);
                        updates.send_modify(|state| state.admission_state = AdmissionState::Failed);
                    },
                    CallbackKind::MediaAnswer | CallbackKind::MediaRejection => {
                        let accepted = camera.receive(&event);
                        crate::live_trace::record("call_camera_callback_v1", 0, usize::from(accepted), usize::from(event.kind == CallbackKind::MediaRejection));
                    }
                    CallbackKind::MediaAcknowledgement => {
                        remember_mixer_leg(waiting, &mut keepalive, &event.body);
                    }
                    CallbackKind::MediaRenegotiation=>{
                        // The new mixer leg publishes its control URLs beside the
                        // offer. Harvest them before this callback is queued.
                        remember_mixer_leg(waiting, &mut keepalive, &event.body);
                        // An answered 1:1 sends this after AddParticipantSuccess.
                        // The original answers with the live local description.
                        // A hollow callback stays connected and sends nothing.
                        if renegotiation_job.is_some() || !pending_negotiations.is_empty() {
                            if pending_negotiations.len() >= 32 || pending_negotiation_bytes.saturating_add(event.bytes()) > 2 * 1024 * 1024 {
                                subscription.stop.overflow();
                                return stopped(subscription);
                            }
                            pending_negotiation_bytes += event.bytes();
                            pending_negotiations.push_back(event);
                        } else {
                            renegotiation_job=renegotiation_request(waiting, key, callbacks, &event.body);
                        }
                    },
                    #[cfg(feature = "libwebrtc-av")]
                    CallbackKind::ControlVideoStreaming => {
                        if let Some(session) = waiting.session.webrtc_mut(key) {
                            let _ = session.handle_camera_control(&event.body);
                        }
                    },
                    CallbackKind::Redirection|CallbackKind::Replacement|CallbackKind::Transfer=>return Err(Fault::Unsupported),
                    _=>{},
                }
            },
            _=health.tick(), if crate::live_trace::enabled()=>{
                live.trace_health();
                #[cfg(feature = "libwebrtc-av")]
                if let Some(session) = waiting.session.webrtc(key) { session.trace_health().await; }
                if let Some(s) = waiting.session.audio_stats(key).await {
                    crate::call_diagnostics::media_path(&s);
                    crate::live_trace::record("call_capture_alignment_v1", s.capture_startup_discarded, 0, 0);
                    crate::live_trace::record("call_decode_level_v1", s.playout.decoded_samples, s.playout.decoded_peak as usize, s.playout.decoded_at_limit as usize);
                    let playback_hz = if s.receive_codec_mask & 2 != 0 { 48000 } else { 8000 };
                    crate::live_trace::record("call_decode_energy_v1", s.playout.decoded_energy, playback_hz, 0);
                    crate::live_trace::record("call_mix_level_v1", s.playout.mix_clipped_samples, s.playout.mix_peak as usize, s.playout.contributing_streams);
                    crate::live_trace::record("call_media_packets", 0, s.packets_sent as usize, s.packets_received as usize);
                    crate::live_trace::record("call_media_send_gaps", s.send_skipped_frames, s.capture_underruns as usize, s.send_errors as usize);
                    crate::live_trace::record("call_media_receive_gaps", s.playout.concealed_samples, s.playout.late_packets as usize, s.playout.dropped_packets as usize);
                    crate::live_trace::record("call_media_rejected", 0, s.receive_auth_failures as usize, s.receive_header_failures as usize);
                    crate::live_trace::record("call_media_payload_v1", s.playout.last_payload_bytes as u64, s.playout.last_payload_type as usize, s.playout.unsupported_payload_packets as usize);
                    crate::live_trace::record("call_media_payload_invalid_v1", s.playout.stream_limit_packets, s.playout.invalid_packet_samples as usize, s.playout.observed_packets as usize);
                    crate::live_trace::record("call_media_device_clock", s.playout.device_silence_frames, s.playout.rendered_frames as usize, s.playout.speaker_dropped_frames as usize);
                    crate::live_trace::record("call_media_playout", s.playout.timeline_resets, s.playout.decoder_errors as usize, s.playout.speaker_dropped_frames as usize);
                    crate::live_trace::record("call_media_network", (s.rtp_quality.jitter_millis() * 1000.0) as u64, s.rtp_quality.estimated_lost_packets as usize, s.control.remote_fraction_lost.map_or(256, usize::from));
                }
            },
            _=monitor.tick()=>{
                if camera.deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                    camera.fail(hub, key, waiting, updates, CameraFailure::Timeout);
                    crate::live_trace::record("call_video_failed", 0, 8, 0);
                }
                if waiting.video_session.as_ref().is_some_and(|session| session.finished()) {
                    if let Some(mut session) = waiting.video_session.take() { session.stop().await; }
                    camera.fail(hub, key, waiting, updates, CameraFailure::TransportEnded);
                }
                if unmute_confirmation_deadline.is_some_and(|deadline|tokio::time::Instant::now()>=deadline) {
                    unmute_confirmation_deadline=None;
                    updates.send_modify(|s|{s.mute_pending=false;s.command_failed=true;});
                }
                if live.is_finished() || live.failure().is_some(){return Err(live.failure().map(audio_fault).unwrap_or(Fault::Audio));}
                if hub.context().is_none(){return Err(Fault::Transport);}
                if !waiting.session.media_running(){return Err(Fault::Media);}
                let level = if microphone.is_muted() { 0 } else { live.microphone_level() };
                let level_changed = updates.borrow().microphone_level != level;
                if level_changed {
                    updates.send_modify(|state| state.microphone_level = level);
                }
            },
            _=heartbeat.tick(),if keepalive_job.is_none()=>{
                let transport=waiting.session.transport().clone();let url=keepalive.url.clone();
                keepalive_job=Some(Box::pin(async move {transport.post_json(&url,&serde_json::json!({"callParticipantUpdate":{}})).await.map(|_|())}) as Request);
            },
            result=request(&mut keepalive_job)=>{keepalive_job=None;if result.is_err(){return Err(Fault::Transport);}},
            result=request(&mut rtc_job)=>{
                rtc_job=None;
                #[cfg(feature = "libwebrtc-av")]
                {
                    crate::live_trace::record("call_webrtc_camera_update", 0, usize::from(rtc_sent.enabled), usize::from(result.is_ok()));
                    if result.is_ok() {
                        rtc_applied = rtc_sent.enabled;
                        if rtc_sent == rtc_intent {
                            if rtc_camera_may_start(rtc_intent, rtc_sent) {
                                if waiting.session.webrtc_mut(key).is_none_or(|session| session.start_camera().is_err()) {
                                    rtc_failed = Some(rtc_sent.generation);
                                    camera.fail(hub, key, waiting, updates, CameraFailure::Prepare);
                                } else {
                                    rtc_started = true;
                                    if let (Some(url), Some(mid)) = (waiting.apply_channel_parameters.clone(),
                                        waiting.session.webrtc_mut(key).and_then(|session| session.camera_mid())) {
                                        let transport = waiting.session.transport().clone();
                                        let payload = camera_capabilities_request(&mid, rtc_sequence);
                                        rtc_capability_job = Some(Box::pin(async move { transport.post_json(&url, &payload).await.map(|_| ()) }));
                                    }
                                    updates.send_modify(|state| {state.camera_negotiation_pending=false;state.camera_negotiation_failure=None;});
                                }
                            } else { updates.send_modify(|state| state.camera_negotiation_pending=false); }
                        }
                    } else if rtc_sent == rtc_intent {
                        rtc_failed = Some(rtc_sent.generation);
                        camera.fail(hub, key, waiting, updates, CameraFailure::Request);
                    }
                }
                #[cfg(not(feature = "libwebrtc-av"))]
                let _ = result;
            },
            result=request(&mut rtc_source_job)=>{
                rtc_source_job=None;
                #[cfg(feature = "libwebrtc-av")]
                {
                    crate::live_trace::record("call_webrtc_video_source", 0, usize::from(rtc_source_sent.is_some()), usize::from(result.is_ok()));
                    if result.is_ok() { rtc_source_applied=Some(rtc_source_sent); rtc_source_failed=None; }
                    else { rtc_source_failed=Some(rtc_source_sent); }
                }
                #[cfg(not(feature = "libwebrtc-av"))]
                let _ = result;
            },
            result=request(&mut rtc_screen_job)=>{
                rtc_screen_job=None;
                #[cfg(feature = "libwebrtc-av")]
                {
                    crate::live_trace::record("call_webrtc_screen_source", 0, usize::from(rtc_screen_sent.is_some()), usize::from(result.is_ok()));
                    if result.is_ok() { rtc_screen_applied=Some(rtc_screen_sent); rtc_screen_failed=None; }
                    else { rtc_screen_failed=Some(rtc_screen_sent); }
                }
                #[cfg(not(feature = "libwebrtc-av"))]
                let _ = result;
            },
            result=request(&mut rtc_share_job)=>{
                rtc_share_job=None;
                #[cfg(feature = "libwebrtc-av")]
                {
                    let sending = share_inflight.unwrap_or(false);
                    share_inflight = None;
                    crate::live_trace::record("call_screen_share_direction", 0, usize::from(sending), usize::from(result.is_ok()));
                    if result.is_ok() {
                        share_posted = sending;
                        if share_want == Some(sending) { share_want = None; }
                    }
                }
                #[cfg(not(feature = "libwebrtc-av"))]
                let _ = result;
            },
            _ = share_tick.tick(), if share_live => {},
            changed = share_commands.changed() => {
                if changed.is_err() { continue; }
                let _ = share_commands.borrow_and_update();
            },
            result=request(&mut rtc_capability_job)=>{
                rtc_capability_job=None;
                crate::live_trace::record("call_webrtc_video_capabilities", 0, usize::from(result.is_ok()), 0);
            },
            changed=camera_commands.changed(), if !camera_closed => {
                if changed.is_err() { camera_closed = true; continue; }
                let command = *camera_commands.borrow_and_update();
                #[cfg(feature = "libwebrtc-av")]
                if rtc_active {
                    rtc_intent = command;
                    if !command.enabled {
                        if let Some(session) = waiting.session.webrtc_mut(key) { session.stop_camera().await; }
                        rtc_started = false;
                    }
                    updates.send_modify(|state| {state.camera_negotiation_pending=false;state.camera_negotiation_failure=None;});
                    continue;
                }
                camera.clear(hub, key);
                updates.send_modify(|state| {
                    state.camera_negotiation_pending = false;
                    state.camera_negotiation_failure = None;
                });
                if !command.enabled { continue; }
                let Some(control) = waiting.video_control.clone() else { continue; };
                // A running track stays negotiated while the camera is off.
                // A completed worker cannot prevent a fresh attempt forever.
                if waiting.video_session.as_ref().is_some_and(|session| session.finished()) {
                    if let Some(mut session) = waiting.video_session.take() { session.stop().await; }
                }
                if waiting.video_session.is_some() { continue; }
                if !control.has_device() {
                    camera.fail(hub, key, waiting, updates, CameraFailure::NoDevice);
                    continue;
                }
                let Some(url) = waiting.media_renegotiation.clone() else {
                    camera.fail(hub, key, waiting, updates, CameraFailure::Unsupported);
                    continue;
                };
                let Some(operation_callbacks) = hub.register_camera_callbacks(key, command.generation) else {
                    camera.fail(hub, key, waiting, updates, CameraFailure::Prepare);
                    continue;
                };
                let transport = waiting.session.transport().clone();
                let sender = waiting.session.participant().clone();
                let audio_offer = waiting.current_offer().to_owned();
                let answer = operation_callbacks.link(CallbackKind::MediaAnswer).to_owned();
                let rejection = operation_callbacks.link(CallbackKind::MediaRejection).to_owned();
                let addresses = addresses.to_vec();
                camera.generation = Some(command.generation);
                camera.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(35));
                updates.send_modify(|state| state.camera_negotiation_pending = true);
                crate::live_trace::record("call_camera_offer_start", 0, 1, 0);
                camera.http = Some(Box::pin(async move {
                    let prepared = crate::call_video::Prepared::bind(&addresses, None, crate::call_video::OfferDirection::SendReceive).await.map_err(|_| ())?;
                    let offer = call_join::camera_offer_sdp(&audio_offer, &prepared.section);
                    let payload = call_join::camera_offer_payload(&sender, &offer, &answer, &rejection);
                    // Return the prepared track even on HTTP failure: a valid
                    // callback can have confirmed the offer before that failure.
                    let body = transport.post_json(&url, &payload).await;
                    Ok((prepared, offer, body))
                }));
            },
            result = async { match &mut camera.http { Some(job) => job.await, None => std::future::pending().await } } => {
                camera.http = None;
                match result {
                    Ok((prepared, offer, body)) => {
                        crate::live_trace::record("call_camera_offer_result", 0, usize::from(body.is_ok()), 0);
                        if let Ok(body) = body {
                            let reply = call_join::camera_offer_reply(&body);
                            let shape = match &reply {
                                call_join::CameraOfferReply::Wait => 0,
                                call_join::CameraOfferReply::Answer { .. } => 1,
                                call_join::CameraOfferReply::Rejected => 2,
                            };
                            crate::live_trace::record("call_camera_offer_body", 0, shape, 0);
                            if camera.reply.is_none() && !matches!(reply, call_join::CameraOfferReply::Wait) {
                                camera.reply = Some(reply);
                            }
                        } else if camera.reply.is_none() {
                            drop(prepared);
                            camera.fail(hub, key, waiting, updates, CameraFailure::Request);
                            continue;
                        }
                        camera.prepared = Some(prepared);
                        camera.offer = Some(offer);
                    }
                    Err(()) => camera.fail(hub, key, waiting, updates, CameraFailure::Prepare),
                }
            },
            result = async { match &mut camera.connect { Some(job) => job.await, None => std::future::pending().await } } => {
                camera.connect = None;
                match result {
                    Ok(session) => {
                        if let Some(offer) = camera.offer.take() { waiting.remember_camera_offer(offer); }
                        waiting.video_session = Some(session);
                        camera.clear(hub, key);
                        updates.send_modify(|state| {
                            state.camera_negotiation_pending = false;
                            state.camera_negotiation_failure = None;
                        });
                        crate::live_trace::record("call_camera_connected", 0, 1, 0);
                    }
                    Err(()) => camera.fail(hub, key, waiting, updates, CameraFailure::Connect),
                }
            },
            command=admit_commands.recv(),if admission_job.is_none()&&!admission_pending=>{
                if let Some(participant_id)=command {
                    crate::live_trace::record("call_admit_request_start",0,0,0);
                    match waiting.session.admit_request(
                        key,
                        &participant_id,
                        callbacks,
                    ) {
                        Ok((transport,url,payload))=>{
                            admission_pending=true;
                            updates.send_modify(|state|state.admission_state=AdmissionState::Sending);
                            admission_job=Some(Box::pin(async move {transport.post_json(&url,&payload).await.map(|_|())}) as Request);
                        },
                        Err(_)=>updates.send_modify(|state|state.admission_state=AdmissionState::Failed),
                    }
                }
            },
            result=request(&mut admission_job)=>{
                admission_job=None;
                crate::live_trace::record("call_admit_http_result",0,usize::from(result.is_ok()),0);
                match result {
                    Ok(()) if admission_pending=>updates.send_modify(|state|state.admission_state=AdmissionState::WaitingForConfirmation),
                    Err(_) if admission_pending=>{
                        admission_pending=false;
                        updates.send_modify(|state|state.admission_state=AdmissionState::Failed);
                    },
                    _=>{},
                }
            },
            changed=volume_commands.changed()=>{
                if changed.is_err(){return stopped(subscription);}
                let percent=(*volume_commands.borrow_and_update()).min(100);
                if let Some(controls)=waiting.session.audio_controls(key){controls.set_speaker_volume_percent(percent);}
                #[cfg(feature = "libwebrtc-av")]
                if let Some(session)=waiting.session.webrtc_mut(key){session.set_volume(percent);}
                updates.send_modify(|state|state.speaker_volume=percent);
            },
            changed=commands.changed()=>{
                if changed.is_err(){return stopped(subscription);}
                commands.borrow_and_update();
                let desired = microphone.latest();
                updates.send_modify(|state| { state.requested_muted=desired.muted;state.muted=microphone.is_muted(); });
                if mute_job.is_some() || server_unmute_job.is_some(){continue;}
                if !desired.muted && server_blocked {
                    let can_request = roster.own_audio(&waiting.session.participant().endpoint_id, &waiting.session.participant().participant_id)
                        .is_some_and(|state| state.admission==crate::call_roster::Admission::InCall && state.server_muted==Some(true));
                    if can_request {
                        match waiting.session.server_unmute_request(key) {
                            Ok((transport,url,payload)) => {
                                in_flight=desired;
                                updates.send_modify(|s|{s.mute_pending=true;s.command_failed=false;});
                                crate::live_trace::record("call_server_unmute_requested_v1",0,0,0);
                                server_unmute_job=Some(Box::pin(async move {
                                    tokio::time::timeout(Duration::from_secs(10),transport.post_json(&url,&payload)).await.unwrap_or(Err(ControlError::TimedOut))
                                }));
                            },
                            Err(_) => updates.send_modify(|s|{s.command_failed=true;s.muted=true;}),
                        }
                    } else { updates.send_modify(|s|{s.command_failed=true;s.muted=true;}); }
                    continue;
                }
                if confirmed.is_some_and(|command| command==desired || (command.muted && desired.muted)){continue;}
                match waiting.session.mute_request(key,desired.muted){
                    Ok((transport,url,payload))=>{
                        in_flight=desired;
                        updates.send_modify(|s|{s.mute_pending=true;s.command_failed=false;});
                        mute_job=Some(Box::pin(async move {transport.post_json(&url,&payload).await.map(|_|())}) as Request);
                    },Err(_)=>{confirmed=None;microphone.confirm(desired,false);updates.send_modify(|s|{s.command_failed=true;s.muted=true;});}
                }
            },
            result=request(&mut renegotiation_job)=>{
                renegotiation_job=None;
                crate::live_trace::record("call_media_renegotiation_result",0,usize::from(result.is_err()),0);
            },
            result=async { match &mut server_unmute_job { Some(job)=>job.await, None=>std::future::pending().await } }=>{
                server_unmute_job=None;
                let accepted = result.as_ref().is_ok_and(|body|roster.confirm_unmute(
                    &waiting.session.participant().endpoint_id, &waiting.session.participant().participant_id, body));
                crate::live_trace::record("call_server_unmute_result_v1",0,usize::from(result.is_ok()),usize::from(accepted));
                unmute_confirmation_deadline=(result.is_ok() && !accepted).then(||tokio::time::Instant::now()+Duration::from_secs(10));
                updates.send_modify(|s|{s.mute_pending=unmute_confirmation_deadline.is_some();s.command_failed=result.is_err();s.muted=microphone.is_muted();});
                // Empty success remains blocked until a versioned roster update.
                // A newer local mute always wins; no HTTP response opens audio.
                if accepted && microphone.latest()==in_flight && !in_flight.muted {
                    confirmed=None;
                    commands.mark_changed();
                }
            },
            result=request(&mut mute_job)=>{
                if !waiting.session.media_running(){return Err(Fault::Media);}
                mute_job=None;
                confirmed=result.is_ok().then_some(in_flight);
                let muted=microphone.confirm(in_flight,result.is_ok());
                updates.send_modify(|s|{s.muted=muted;s.mute_pending=false;s.command_failed=result.is_err();});
                // A newer intent may already have been consumed while this
                // request was pending. Reconcile it after the older response.
                if microphone.latest()!=in_flight {commands.mark_changed();}
            },
        }
    }
}

fn host_addresses() -> Result<Vec<IpAddr>, Fault> {
    let mut pointer = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut pointer) } != 0 {
        return Err(Fault::Route);
    }
    struct Interfaces(*mut libc::ifaddrs);
    impl Drop for Interfaces {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) }
        }
    }
    let owner = Interfaces(pointer);
    let mut current = owner.0;
    let mut result = Vec::new();
    while !current.is_null() {
        let item = unsafe { &*current };
        if !item.ifa_addr.is_null()
            && item.ifa_flags & libc::IFF_UP as u32 != 0
            && item.ifa_flags & libc::IFF_LOOPBACK as u32 == 0
            && unsafe { (*item.ifa_addr).sa_family as i32 } == libc::AF_INET
        {
            let address = unsafe { &*(item.ifa_addr as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
            if !ip.is_unspecified() && !ip.is_multicast() && !ip.is_loopback() {
                let ip = IpAddr::V4(ip);
                if !result.contains(&ip) {
                    result.push(ip);
                }
            }
        }
        if result.len() >= 16 {
            break;
        }
        current = item.ifa_next;
    }
    if result.is_empty() {
        Err(Fault::Route)
    } else {
        Ok(result)
    }
}

fn offers_wide_receive(opus_available: bool, has_callee: bool) -> bool {
    let _ = has_callee;
    opus_available
}

fn webrtc_requested(link: &JoinLink) -> bool {
    #[cfg(feature = "libwebrtc-av")]
    { let _ = link; std::env::var_os("TEAMS_LINUX_WEBRTC_AV").as_deref() == Some(std::ffi::OsStr::new("1")) }
    #[cfg(not(feature = "libwebrtc-av"))]
    { let _ = link; false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    use tiny_http::{Response, Server};

    #[cfg(feature = "libwebrtc-av")]
    #[test]
    fn camera_updates_serialize_user_intent_and_do_not_restart_after_a_late_answer() {
        let on = CameraCommand { generation: 1, enabled: true };
        let off = CameraCommand { generation: 2, enabled: false };
        assert!(rtc_camera_needs_request(on, false, false, None));
        assert!(!rtc_camera_may_start(off, on));
        assert!(rtc_camera_needs_request(off, true, false, None));
        assert!(!rtc_camera_needs_request(on, false, false, Some(1)));
        assert!(rtc_camera_needs_request(CameraCommand { generation: 3, enabled: true }, true, false, Some(1)));
        let body = camera_source_request("2", 42, Some(51), 7, false);
        let p = &body["applyChannelParameters"]["multiChannelParameter"];
        assert_eq!(p["mids"], serde_json::json!(["2"]));
        let video: serde_json::Value = serde_json::from_str(p["mediaParameter"].as_str().unwrap()).unwrap();
        assert_eq!(video["controlVideoStreaming"]["controlInfo"]["sourceId"], 51);
        assert_eq!(video["controlVideoStreaming"]["controlInfo"]["streamMsid"], 42);
        assert_eq!(video["controlVideoStreaming"]["controlInfo"]["fmtParams"][0]["max-fs"], 3600);
        let share = camera_source_request("11", 9, Some(77), 8, true);
        let share_body: serde_json::Value = serde_json::from_str(share["applyChannelParameters"]["multiChannelParameter"]["mediaParameter"].as_str().unwrap()).unwrap();
        assert_eq!(share_body["controlVideoStreaming"]["controlInfo"]["fmtParams"][0]["max-fs"], 8160);
        assert_eq!(share_body["controlVideoStreaming"]["controlInfo"]["sourceId"], 77);
        let caps = camera_capabilities_request("1", 3);
        let params: serde_json::Value = serde_json::from_str(caps["applyChannelParameters"]["multiChannelParameter"]["mediaParameter"].as_str().unwrap()).unwrap();
        assert_eq!(params["maxVideoSendCapabilities"]["max-width"], 640);
        assert_eq!(params["maxVideoSendCapabilities"]["max-layers"], 2);
    }

    #[test]
    fn meeting_wait_is_not_a_call_and_a_call_wait_is_not_a_meeting() {
        assert_eq!(Phase::Waiting.status(false), "Confirming the meeting…");
        assert_eq!(Phase::Waiting.status(true), "Ringing…");
        assert!(!Phase::Waiting.status(false).contains("Ringing"));
        assert!(!Phase::Waiting.status(true).contains("meeting"));
        assert_eq!(Phase::Ended.status(false), "The meeting ended.");
        assert_eq!(Phase::Ended.status(true), "The call ended.");
    }

    #[test]
    fn direct_call_uses_the_same_48k_receive_as_the_official_headset_capture() {
        assert!(offers_wide_receive(true, true));
        assert!(offers_wide_receive(true, false));
        assert!(!offers_wide_receive(false, true));
        assert!(!offers_wide_receive(false, false));
    }

    fn hub() -> CallHub {
        let hub = CallHub::default();
        hub.handle_event(ost::event_hub::TrouterEvent{kind:ost::event_hub::TrouterEventKind::TransportConnected,
            payload_json:json!({"messagingRoot":"https://fixture.trouter.skype.com/opaque/","transportEndpointId":"fixture-trouter-id","ttlSeconds":300}).to_string()});
        hub
    }
    fn deliver(hub: &CallHub, url: &str, body: Value) {
        let event = ost::trouter::data_frame_routed_event(
            &json!({"method":"POST","url":url,"body":body}).to_string(),
        )
        .unwrap();
        hub.handle_event(event);
    }
    #[test]
    fn camera_answer_before_http_is_retained_and_cancel_invalidates_it() {
        let hub = hub();
        let key = CallKey { generation: 9, id: native_id() };
        let callbacks = hub.context().unwrap().prepare_callbacks(key.clone()).unwrap();
        let mut subscription = hub.subscribe(key.clone(), callbacks).unwrap();
        let operation_callbacks = hub.register_camera_callbacks(&key, 17).unwrap();
        let mut operation = CameraOperation {
            generation: Some(17),
            http: Some(Box::pin(std::future::pending())),
            ..Default::default()
        };
        deliver(&hub, operation_callbacks.link(CallbackKind::MediaAnswer), json!({
            "mediaAnswer": { "mediaContent": { "contentType": "application/sdp", "blob": "v=0\r\n" } }
        }));
        let answer = subscription.try_next().unwrap();
        assert!(operation.prepared.is_none(), "HTTP has not finished preparing the track");
        assert!(operation.receive(&answer));
        assert!(matches!(operation.reply, Some(call_join::CameraOfferReply::Answer { .. })));
        assert!(!operation.receive(&answer), "duplicate answer must not replace the retained answer");
        operation.clear(&hub, &key);
        assert!(!operation.receive(&answer), "off cannot consume a retained or delayed answer");
        deliver(&hub, operation_callbacks.link(CallbackKind::MediaAnswer), json!({"mediaAnswer":{}}));
        assert!(subscription.try_next().is_none());
    }

    fn identity(hub: &CallHub) -> LocalParticipant {
        LocalParticipant {
            id: "8:orgid:aaaaaaaa-1111-2222-3333-444444444444".into(),
            display_name: "Pessoa fictícia".into(),
            endpoint_id: hub.agent_endpoint(),
            participant_id: native_id(),
            language_id: "pt-BR".into(),
        }
    }
    fn link() -> JoinLink {
        JoinLink::parse("https://teams.microsoft.com/meet/123456?p=fixture").unwrap()
    }
    #[tokio::test]
    async fn cancellation_releases_a_pending_preparation_without_opening_audio() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut client = Client::spawn(
            hub(),
            CallKey { generation: 1, id: native_id() },
            link(),
            Box::pin(async move {
                entered_tx.send(()).unwrap();
                std::future::pending::<Result<PreparedCall, Fault>>().await
            }),
            Box::new(|| panic!("cancelled preparation must not open audio")),
        ).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        client.cancel();
        tokio::time::timeout(Duration::from_secs(2), Client::completed(client.changes())).await.unwrap();
        let snapshot = client.snapshot();
        assert_eq!(snapshot.phase, Phase::Ended);
        assert_eq!(snapshot.fault, None);
    }
    #[test]
    fn closed_watch_keeps_worker_terminal_state_instead_of_synthesizing_worker_fault() {
        let mut snapshot = Snapshot { phase: Phase::Connecting, ..Default::default() };
        let terminal = Snapshot { phase: Phase::Ended, ..Default::default() };
        assert!(preserve_terminal_after_close(&mut snapshot, terminal.clone()));
        assert_eq!(snapshot.phase, Phase::Ended);
        assert_eq!(snapshot.fault, terminal.fault);

        let mut snapshot = Snapshot { phase: Phase::Connecting, ..Default::default() };
        assert!(!preserve_terminal_after_close(
            &mut snapshot,
            Snapshot { phase: Phase::Connecting, ..Default::default() },
        ));
        snapshot.phase = Phase::Failed;
        snapshot.fault = Some(Fault::Worker);
        assert_eq!(snapshot.fault, Some(Fault::Worker));
    }
    async fn until(client: &mut Client, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let state = client.snapshot();
                if predicate(&state) {
                    return state;
                }
                assert!(!state.phase.terminal(), "unexpected final state: {state:?}");
                let mut changes = client.changes();
                changes.changed().await.unwrap();
            }
        })
        .await
        .expect("native call timed out")
    }
    #[test]
    fn negotiation_failures_keep_their_stage_without_private_response_data() {
        assert_eq!(join_fault(call_join::JoinError::InvalidController),Fault::Negotiation(ProtocolIssue::Controller));
        assert_eq!(join_fault(call_join::JoinError::InvalidAcceptance),Fault::Negotiation(ProtocolIssue::Acceptance));
        assert_eq!(join_fault(call_join::JoinError::Control(ControlError::InvalidResponse)),Fault::Negotiation(ProtocolIssue::Response));
        assert_eq!(join_fault(call_join::JoinError::Control(ControlError::Http(403))),Fault::Http(403));
        assert_ne!(Fault::Negotiation(ProtocolIssue::Controller).message(),Fault::Negotiation(ProtocolIssue::Acceptance).message());
    }

    #[test]
    fn pending_activation_offers_mute_without_permitting_another_activation() {
        let mut state=Snapshot { phase:Phase::Connected,muted:true,requested_muted:false,mute_pending:true,can_mute:true,..Default::default() };
        assert_eq!(state.microphone_action(),Some(true));
        state.requested_muted=true;
        assert_eq!(state.microphone_action(),None);
        state.mute_pending=false;
        state.muted=false;
        state.can_mute=false;
        assert_eq!(state.microphone_action(),Some(true));
        state.muted=true;
        assert_eq!(state.microphone_action(),None);
        state.phase=Phase::Ended;
        assert_eq!(state.microphone_action(),None);
    }

    #[tokio::test]
    async fn coordinator_routes_real_callbacks_negotiates_audio_keepalive_mute_and_all_exit_paths()
    {
        for ending in [0, 1, 2, 3, 4, 5, 6] {
            let relay_fixture = if ending == 0 {
                Some(crate::call_turn::tests::fixture().await)
            } else {
                None
            };
            let (relay_request, grant_worker) = if let Some(fixture) = &relay_fixture {
                let grant_server = Server::http("127.0.0.1:0").unwrap();
                let base = format!("http://{}", grant_server.server_addr());
                let request = crate::call_relay::RelayRequest::fixture(&base)
                    .with_relay_fixture(fixture.server);
                let worker = std::thread::spawn(move || {
                    let req = grant_server
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    assert_eq!(req.url(), "/trap/tokens");
                    req.respond(Response::from_string(json!({"expires":600,"realm":"rtcmedia","username":"fixture-user","password":"fixture-secret"}).to_string())).unwrap();
                });
                (Some(request), Some(worker))
            } else {
                (None, None)
            };

            let hub = hub();
            assert_ne!(hub.agent_endpoint(), "fixture-trouter-id");
            assert_eq!(hub.agent_endpoint(), hub.clone().agent_endpoint());
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let route = CallRoute::fixture(&base);
            let transport = CallTransport::fixture(&base);
            let person = identity(&hub);
            let hb = Arc::new(AtomicUsize::new(0));
            let answers = Arc::new(AtomicUsize::new(0));
            let leaving = Arc::new(AtomicUsize::new(0));
            let finish = Arc::new(AtomicBool::new(false));
            let release_unmute = Arc::new(AtomicBool::new(false));
            let remute_seen = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = mpsc::channel();
            let (audio_tx, audio_rx) = mpsc::channel();
            let worker_hub = hub.clone();
            let worker_base = base.clone();
            let count = hb.clone();
            let answered = answers.clone();
            let leaves = leaving.clone();
            let done = finish.clone();
            let release = release_unmute.clone();
            let remuted = remute_seen.clone();
            let worker = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();
                let mut req = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(req.url(), "/epconv");
                let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
                assert_eq!(body["participants"]["to"], json!([]));
                assert_eq!(body["endpointState"]["state"]["isMuted"], true);
                let callbacks = body["callInvitation"]["links"].clone();
                let roster_callback = body["conversationRequest"]["roster"]["rosterUpdate"].as_str().unwrap().to_owned();
                let local_offer = body["callInvitation"]["mediaContent"]["blob"].as_str().unwrap().to_owned();
                let remote = crate::call_media::RemoteAudio::parse(
                    "application/sdp",
                    body["callInvitation"]["mediaContent"]["blob"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let mut peer = runtime
                    .block_on(PreparedAudio::bind(
                        "127.0.0.1:0".parse().unwrap(),
                        &["127.0.0.1".parse().unwrap()],
                    ))
                    .unwrap();
                if ending == 0 {
                    peer.enable_opus();
                }
                if ending < 4 {
                    peer.dtls_answer(ost::calling::dtls::Setup::Passive).unwrap();
                } else {
                    let offer = body["callInvitation"]["mediaContent"]["blob"].as_str().unwrap();
                    assert!(offer.contains(" RTP/SAVP "));
                    assert!(offer.contains("a=crypto:"));
                    assert!(!offer.contains("a=fingerprint:"));
                }
                // This scenario validates the audio-only entry retained in
                // production. Video transport is exercised by call_video's
                // synthetic ICE/SRTP and TURN tests, not an invented offer here.
                assert!(!body["callInvitation"]["mediaContent"]["blob"].as_str().unwrap().contains("m=video "));
                let description = peer.offer().to_owned();
                let peer_port = peer.local_addr().unwrap().port();
                let (mic_tx, mic_rx) = mpsc::sync_channel(3);
                let (speaker_tx, speaker_rx) = mpsc::sync_channel(8);
                let media = runtime.spawn(peer.connect(
                    remote,
                    ost::calling::media::PcmAudioIo {
                        microphone: mic_rx,
                        speaker: speaker_tx,
                        initially_muted: false,
                    },
                    false,
                ));
                req.respond(Response::from_string(json!({"sequenceNumber":if ending==3 {-1} else {1},"conversationController":format!("{worker_base}/controller"),"links":{"leave":format!("{worker_base}/leave"),"updateEndpointState":format!("{worker_base}/mute")}}).to_string())).unwrap();
                deliver(
                    &worker_hub,
                    callbacks["acceptance"].as_str().unwrap(),
                    json!({"callAcceptance":{"mediaContent":{"contentType":"application/sdp","blob":description.clone()},"callKeepAliveInterval":1,
                    "links":{"acknowledgement":format!("{worker_base}/ack"),"callLeg":format!("{worker_base}/leg")}}}),
                );
                ready_tx
                    .send(callbacks["end"].as_str().unwrap().to_owned())
                    .unwrap();
                // The real renegotiation callback is registered in ACK.
                let mut mute_attempts = 0;
                while !done.load(Ordering::Acquire) {
                    let Some(mut req) = server.recv_timeout(Duration::from_millis(50)).unwrap()
                    else {
                        continue;
                    };
                    match req.url() {
                        "/ack" => {
                            let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
                            let ack_links = body["callAcceptanceAcknowledgement"]["links"].clone();
                            if ending == 0 {
                                // Hold the ACK until the callback has left the hub,
                                // proving it arrived while setup was still pending.
                                for number in 1..=2 {
                                    deliver(&worker_hub, ack_links["mediaRenegotiation"].as_str().unwrap(), json!({
                                        "mediaNegotiation": {
                                            "callModalities": ["Audio"],
                                            "links": {"mediaAnswer": format!("{worker_base}/media-answer-{number}")},
                                            "mediaContent": {"contentType": "application/sdp", "blob": description.clone()}
                                        }
                                    }));
                                }
                                deliver(&worker_hub, &roster_callback, json!({"type":"Full", "participants": {
                                    "8:orgid:fixture": {"displayName":"Pessoa inicial"}
                                }}));
                                deliver(&worker_hub, &roster_callback, json!({"type":"Delta", "participants": {
                                    "8:orgid:fixture": {"displayName":"Pessoa atualizada"}
                                }}));
                                let started = std::time::Instant::now();
                                while worker_hub.queued_events() != Some(0) {
                                    assert!(started.elapsed() < Duration::from_secs(3));
                                    std::thread::sleep(Duration::from_millis(2));
                                }
                            }
                            req.respond(Response::empty(204)).unwrap();
                            let mut peer =
                                runtime.block_on(async { media.await.unwrap().unwrap() });
                            audio_tx
                                .send((speaker_rx, mic_tx, peer_port, ack_links.clone()))
                                .unwrap();
                            // Media remains owned here while the coordinator is active.
                            while !done.load(Ordering::Acquire) {
                                if let Some(mut req) =
                                    server.recv_timeout(Duration::from_millis(50)).unwrap()
                                {
                                    let url = req.url().to_owned();
                                    let body: Value = serde_json::from_reader(req.as_reader())
                                        .unwrap_or(Value::Null);
                                    match url.as_str() {
                                        "/leg" => {
                                            assert_eq!(body, json!({"callParticipantUpdate":{}}));
                                            count.fetch_add(1, Ordering::SeqCst);
                                        }
                                        "/mute" => {
                                            mute_attempts += 1;
                                            let requested_muted = body["endpointState"]["state"]["isMuted"].as_bool().unwrap();
                                            assert_eq!(requested_muted, ending==3 && mute_attempts==2);
                                            if ending==3 && mute_attempts==1 {
                                                // Hold activation's HTTP response while the user
                                                // asks to mute. No concurrent endpoint requests.
                                                let started = std::time::Instant::now();
                                                while !release.load(Ordering::Acquire) {
                                                    assert!(started.elapsed()<Duration::from_secs(3));
                                                    if let Some(extra) = server.try_recv().unwrap() {
                                                        assert_eq!(extra.url(),"/leg","mute requests must not overlap");
                                                        count.fetch_add(1,Ordering::SeqCst);
                                                        extra.respond(Response::empty(204)).unwrap();
                                                    }
                                                    std::thread::sleep(Duration::from_millis(2));
                                                }
                                            }
                                            if ending==3 && mute_attempts==2 { remuted.store(true, Ordering::Release); }
                                            if ending == 0 && mute_attempts == 1 {
                                                req.respond(Response::empty(500)).unwrap();
                                                continue;
                                            }
                                            assert!(
                                                body["endpointState"]["endpointStateSequenceNumber"]
                                                    .as_u64()
                                                    .unwrap()
                                                    >= 2
                                            );
                                        }
                                        "/leave" => {
                                            assert_eq!(body["callTransactionEnd"]["code"], 0);
                                            leaves.fetch_add(1, Ordering::SeqCst);
                                        }
                                        "/media-answer-1" | "/media-answer-2" | "/media-answer-3" => {
                                            let blob = body["mediaAnswer"]["mediaContent"]["blob"].as_str().unwrap();
                                            assert!(blob.starts_with(local_offer.as_str()));
                                            if description.lines().any(|line| line.starts_with("m=video "))
                                                && !local_offer.lines().any(|line| line.starts_with("m=video "))
                                            {
                                                assert!(blob.contains("m=video 0 "));
                                                assert!(blob.contains("a=inactive"));
                                            }
                                            let order = answered.fetch_add(1, Ordering::SeqCst) + 1;
                                            assert_eq!(url, format!("/media-answer-{order}"));
                                            if order == 1 {
                                                // A third offer while the first response is in flight
                                                // must wait behind the second, without being lost.
                                                deliver(&worker_hub, ack_links["mediaRenegotiation"].as_str().unwrap(), json!({
                                                    "mediaNegotiation": {
                                                        "callModalities": ["Audio"],
                                                        "links": {"mediaAnswer": format!("{worker_base}/media-answer-3")},
                                                        "mediaContent": {"contentType": "application/sdp", "blob": description.clone()}
                                                    }
                                                }));
                                                let started = std::time::Instant::now();
                                                while worker_hub.queued_events() != Some(0) {
                                                    assert!(started.elapsed() < Duration::from_secs(3));
                                                    std::thread::sleep(Duration::from_millis(2));
                                                }
                                            }
                                        }
                                        _ => panic!("unexpected request"),
                                    }
                                    req.respond(Response::empty(204)).unwrap();
                                }
                            }
                            runtime.block_on(peer.stop());
                            return;
                        }
                        _ => panic!("unexpected setup request"),
                    }
                }
            });
            let prep: Prep = Box::pin(async move {
                Ok(PreparedCall {
                    resolved_invitation: None,
                    offer_profile: if ending >= 4 { OfferProfile::MacSdes } else { OfferProfile::Dtls },
                    route,
                    transport,
                    identity: person,
                    addresses: vec!["127.0.0.1".parse().unwrap()],
                    stun_service: None,
                    relay_request,
                })
            });
            let mut invitation=link();
            if ending==1{invitation.camera=Some((crate::video::CameraDevice{path:"/dev/video9999".into(),label:"Synthetic: never opened".into()},true));}
            if ending==5{invitation.camera=Some((crate::video::CameraDevice{path:"/dev/video9999".into(),label:"Synthetic: never opened".into()},false));}
            if ending==6{invitation.camera=Some((crate::video::CameraDevice{path:"/dev/video9999".into(),label:"Synthetic: never opened".into()},true));}
            let mut client = Client::spawn(
                hub.clone(),
                CallKey {
                    generation: 7,
                    id: native_id(),
                },
                invitation,
                prep,
                Box::new(|| LiveAudio::synthetic().map_err(|_| Fault::Audio)),
            )
            .unwrap();
            if matches!(ending, 1 | 6) { client.set_camera_enabled(false); }
            if matches!(ending, 0 | 3) {
                // Prejoin intent arrives before signaling/DTLS are ready. It
                // must be retained, without enabling the local microphone yet.
                client.set_muted(false);
            }
            let remote_end = tokio::task::spawn_blocking(move || {
                ready_rx.recv_timeout(Duration::from_secs(8)).unwrap()
            })
            .await
            .unwrap();
            let connected = until(&mut client, |s| s.phase == Phase::Connected).await;
            if ending==0 { assert_eq!(connected.roster[0].label, "Pessoa atualizada"); }
            if ending==1{assert!(!connected.camera_available);assert!(!connected.camera_enabled);assert!(!connected.camera_sending);assert!(connected.camera_negotiation_failure.is_none());}
            if ending==5{assert!(!connected.camera_available);assert!(!connected.camera_enabled);assert!(!connected.camera_sending);}
            if ending==6{assert!(!connected.camera_available);assert!(!connected.camera_enabled);assert!(!connected.camera_sending);assert!(connected.camera_negotiation_failure.is_none());}
            if ending != 3 { assert!(connected.muted); }
            if ending != 3 { assert_eq!(connected.microphone_level, 0); }
            assert!(connected.can_mute);
            client.set_speaker_volume(35);
            until(&mut client, |s| s.speaker_volume == 35).await;
            client.set_speaker_volume(255);
            until(&mut client, |s| s.speaker_volume == 100).await;
            let (speaker, microphone, peer_port, ack_links) =
                tokio::task::spawn_blocking(move || {
                    audio_rx.recv_timeout(Duration::from_secs(5)).unwrap()
                })
                .await
                .unwrap();
            if !matches!(ending, 0 | 3) { client.set_muted(false); }
            if ending == 0 {
                let failed = until(&mut client, |s| s.command_failed && !s.mute_pending).await;
                assert!(
                    failed.muted,
                    "a rejected unmute must never expose microphone samples"
                );
                let mut frames = 0;
                tokio::time::timeout(Duration::from_secs(2), async {
                    while frames < 3 {
                        while let Ok(frame) = speaker.try_recv() {
                            assert!(frame.iter().all(|sample| i32::from(*sample).abs() < 1000), "prejoin intent leaked speech before confirmation");
                            frames += 1;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.unwrap();
                client.set_muted(false);
            }
            if ending == 3 {
                until(&mut client, |s| s.mute_pending && !s.requested_muted).await;
                client.set_muted(true);
                let muted = client.snapshot();
                assert!(muted.muted && muted.requested_muted, "local mute must not wait for HTTP");
                assert_eq!(muted.microphone_level, 0, "mute must immediately hide the input level");
                let mut frames = 0;
                tokio::time::timeout(Duration::from_secs(2), async {
                    while frames < 3 {
                        while let Ok(frame) = speaker.try_recv() {
                            assert!(frame.iter().all(|sample|i32::from(*sample).abs()<1000),"activation in flight leaked speech after mute");
                            frames += 1;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.unwrap();
                release_unmute.store(true, Ordering::Release);
                until(&mut client, |s| remute_seen.load(Ordering::Acquire) && s.muted && !s.mute_pending).await;
                // Only a fresh activation may now release microphone frames.
                client.set_muted(false);
            }
            until(&mut client, |s| !s.muted && !s.mute_pending).await;
            until(&mut client, |s| s.microphone_level > 0).await;
            microphone.send(vec![-9000; 160]).unwrap();
            let heard = tokio::task::spawn_blocking(move || {
                for _ in 0..20 {
                    if speaker
                        .recv_timeout(Duration::from_secs(1))
                        .unwrap()
                        .iter()
                        .any(|s| *s > 5000)
                    {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap();
            assert!(
                heard,
                "synthetic microphone did not reach the peer over SRTP"
            );
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(hb.load(Ordering::SeqCst) > 0);
            if ending == 0 {
                assert_eq!(answers.load(Ordering::SeqCst), 3, "offers must be answered once and in order");
            }
            match ending {
                0 => client.cancel(),
                1 => deliver(&hub, &remote_end, json!({"callEnd":{}})),
                2 => hub.invalidate(),
                _ => deliver(
                    &hub,
                    ack_links["mediaRenegotiation"].as_str().unwrap(),
                    json!({"mediaNegotiation":{}}),
                ),
            }
            if ending >= 3 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let stayed = client.snapshot();
                assert_eq!(stayed.phase, Phase::Connected);
                assert!(stayed.fault.is_none());
                client.cancel();
            }
            let final_state = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let state = client.snapshot();
                    if state.phase.terminal() {
                        break state;
                    }
                    let mut rx = client.changes();
                    rx.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            assert_eq!(
                final_state.phase,
                if ending == 2 {
                    Phase::Failed
                } else {
                    Phase::Ended
                }
            );
            if ending == 2 {
                assert_eq!(final_state.fault, Some(Fault::Transport));
            }
            if ending >= 3 {
                assert!(final_state.fault.is_none());
            }
            assert_eq!(leaving.load(Ordering::SeqCst), usize::from(ending != 1));
            finish.store(true, Ordering::Release);
            worker.join().unwrap();
            let _rebound = std::net::UdpSocket::bind(("127.0.0.1", peer_port)).unwrap();
            if let Some(worker) = grant_worker {
                worker.join().unwrap();
            }
            if let Some(fixture) = relay_fixture {
                assert!(fixture.forwarded.load(Ordering::SeqCst) > 0);
                tokio::time::timeout(Duration::from_secs(1), fixture.released)
                    .await
                    .unwrap()
                    .unwrap();
                fixture.worker.await.unwrap();
            }
        }
    }
    #[tokio::test]
    async fn cancellation_during_ack_is_normal_exit_and_never_publishes_connected() {
        let hub = hub();
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let route = CallRoute::fixture(&base);
        let transport = CallTransport::fixture(&base);
        let person = identity(&hub);
        let worker_hub = hub.clone();
        let (ack_tx, ack_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
            let callback = body["callInvitation"]["links"]["acceptance"]
                .as_str()
                .unwrap()
                .to_owned();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut peer = runtime
                .block_on(PreparedAudio::bind(
                    "127.0.0.1:0".parse().unwrap(),
                    &["127.0.0.1".parse().unwrap()],
                ))
                .unwrap();
            peer.dtls_answer(ost::calling::dtls::Setup::Passive).unwrap();
            req.respond(Response::from_string(json!({"conversationController":format!("{base}/controller"),"links":{"leave":format!("{base}/leave")}}).to_string())).unwrap();
            deliver(
                &worker_hub,
                &callback,
                json!({"callAcceptance":{"mediaContent":{"contentType":"application/sdp","blob":peer.offer()},"callKeepAliveInterval":1,"links":{"acknowledgement":format!("{base}/ack"),"callLeg":format!("{base}/leg")}}}),
            );
            let req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(req.url(), "/ack");
            ack_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let _ = req.respond(Response::empty(204));
            let mut req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(req.url(), "/leave");
            let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
            assert_eq!(body["callTransactionEnd"]["code"], 487);
            req.respond(Response::empty(204)).unwrap();
        });
        let mut client = Client::spawn(
            hub,
            CallKey {
                generation: 1,
                id: native_id(),
            },
            link(),
            Box::pin(async move {
                Ok(PreparedCall {
                    resolved_invitation: None,
                    offer_profile: OfferProfile::Dtls,
                    route,
                    transport,
                    identity: person,
                    addresses: vec!["127.0.0.1".parse().unwrap()],
                    stun_service: None,
                    relay_request: None,
                })
            }),
            Box::new(|| LiveAudio::synthetic().map_err(|_| Fault::Audio)),
        )
        .unwrap();
        ack_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(client.snapshot().phase, Phase::Connecting);
        client.cancel();
        release_tx.send(()).unwrap();
        let final_state = until(&mut client, |s| s.phase.terminal()).await;
        assert_eq!(final_state.phase, Phase::Ended);
        assert_eq!(final_state.fault, None);
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn remote_end_during_connecting_cancels_join_without_publishing_connected() {
        let hub = hub();
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let route = CallRoute::fixture(&base);
        let transport = CallTransport::fixture(&base);
        let person = identity(&hub);
        let worker_hub = hub.clone();
        let (ack_tx, ack_rx) = mpsc::channel();
        let (end_url_tx, end_url_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let leaves = Arc::new(AtomicUsize::new(0));
        let worker_leaves = leaves.clone();
        let worker = std::thread::spawn(move || {
            let mut req = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
            let callbacks = body["callInvitation"]["links"].clone();
            end_url_tx.send(callbacks["end"].as_str().unwrap().to_owned()).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut peer = runtime.block_on(PreparedAudio::bind(
                "127.0.0.1:0".parse().unwrap(), &["127.0.0.1".parse().unwrap()],
            )).unwrap();
            peer.dtls_answer(ost::calling::dtls::Setup::Passive).unwrap();
            req.respond(Response::from_string(json!({
                "conversationController":format!("{base}/controller"),
                "links":{"leave":format!("{base}/leave")}
            }).to_string())).unwrap();
            deliver(&worker_hub, callbacks["acceptance"].as_str().unwrap(), json!({
                "callAcceptance":{"mediaContent":{"contentType":"application/sdp","blob":peer.offer()},
                "callKeepAliveInterval":1,"links":{"acknowledgement":format!("{base}/ack"),"callLeg":format!("{base}/leg")}}
            }));
            let req = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(req.url(), "/ack");
            ack_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            req.respond(Response::empty(204)).unwrap();
            let mut req = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(req.url(), "/leave");
            worker_leaves.fetch_add(1, Ordering::AcqRel);
            let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
            assert_eq!(body["callTransactionEnd"]["code"], 487);
            req.respond(Response::empty(204)).unwrap();
        });
        let mut client = Client::spawn(
            hub.clone(),
            CallKey { generation: 1, id: native_id() },
            link(),
            Box::pin(async move { Ok(PreparedCall {
                    resolved_invitation: None,
                    offer_profile: OfferProfile::Dtls,
                route, transport, identity: person, addresses: vec!["127.0.0.1".parse().unwrap()],
                stun_service: None, relay_request: None,
            }) }),
            Box::new(|| LiveAudio::synthetic().map_err(|_| Fault::Audio)),
        ).unwrap();
        ack_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(client.snapshot().phase, Phase::Connecting);
        let end_url = end_url_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        deliver(&hub, &end_url, json!({"callEnd":{}}));
        tokio::time::timeout(Duration::from_secs(3), async {
            while client.stop.reason() != StopReason::RemoteEnded {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        release_tx.send(()).unwrap();
        let final_state = until(&mut client, |state| state.phase.terminal()).await;
        assert_eq!(final_state.phase, Phase::Ended);
        assert_eq!(final_state.fault, None);
        assert_eq!(leaves.load(Ordering::Acquire), 1);
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn rejected_create_surfaces_only_the_status_and_never_starts_audio() {
        let hub = hub();
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let person = identity(&hub);
        let route = CallRoute::fixture(&base);
        let transport = CallTransport::fixture(&base);
        let worker = std::thread::spawn(move || {
            let req = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            req.respond(
                Response::from_string("private-response-token-never-display").with_status_code(503),
            )
            .unwrap();
        });
        let mut client = Client::spawn(
            hub,
            CallKey {
                generation: 1,
                id: native_id(),
            },
            link(),
            Box::pin(async move {
                Ok(PreparedCall {
                    resolved_invitation: None,
                    offer_profile: OfferProfile::Dtls,
                    route,
                    transport,
                    identity: person,
                    addresses: vec!["127.0.0.1".parse().unwrap()],
                    stun_service: None,
                    relay_request: None,
                })
            }),
            Box::new(|| panic!("rejected create must not open audio")),
        )
        .unwrap();
        let state = until(&mut client, |s| s.phase.terminal()).await;
        assert_eq!(state.fault, Some(Fault::Http(503)));
        assert!(state.fault.unwrap().user_message().contains("HTTP 503"));
        assert!(!format!("{state:?}").contains("private-response"));
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn cancelling_creation_compensates_late_controller_without_opening_devices() {
        let hub = hub();
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let route = CallRoute::fixture(&base);
        let transport = CallTransport::fixture(&base);
        let person = identity(&hub);
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let used = Arc::new(AtomicUsize::new(0));
        let calls = used.clone();
        let worker = std::thread::spawn(move || {
            let req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            arrived_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            req.respond(Response::from_string(json!({"conversationController":format!("{base}/controller"),"links":{"leave":format!("{base}/leave")}}).to_string())).unwrap();
            let mut req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(req.as_reader()).unwrap();
            assert_eq!(req.url(), "/leave");
            assert_eq!(body["callTransactionEnd"]["code"], 487);
            req.respond(Response::empty(204)).unwrap();
        });
        let mut client = Client::spawn(
            hub,
            CallKey {
                generation: 1,
                id: native_id(),
            },
            link(),
            Box::pin(async move {
                Ok(PreparedCall {
                    resolved_invitation: None,
                    offer_profile: OfferProfile::Dtls,
                    route,
                    transport,
                    identity: person,
                    addresses: vec!["127.0.0.1".parse().unwrap()],
                    stun_service: None,
                    relay_request: None,
                })
            }),
            Box::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Fault::Audio)
            }),
        )
        .unwrap();
        arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        client.cancel();
        release_tx.send(()).unwrap();
        let state = until(&mut client, |s| s.phase.terminal()).await;
        assert_eq!(state.phase, Phase::Ended);
        assert_eq!(used.load(Ordering::SeqCst), 0);
        worker.join().unwrap();
    }
}
