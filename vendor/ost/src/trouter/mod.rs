//! Trouter v4 WebSocket push notification client
//!
//! Connects to Microsoft Teams' Trouter service to receive real-time
//! push notifications (messages, presence, calls, etc.).

pub mod registrar;
pub mod session;
pub mod websocket;

use anyhow::{Context, Result};
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::time;

use crate::calling;
use crate::config::Config;

/// Reason the inner connection loop exited.
enum DisconnectReason {
    /// Clean shutdown (Ctrl+C). Do not reconnect.
    Shutdown,
    /// Error or server-initiated close. Should reconnect.
    Error(anyhow::Error),
}

/// Run the Trouter connection with automatic reconnection.
///
/// On transient errors or server-initiated disconnects, reconnects with
/// exponential backoff (1s, 2s, 4s, ... capped at 64s). On clean shutdown
/// (Ctrl+C), exits immediately.
pub async fn connect_and_run() -> Result<()> {
    let mut backoff = 1u64;

    loop {
        match connect_and_run_inner().await {
            Ok(DisconnectReason::Shutdown) => {
                return Ok(());
            }
            Ok(DisconnectReason::Error(e)) => {
                // Connection was stable (>60s), reset backoff before reconnecting.
                backoff = 1;
                tracing::warn!(
                    "Trouter disconnected after stable session: {:#}. Reconnecting in 1s...",
                    e,
                );

                tokio::select! {
                    _ = time::sleep(Duration::from_secs(1)) => {}
                    _ = tokio::signal::ctrl_c() => {
                        println!("Shutting down...");
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Trouter disconnected: {:#}. Reconnecting in {}s...",
                    e,
                    backoff
                );

                tokio::select! {
                    _ = time::sleep(Duration::from_secs(backoff)) => {}
                    _ = tokio::signal::ctrl_c() => {
                        println!("Shutting down...");
                        return Ok(());
                    }
                }

                backoff = (backoff * 2).min(64);
            }
        }
    }
}

/// Run one full Trouter session: negotiate, connect, event loop.
///
/// Returns `DisconnectReason::Shutdown` on clean Ctrl+C, or
/// `DisconnectReason::Error` when the connection should be retried.
async fn connect_and_run_inner() -> Result<DisconnectReason> {
    // Reload config each attempt so we pick up refreshed tokens.
    let config = Config::load_cached().context("Failed to load config")?;

    let skype_token = config
        .get_skype_token()
        .context("No skype token found. Run `teams-cli login` first.")?;
    anyhow::ensure!(
        !skype_token.is_expired(),
        "Skype token expired. Run `teams-cli login` to refresh."
    );

    let skype_token_str = &skype_token.token;
    let http = crate::api::client::shared_http();

    // 1. Negotiate session (returns session info + epid)
    let (session, epid) = session::negotiate(&http, skype_token_str).await?;

    // 2. Get socket.io session ID (authenticated via X-Skypetoken header)
    let session_id = session::get_session_id(&http, &session, skype_token_str, &epid).await?;

    // 3. Connect WebSocket (auth is via session ID in URL, no headers needed)
    let mut ws = websocket::TrouterSocket::connect(&session, &session_id, &epid).await?;

    // 4. Wait for handshake frame (1::)
    let frame = ws
        .recv_frame()
        .await?
        .context("Connection closed before handshake")?;

    if !frame.starts_with("1::") {
        tracing::warn!("Expected 1:: handshake, got: {}", frame);
    } else {
        tracing::info!("Received handshake frame");
    }

    // 5. Register with registrar
    let registrar_ttl_secs: u64 = 86400;
    if let Some(ref reg_url) = session.registrar_url {
        if let Err(e) = registrar::register_with_endpoint(
            &http, skype_token_str, reg_url, &session.surl, Some(&epid),
        )
        .await
        {
            tracing::warn!("Initial registrar registration failed: {:#}", e);
        }
    }
    // OstMac §106: mark the endpoint active (Teams clients send this
    // right after the handshake; the server acks with `6:1+::`).
    let activity = serde_json::json!({
        "name": "user.activity",
        "args": [{"state": "active", "cv": format!("{}.0.1", uuid::Uuid::new_v4().simple())}],
    });
    if let Err(e) = ws.send_text(&format!("5:1+::{}", activity)).await {
        tracing::warn!("user.activity send failed: {:#}", e);
    }

    // 6. Event loop: recv frames, send heartbeat, re-register before TTL,
    //    force reconnect after session max age.
    let connected_at = Instant::now();
    let mut heartbeat = time::interval(Duration::from_secs(30));
    heartbeat.tick().await; // skip first immediate tick

    // Re-register 30s before TTL expires.
    let re_register_interval = Duration::from_secs(registrar_ttl_secs.saturating_sub(30));
    let mut re_register_deadline = Box::pin(time::sleep(re_register_interval));

    // Force full reconnect after 1 hour to refresh the session.
    // The session TTL is typically ~589000s but rotating more frequently
    // keeps tokens and registrations fresh.
    let session_max_age = Duration::from_secs(3600);
    let mut session_deadline = Box::pin(time::sleep(session_max_age));

    // Stability threshold: reset backoff after 60s of successful connection.
    // We communicate this via the return value — the caller checks timing.
    let stability_threshold = Duration::from_secs(60);

    println!("Trouter connected. Listening for events... (Ctrl-C to stop)");

    let mut cdl_reregistered = false;
    let disconnect_reason = loop {
        tokio::select! {
            frame = ws.recv_frame() => {
                match frame {
                    Ok(Some(text)) => {
                        // OstMac §106: first message_loss after connect →
                        // re-register the chat (CDL) entry with the 1.9
                        // template, as the Teams clients do.
                        if !cdl_reregistered && text.contains("trouter.message_loss") {
                            cdl_reregistered = true;
                            if let Some(ref reg_url) = session.registrar_url {
                                let (h, tok, reg, surl, ep) = (http.clone(), skype_token_str.to_string(),
                                    reg_url.clone(), session.surl.clone(), epid.clone());
                                tokio::spawn(async move {
                                    if let Err(e) = registrar::register_cdl(&h, &tok, &reg, &surl, &ep,
                                        "TeamsCDLWebWorker_1.9").await {
                                        tracing::warn!("CDL re-registration failed: {:#}", e);
                                    }
                                });
                            }
                        }
                        handle_frame(&text, &http, skype_token_str, None).await
                    }
                    Ok(None) => {
                        break DisconnectReason::Error(anyhow::anyhow!("WebSocket closed by server"));
                    }
                    Err(e) => {
                        break DisconnectReason::Error(e.context("WebSocket recv error"));
                    }
                }
            }
            _ = heartbeat.tick() => {
                if let Err(e) = ws.send_text("2::").await {
                    break DisconnectReason::Error(e.context("Heartbeat send failed"));
                }
            }
            _ = &mut re_register_deadline => {
                tracing::info!("Re-registering with registrar (TTL refresh)");
                if let Some(ref reg_url) = session.registrar_url {
                    let http2 = http.clone();
                    let tok = skype_token_str.to_string();
                    let surl = session.surl.clone();
                    let reg = reg_url.clone();
                    let ep = epid.clone();
                    tokio::spawn(async move {
                        if let Err(e) = registrar::register_with_endpoint(&http2, &tok, &reg, &surl, Some(&ep)).await {
                            tracing::warn!("Re-registration failed: {:#}", e);
                        }
                    });
                }
                // Reset the timer for another cycle.
                re_register_deadline = Box::pin(time::sleep(re_register_interval));
            }
            _ = &mut session_deadline => {
                tracing::info!("Session max age reached (1h), forcing reconnect for fresh session");
                break DisconnectReason::Error(anyhow::anyhow!("Session max age reached"));
            }
            _ = tokio::signal::ctrl_c() => {
                println!("Shutting down...");
                break DisconnectReason::Shutdown;
            }
        }
    };

    // If we were connected long enough, signal stability so caller resets backoff.
    // We do this by returning Ok (the caller pattern-matches on it).
    if connected_at.elapsed() >= stability_threshold {
        // Reset backoff indirectly: caller sees Ok and resets.
        // But we still need to convey the reason.
        // Use Ok for both shutdown and stable-error cases.
        return Ok(disconnect_reason);
    }

    match disconnect_reason {
        DisconnectReason::Shutdown => Ok(DisconnectReason::Shutdown),
        DisconnectReason::Error(e) => Err(e),
    }
}

/// Run one cancellable Trouter session embedded in an application session.
///
/// The caller owns token refresh and reconnection so it can use its own
/// authenticated session. Events are sent to a session-scoped bounded
/// channel; this path never writes to the process-global event hub and never
/// auto-answers incoming calls.
pub type CallEventSink = std::sync::Arc<dyn Fn(crate::event_hub::TrouterEvent) + Send + Sync>;

#[derive(Clone, Copy, Debug)]
enum ConnectStage {
    Negotiation,
    Session,
    Socket,
    Handshake,
    Registration,
    Metadata,
}
impl std::fmt::Display for ConnectStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for ConnectStage {}
impl ConnectStage {
    fn code(self) -> &'static str {
        match self {
            Self::Negotiation => "negotiation",
            Self::Session => "session",
            Self::Socket => "socket",
            Self::Handshake => "handshake",
            Self::Registration => "registration",
            Self::Metadata => "metadata_expired",
        }
    }
}
/// Only a closed set of diagnostic codes crosses into the host UI. Never pass
/// raw HTTP/WebSocket errors, URLs, response bodies or credentials to it.
pub fn connection_failure_code(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<ConnectStage>()
        .map_or("connection", |stage| stage.code())
}

async fn register_embedded(
    http: &reqwest::Client,
    token: &str,
    registrar: Option<&str>,
    root: &str,
    endpoint: &str,
) -> Result<registrar::RegistrationSet> {
    let url = registrar.context(ConnectStage::Registration)?;
    registrar::register_with_endpoint_tracked(http, token, url, root, Some(endpoint))
        .await
        .context(ConnectStage::Registration)
}

struct CallSinkLifetime { sink: Option<CallEventSink>, connected: bool }
impl CallSinkLifetime {
    fn disconnect(&mut self) {
        if self.connected {
            self.connected = false;
            if let Some(sink) = &self.sink {
                sink(crate::event_hub::TrouterEvent { kind: crate::event_hub::TrouterEventKind::TransportDisconnected, payload_json: "{}".into() });
            }
        }
    }
}
impl Drop for CallSinkLifetime { fn drop(&mut self) { self.disconnect(); } }

pub async fn connect_with_token(
    skype_token: &str,
    events: mpsc::Sender<crate::event_hub::TrouterEvent>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    connect_with_call_sink(skype_token, events, shutdown, None).await
}

pub async fn connect_with_call_sink(
    skype_token: &str,
    events: mpsc::Sender<crate::event_hub::TrouterEvent>,
    mut shutdown: watch::Receiver<bool>,
    call_sink: Option<CallEventSink>,
) -> Result<()> {
    let mut lifetime = CallSinkLifetime { sink: call_sink, connected: false };
    let http = crate::api::client::shared_http();

    let Some(negotiated) = cancellable(
        &mut shutdown,
        time::timeout(
            Duration::from_secs(30),
            session::negotiate(&http, skype_token),
        ),
    )
    .await
    else {
        return Ok(());
    };
    let (session, epid) = negotiated.context("Trouter negotiation timed out")
        .and_then(|result| result).context(ConnectStage::Negotiation)?;
    let negotiated_at = Instant::now();

    let Some(session_id) = cancellable(
        &mut shutdown,
        time::timeout(
            Duration::from_secs(30),
            session::get_session_id(&http, &session, skype_token, &epid),
        ),
    )
    .await
    else {
        return Ok(());
    };
    let session_id = session_id.context("Socket.IO session request timed out")
        .and_then(|result| result).context(ConnectStage::Session)?;

    let Some(socket) = cancellable(
        &mut shutdown,
        time::timeout(
            Duration::from_secs(15),
            websocket::TrouterSocket::connect(&session, &session_id, &epid),
        ),
    )
    .await
    else {
        return Ok(());
    };
    let mut ws = socket.context("Trouter WebSocket connection timed out")
        .and_then(|result| result).context(ConnectStage::Socket)?;

    let Some(handshake) = cancellable(
        &mut shutdown,
        time::timeout(Duration::from_secs(20), ws.recv_frame()),
    )
    .await
    else {
        return Ok(());
    };
    let frame = handshake.context("Trouter handshake timed out")
        .and_then(|result| result)
        .and_then(|frame| frame.context("Connection closed before handshake"))
        .context(ConnectStage::Handshake)?;
    if !frame.starts_with("1::") {
        return Err(anyhow::anyhow!("Expected Socket.IO handshake").context(ConnectStage::Handshake));
    }

    // Failure must escape to the host's reconnect loop. Keeping an unregistered
    // socket alive used to leave call readiness absent for the full session.
    let registrations = Some(register_embedded(
        &http, skype_token, session.registrar_url.as_deref(), &session.surl, &epid,
    ).await?);
    let remaining = remaining_transport_ttl(session.ttl, negotiated_at.elapsed()).min(3600);
    if remaining == 0 {
        if let Some(registrations) = &registrations {
            let _ = registrar::unregister(&http, skype_token, registrations).await;
        }
        return Err(anyhow::anyhow!("Trouter metadata expired during setup").context(ConnectStage::Metadata));
    }
    if registrations.is_some() {
        // Reuse this authenticated channel for future call callbacks. Do not
        // start another Trouter connection for each call or expose connectparams.
        let event = connected_transport_event(&session.surl, &epid, remaining);
        lifetime.connected = true;
        if let Some(sink) = &lifetime.sink { sink(event.clone()); }
        let _ = events.send(event).await;
    }
    if *shutdown.borrow() {
        if let Some(registrations) = &registrations {
            let _ = registrar::unregister(&http, skype_token, registrations).await;
        }
        return Ok(());
    }

    let activity = serde_json::json!({
        "name": "user.activity",
        "args": [{"state": "active", "cv": format!("{}.0.1", uuid::Uuid::new_v4().simple())}],
    });
    match time::timeout(
        Duration::from_secs(5),
        ws.send_text(&format!("5:1+::{}", activity)),
    )
    .await
    {
        Ok(Ok(())) => {}
        _ => tracing::warn!("user.activity send failed or timed out"),
    }

    const REGISTRAR_TTL_SECS: u64 = 86400;
    let mut heartbeat = time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let re_register_interval = Duration::from_secs(REGISTRAR_TTL_SECS.saturating_sub(30));
    let mut re_register_deadline = Box::pin(time::sleep(re_register_interval));
    let mut session_deadline = Box::pin(time::sleep(Duration::from_secs(remaining)));
    let mut cdl_reregistered = false;

    let result = async {
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                frame = ws.recv_frame() => {
                    match frame {
                        Ok(Some(text)) => {
                            if !cdl_reregistered && has_message_loss_marker(&text) {
                                cdl_reregistered = true;
                                publish_transport_status(
                                    &events,
                                    crate::event_hub::TrouterEventKind::MessageLoss,
                                )
                                .await;
                                if let Some(registrar_url) = session.registrar_url.as_deref() {
                                    if let Err(error) = registrar::register_cdl(
                                        &http, skype_token, registrar_url, &session.surl, &epid,
                                        "TeamsCDLWebWorker_1.9",
                                    ).await {
                                        tracing::warn!("CDL re-registration failed: {error:#}");
                                    }
                                }
                            }
                            handle_frame_with_call_sink(&text, &http, skype_token, Some(&events), lifetime.sink.as_ref()).await;
                        }
                        Ok(None) => anyhow::bail!("Trouter WebSocket closed by server"),
                        Err(error) => return Err(error.context("Trouter WebSocket receive failed")),
                    }
                }
                _ = heartbeat.tick() => {
                    time::timeout(Duration::from_secs(10), ws.send_text("2::"))
                        .await
                        .context("Trouter heartbeat timed out")??;
                }
                _ = &mut re_register_deadline => {
                    if let Some(registrations) = &registrations {
                        if let Err(error) = registrar::refresh(
                            &http, skype_token, &session.surl, registrations
                        ).await {
                            tracing::warn!("Trouter re-registration failed: {error:#}");
                        }
                    }
                    re_register_deadline = Box::pin(time::sleep(re_register_interval));
                }
                _ = &mut session_deadline => anyhow::bail!("Trouter session reached its renewal age"),
            }
        }
    }
    .await;

    lifetime.disconnect();
    publish_transport_status(
        &events,
        crate::event_hub::TrouterEventKind::TransportDisconnected,
    )
    .await;

    if let Some(registrations) = &registrations {
        if registrar::unregister(&http, skype_token, registrations)
            .await
            .is_err()
        {
            tracing::warn!("Trouter endpoint cleanup failed");
        }
    }
    result
}

async fn publish_transport_status(
    events: &mpsc::Sender<crate::event_hub::TrouterEvent>,
    kind: crate::event_hub::TrouterEventKind,
) {
    let _ = events
        .send(crate::event_hub::TrouterEvent {
            kind,
            payload_json: String::new(),
        })
        .await;
}

fn connected_transport_event(
    root: &str,
    endpoint: &str,
    ttl: u64,
) -> crate::event_hub::TrouterEvent {
    crate::event_hub::TrouterEvent {
        kind: crate::event_hub::TrouterEventKind::TransportConnected,
        payload_json: serde_json::json!({"messagingRoot":root,"transportEndpointId":endpoint,"ttlSeconds":ttl}).to_string(),
    }
}

fn remaining_transport_ttl(ttl: u64, elapsed: Duration) -> u64 {
    // Round the remaining lifetime down, rather than extending it by the
    // fractional time already spent registering the connection.
    Duration::from_secs(ttl).saturating_sub(elapsed).as_secs()
}

#[cfg(test)]
mod connected_call_context_tests {
    #[test]
    fn internal_connected_event_contains_only_callback_metadata_and_debug_redacts_payload() {
        assert_eq!(
            super::remaining_transport_ttl(1, std::time::Duration::from_millis(750)),
            0
        );
        assert_eq!(
            super::remaining_transport_ttl(60, std::time::Duration::from_secs(10)),
            50
        );
        assert_eq!(
            super::remaining_transport_ttl(1, std::time::Duration::from_secs(5)),
            0
        );
        let event = super::connected_transport_event(
            "https://fixture.trouter.skype.com/private-root/",
            "fixture-endpoint",
            60,
        );
        assert_eq!(
            event.kind,
            crate::event_hub::TrouterEventKind::TransportConnected
        );
        let value: serde_json::Value = serde_json::from_str(&event.payload_json).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 3);
        assert_eq!(value["ttlSeconds"], 60);
        assert_eq!(value["transportEndpointId"], "fixture-endpoint");
        assert!(value.get("connectparams").is_none());
        assert!(!format!("{event:?}").contains("private-root"));
        assert!(!format!("{event:?}").contains("fixture-endpoint"));
    }
}

fn has_message_loss_marker(frame: &str) -> bool {
    frame.contains("trouter.message_loss")
}

async fn cancellable<F: Future>(
    shutdown: &mut watch::Receiver<bool>,
    future: F,
) -> Option<F::Output> {
    if *shutdown.borrow() {
        return None;
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => None,
        result = future => Some(result),
    }
}

/// Handle an incoming socket.io frame.
async fn handle_frame(
    frame: &str, http: &reqwest::Client, skype_token: &str,
    events: Option<&mpsc::Sender<crate::event_hub::TrouterEvent>>,
) { handle_frame_with_call_sink(frame, http, skype_token, events, None).await; }

async fn handle_frame_with_call_sink(
    frame: &str,
    http: &reqwest::Client,
    skype_token: &str,
    events: Option<&mpsc::Sender<crate::event_hub::TrouterEvent>>,
    call_sink: Option<&CallEventSink>,
) {
    // socket.io framing:
    // 1:: — handshake (handled above)
    // 2:: — heartbeat ping (server)
    // 3::: — ephemeral message
    // 5:X::{json} — event
    // 6:X+::{json} — ack event

    if frame.starts_with("2::") {
        tracing::debug!("Heartbeat ping from server");
        return;
    }

    if frame.starts_with("5:") {
        // Socket.IO v1 event frame: 5:ACK_ID:ENDPOINT:JSON
        // Ack (6:ID::) is sent automatically by recv_frame() in websocket.rs.
        // Here we just extract the JSON payload after the `::` separator.
        if let Some(json_str) = socketio_event_payload(frame) {
            if let Some(event) = classify_event_json(&json_str) {
                let is_call = event.kind == crate::event_hub::TrouterEventKind::CallInvitation;
                publish_call_or_ui(event, events, call_sink);
                if is_call
                    && events.is_none()
                    && auto_answer_enabled(std::env::var_os("TEAMS_AUTO_ANSWER"))
                {
                    handle_call_event(&json_str, http, skype_token).await;
                }
            }
        }
        return;
    }

    if frame.starts_with("6:") {
        // Acks are transport bookkeeping; never print their body.
        return;
    }

    // OstMac §106: chat-service pushes arrive as HTTP-over-WS data frames
    // (`3:::`); embedders get the decoded event (was: printed, dropped).
    if let Some(payload) = frame.strip_prefix("3:::") {
        if let Some(event) = data_frame_routed_event(payload) {
            publish_call_or_ui(event, events, call_sink);
        }
    }
}

fn publish_call_or_ui(event: crate::event_hub::TrouterEvent, events: Option<&mpsc::Sender<crate::event_hub::TrouterEvent>>, sink: Option<&CallEventSink>) {
    if event.kind == crate::event_hub::TrouterEventKind::CallSignal {
        if let Some(sink) = sink { sink(event); return; }
    }
    publish_event(event, events);
}

/// Preserve only the HTTP callback envelope, never search inside chat bodies.
fn call_delivery_event(text: &str) -> Option<crate::event_hub::TrouterEvent> {
    let decoded = decode_event_json(text)?;
    let value: serde_json::Value = serde_json::from_str(&decoded).ok()?;
    let value = value.get("args").and_then(|v| v.get(0)).unwrap_or(&value);
    let value = value.get("data").unwrap_or(value);
    let object = value.as_object()?;
    let get = |name: &str| object.iter().find(|(key,_)|key.eq_ignore_ascii_case(name)).map(|(_,v)|v);
    // The original Trouter client derives `shortUrl` by removing its
    // `baseEndpointUrl`, then rebuilds `url` as `baseEndpointUrl + path`.
    // Keep both representations so the session-scoped CallHub can normalize
    // them against its own opaque endpoint and reject disagreement.
    let address = get("url").and_then(serde_json::Value::as_str);
    let short_address = get("shortUrl").and_then(serde_json::Value::as_str);
    if address.is_none() && short_address.is_none() { return None; }
    let is_call_route = |address: &str| {
        let route = address.to_ascii_lowercase();
        address.len() <= 16384
            && (route.contains("/callagent/")
                || route.starts_with("callagent/")
                || route.contains("ngcallmanager"))
            && !address.chars().any(char::is_control)
    };
    if !address.is_some_and(is_call_route) && !short_address.is_some_and(is_call_route) { return None; }
    let raw = get("body")?;
    let body = if let Some(text) = raw.as_str() {
        let decoded = decode_event_json(text).or_else(||gunzip_base64(text).and_then(|v|decode_event_json(&v)))?;
        serde_json::from_str::<serde_json::Value>(&decoded).ok()?
    } else { raw.clone() };
    if !body.is_object() {return None;}
    let body = normalize_call_body(body);
    let mut envelope = serde_json::Map::new();
    if let Some(address) = address { envelope.insert("url".into(), serde_json::Value::String(address.to_owned())); }
    if let Some(address) = short_address { envelope.insert("shortUrl".into(), serde_json::Value::String(address.to_owned())); }
    let callback_url = [address, short_address].into_iter().flatten().any(is_callback_address);
    let kind = if incoming_call_invitation(&body)
        || (!callback_url && (contains_call_invitation(&body) || contains_call_notification(&body)))
    {
        crate::event_hub::TrouterEventKind::CallInvitation
    } else {
        crate::event_hub::TrouterEventKind::CallSignal
    };
    envelope.insert("body".into(), body);
    let payload_json = serde_json::Value::Object(envelope).to_string();
    if payload_json.len()>1024*1024 {return None;}
    Some(crate::event_hub::TrouterEvent {kind,payload_json})
}

/// A call-agent push whose body is the invitation itself, not a callback for a
/// call this client already joined. Callbacks keep names such as
/// `callAcceptance` and stay on the call session path.
fn is_callback_address(address: &str) -> bool {
    let lower = address.to_ascii_lowercase();
    ["/acceptance", "/progress", "/roster", "/call/end", "callend", "renegotiation", "addparticipant"]
        .iter()
        .any(|part| lower.contains(part))
}

fn invitation_object(value: &serde_json::Value) -> bool {
    if value.is_object() {
        return true;
    }
    value
        .as_str()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|parsed| parsed.is_object())
}

/// Turn a stringified invitation, or a `callNotification` wrapper, into the
/// object the call UI already knows how to read.
fn normalize_call_body(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(encoded) = body.as_object().and_then(|object| object.get("cp")).and_then(|value| value.as_str()) {
        if let Some(text) = gunzip_base64(encoded) {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(&text) {
                return normalize_call_body(decoded);
            }
        }
    }
    let Some(object) = body.as_object() else { return body };
    if let Some(note) = object.get("callNotification").cloned() {
        let parsed = if let Some(text) = note.as_str() {
            serde_json::from_str::<serde_json::Value>(text).ok()
        } else if note.is_object() {
            Some(note)
        } else {
            None
        };
        if let Some(parsed) = parsed {
            if contains_call_invitation(&parsed) {
                return parsed;
            }
        }
    }
    let Some(object) = body.as_object_mut() else { return body };
    if let Some(text) = object.get("callInvitation").and_then(|value| value.as_str()).map(str::to_owned) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
            if parsed.is_object() {
                object.insert("callInvitation".into(), parsed);
            }
        }
    }
    body
}

fn incoming_call_invitation(body: &serde_json::Value) -> bool {
    let Some(object) = body.as_object() else { return false };
    let invited = object.iter().any(|(key, value)| {
        (key.eq_ignore_ascii_case("callInvitation") && invitation_object(value))
            || (key.eq_ignore_ascii_case("callNotification") && contains_call_notification(value))
    });
    if !invited { return false; }
    const CALLBACKS: &[&str] = &[
        "callAcceptance", "conversationUpdate", "rosterUpdate", "callEnd",
        "mediaRenegotiation", "progress", "addParticipantSuccess", "addParticipantFailure",
    ];
    !object.keys().any(|key| CALLBACKS.iter().any(|known| key.eq_ignore_ascii_case(known)))
}

fn publish_event(
    event: crate::event_hub::TrouterEvent,
    events: Option<&mpsc::Sender<crate::event_hub::TrouterEvent>>,
) {
    if let Some(events) = events {
        let kind = event.kind;
        if let Err(error) = events.try_send(event) {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "full",
                mpsc::error::TrySendError::Closed(_) => "closed",
            };
            tracing::warn!("Dropping Trouter event (kind={kind:?}, app channel {reason})");
        }
    } else {
        crate::event_hub::publish_routed(event);
    }
}

fn auto_answer_enabled(value: Option<std::ffi::OsString>) -> bool {
    value.as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// Extract JSON from a Socket.IO v1 event frame (`5:ACK:ENDPOINT:JSON`).
/// The body is deliberately returned without logging or endpoint assumptions.
fn socketio_event_payload(frame: &str) -> Option<String> {
    let rest = frame.strip_prefix("5:")?;
    let separator = rest.find("::")?;
    let payload = rest[separator + 2..].trim();
    serde_json::from_str::<serde_json::Value>(payload).ok()?;
    Some(payload.to_owned())
}

/// Classify only payloads with explicit call, presence, or chat markers.
/// Unknown and malformed JSON is intentionally ignored by the event hub.
fn classify_event_json(text: &str) -> Option<crate::event_hub::TrouterEvent> {
    if let Some(event) = call_delivery_event(text) {return Some(event);}
    use crate::event_hub::{TrouterEvent, TrouterEventKind};

    let normalized = decode_event_json(text)?;
    let value: serde_json::Value = serde_json::from_str(&normalized).ok()?;
    let kind = if contains_call_invitation(&value) || contains_call_notification(&value) {
        TrouterEventKind::CallInvitation
    } else if contains_presence_marker(&value) {
        TrouterEventKind::Presence
    } else if contains_chat_marker(&value) {
        TrouterEventKind::ChatService
    } else {
        return None;
    };

    Some(TrouterEvent {
        kind,
        payload_json: normalized,
    })
}

/// The live ring is `callNotification.from`, often inside a gzip `cp` wrapper.
/// It does not carry `callInvitation`.
fn contains_call_notification(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
            (key.eq_ignore_ascii_case("callNotification") && notification_has_caller(child))
                || (key.eq_ignore_ascii_case("cp") && child.as_str().and_then(gunzip_base64).is_some_and(|text| {
                    serde_json::from_str::<serde_json::Value>(&text).ok().is_some_and(|nested| contains_call_notification(&nested))
                }))
                || contains_call_notification(child)
        }),
        serde_json::Value::Array(items) => items.iter().any(contains_call_notification),
        serde_json::Value::String(text) => serde_json::from_str::<serde_json::Value>(text)
            .map(|nested| contains_call_notification(&nested))
            .unwrap_or(false),
        _ => false,
    }
}

fn notification_has_caller(value: &serde_json::Value) -> bool {
    let parsed = if let Some(text) = value.as_str() {
        serde_json::from_str::<serde_json::Value>(text).ok()
    } else if value.is_object() {
        Some(value.clone())
    } else {
        None
    };
    parsed.is_some_and(|note| note.get("from").is_some_and(serde_json::Value::is_object))
}

fn contains_call_invitation(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
            (key.eq_ignore_ascii_case("callInvitation") && invitation_object(child))
                || contains_call_invitation(child)
        }),
        serde_json::Value::Array(items) => items.iter().any(contains_call_invitation),
        serde_json::Value::String(text) => serde_json::from_str::<serde_json::Value>(text)
            .map(|nested| contains_call_invitation(&nested))
            .unwrap_or(false),
        _ => false,
    }
}

fn contains_presence_marker(value: &serde_json::Value) -> bool {
    const PRESENCE_KEYS: &[&str] = &[
        "presence",
        "presencechange",
        "presencechanged",
        "presenceupdate",
        "availability",
        "presencestatus",
    ];
    const PRESENCE_TYPES: &[&str] = &[
        "presence",
        "presencechange",
        "presencechanged",
        "presenceupdate",
    ];

    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
            PRESENCE_KEYS
                .iter()
                .any(|known| key.eq_ignore_ascii_case(known))
                || ((key.eq_ignore_ascii_case("resourceType")
                    || key.eq_ignore_ascii_case("type")
                    || key.eq_ignore_ascii_case("eventType"))
                    && child.as_str().map_or(false, |name| {
                        PRESENCE_TYPES
                            .iter()
                            .any(|known| name.eq_ignore_ascii_case(known))
                    }))
                || contains_presence_marker(child)
        }),
        serde_json::Value::Array(items) => items.iter().any(contains_presence_marker),
        serde_json::Value::String(text) => serde_json::from_str::<serde_json::Value>(text)
            .map(|nested| contains_presence_marker(&nested))
            .unwrap_or(false),
        _ => false,
    }
}

fn contains_chat_marker(value: &serde_json::Value) -> bool {
    const CHAT_RESOURCE_TYPES: &[&str] = &[
        "NewMessage",
        "MessageUpdate",
        "PartialMessageUpdate",
        "ThreadUpdate",
        "ConversationUpdate",
        "ConversationFullUpdate",
        "CommentStreamLie",
        "NewLiveMessages",
        "LiveMessageUpdates",
    ];

    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
            ((key.eq_ignore_ascii_case("resourceType")
                || key.eq_ignore_ascii_case("__typename")
                || key.eq_ignore_ascii_case("eventType"))
                && child.as_str().map_or(false, |name| {
                    CHAT_RESOURCE_TYPES
                        .iter()
                        .any(|known| name.eq_ignore_ascii_case(known))
                }))
                || (key.eq_ignore_ascii_case("eventMessages")
                    && contains_chat_marker(child))
                || contains_chat_marker(child)
        }),
        serde_json::Value::Array(items) => items.iter().any(contains_chat_marker),
        serde_json::Value::String(text) => serde_json::from_str::<serde_json::Value>(text)
            .map(|nested| contains_chat_marker(&nested))
            .unwrap_or(false),
        _ => false,
    }
}

fn decode_event_json(text: &str) -> Option<String> {decode_event_json_at(text,0)}

fn decode_event_json_at(text: &str, depth: usize) -> Option<String> {
    if depth>8 || text.len()>1024*1024 {return None;}
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if let Some(wrapper) = value.as_object() {
        for key in ["cp", "gp"] {
            if let Some(encoded) = wrapper.get(key).and_then(|value| value.as_str()) {
                let decoded = if key == "cp" {
                    gunzip_base64(encoded)?
                } else {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded.trim())
                        .ok()?;
                    String::from_utf8(bytes).ok()?
                };
                return decode_event_json_at(&decoded,depth+1);
            }
        }
    }
    Some(text.to_owned())
}

/// OstMac §106: decode one Trouter `3:::` data frame payload
/// (`{id, method, url, headers, body}`) into the pushed chat-service
/// event JSON. `body` is the event itself, or base64 + gzip
/// (`X-Microsoft-Skype-Content-Encoding: gzip`, also tried when the
/// plain body isn't JSON). Only chat-service events (`resourceType` /
/// `resource` / `eventMessages`) are returned; anything else (calls,
/// other services) is None and keeps its existing path.
pub fn data_frame_event(payload: &str) -> Option<String> {
    let event = data_frame_routed_event(payload)?;
    (event.kind == crate::event_hub::TrouterEventKind::ChatService)
        .then_some(event.payload_json)
}

/// Decode and classify a Trouter HTTP-over-WebSocket data frame.
/// Presence payload markers remain static-analysis-derived hints because the
/// original bundle selection does not establish a complete presence envelope.
pub fn data_frame_routed_event(payload: &str) -> Option<crate::event_hub::TrouterEvent> {
    if payload.len()>1024*1024 {return None;}
    if let Some(event) = call_delivery_event(payload) {return Some(event);}
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let obj = v.as_object()?;
    // `body` sits top-level or under `data` depending on the Trouter
    // version (same two shapes the call path reads); an object body is
    // the event itself.
    let find_body = |m: &serde_json::Map<String, serde_json::Value>| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("body"))
            .map(|(_, v)| v.clone())
    };
    let body_val = find_body(obj).or_else(|| {
        obj.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("data"))
            .and_then(|(_, d)| d.as_object())
            .and_then(|d| find_body(d))
    })?;
    let body = if body_val.is_object() || body_val.is_array() {
        body_val.to_string()
    } else {
        body_val.as_str()?.to_owned()
    };
    let gzip_header = obj
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("headers"))
        .and_then(|(_, h)| h.as_object())
        .map(|h| {
            h.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("X-Microsoft-Skype-Content-Encoding")
                    && v.as_str().map_or(false, |s| s.to_ascii_lowercase().contains("gzip"))
            })
        })
        .unwrap_or(false);
    let plain = if gzip_header {
        None
    } else {
        classify_event_json(&body)
    };
    plain.or_else(|| gunzip_base64(&body).and_then(|decoded| classify_event_json(&decoded)))
}

fn gunzip_base64(body: &str) -> Option<String> {
    use base64::Engine;
    use std::io::Read;
    if body.len()>2*1024*1024 {return None;}
    let raw = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .ok()?;
    let mut out = String::new();
    flate2::read::GzDecoder::new(&raw[..])
        .take(1024*1024+1)
        .read_to_string(&mut out)
        .ok()?;
    (out.len()<=1024*1024).then_some(out)
}

/// Legacy experimental auto-answer path, invoked only when
/// `TEAMS_AUTO_ANSWER=1` is explicitly present in the process environment.
async fn handle_call_event(json_str: &str, http: &reqwest::Client, skype_token: &str) {
    let notification = match calling::parse_call_notification(json_str) {
        Some(n) => n,
        None => {
            tracing::debug!("Could not parse call notification from JSON");
            return;
        }
    };

    // Log caller identity.
    if let Some(ref participants) = notification.participants {
        if let Some(ref from) = participants.from {
            println!(
                "  Incoming call from: {} ({})",
                from.display_name.as_deref().unwrap_or("unknown"),
                from.id.as_deref().unwrap_or("?")
            );
        }
    }

    // Log call modalities.
    if let Some(ref inv) = notification.call_invitation {
        if let Some(ref mods) = inv.call_modalities {
            println!("  Modalities: {:?}", mods);
        }
    }

    // Log call ID from debug content.
    if let Some(ref debug) = notification.debug_content {
        if let Some(ref call_id) = debug.call_id {
            println!("  Call ID: {}", call_id);
        }
    }

    // Determine if video modality is requested.
    let has_video = notification
        .call_invitation
        .as_ref()
        .and_then(|inv| inv.call_modalities.as_ref())
        .map(|mods| mods.iter().any(|m| m.eq_ignore_ascii_case("video")))
        .unwrap_or(false);

    // Auto-answer: generate SDP answer and send acceptance.
    println!("  Auto-answering call...");

    // Try to acquire TURN relay credentials (best-effort, fail gracefully).
    let relay_config = match calling::turn::acquire_relay_credentials(http, skype_token).await {
        Ok(config) => {
            tracing::info!(
                "Acquired relay credentials: {} servers, username={}, ttl={}s",
                config.servers.len(),
                config.username,
                config.ttl
            );
            Some(config)
        }
        Err(e) => {
            tracing::info!(
                "Relay credential acquisition failed (will use direct/srflx only): {:#}",
                e
            );
            None
        }
    };

    // Gather relay candidate if we have credentials.
    let relay_candidate = if let Some(ref config) = relay_config {
        match calling::turn::gather_relay_candidate(config).await {
            Some((candidate, _client)) => {
                tracing::info!(
                    "Gathered relay candidate: {}:{}",
                    candidate.address,
                    candidate.port
                );
                Some(candidate)
            }
            None => {
                tracing::info!("Failed to gather relay candidate (TURN allocate failed)");
                None
            }
        }
    } else {
        None
    };

    // Try to generate and send media answer first (protocol order: media answer before acceptance).
    if let Some(ref inv) = notification.call_invitation {
        if let Some(ref mc) = inv.media_content {
            if let Some(ref blob) = mc.blob {
                match calling::sdp::parse_sdp_offer(blob) {
                    Ok(offer_info) => {
                        let local_ip = calling::sdp::get_local_ip();

                        // Parse remote SRTP keying material from the offer's crypto lines.
                        let remote_audio_crypto = offer_info
                            .crypto_lines
                            .iter()
                            .find_map(|line| calling::srtp::parse_crypto_line(line).ok());

                        let remote_video_crypto = offer_info.video.as_ref().and_then(|v| {
                            v.crypto_lines
                                .iter()
                                .find_map(|line| calling::srtp::parse_crypto_line(line).ok())
                        });

                        // Build local candidates list (relay if available).
                        let mut local_cands: Vec<calling::ice::IceCandidate> = Vec::new();
                        if let Some(ref rc) = relay_candidate {
                            local_cands.push(rc.clone());
                        }

                        // Generate SDP answer with ICE credentials.
                        let answer_result = calling::sdp::generate_sdp_answer_full(
                            &local_ip,
                            0,
                            0,
                            &offer_info,
                            &local_cands,
                            &[],
                        );

                        // Parse our own SRTP keying material from the answer.
                        let local_audio_crypto =
                            calling::srtp::parse_crypto_line(&answer_result.audio_crypto_line).ok();
                        let local_video_crypto = answer_result
                            .video_crypto_line
                            .as_ref()
                            .and_then(|line| calling::srtp::parse_crypto_line(line).ok());

                        tracing::info!(
                            "Generated SDP answer ({} bytes, video={}, ufrag={})",
                            answer_result.sdp.len(),
                            offer_info.video.is_some(),
                            answer_result.audio_ice_ufrag
                        );

                        if let Err(e) = calling::signaling::send_media_answer(
                            http,
                            skype_token,
                            &notification,
                            &answer_result.sdp,
                        )
                        .await
                        {
                            tracing::warn!("Failed to send media answer: {:#}", e);
                        }

                        // Start audio media session with ICE connectivity checks.
                        if let (Some(local_mat), Some(remote_mat)) =
                            (local_audio_crypto, remote_audio_crypto)
                        {
                            let candidates = calling::ice::parse_candidates_from_sdp(blob);
                            if candidates.iter().any(|c| {
                                c.transport == calling::ice::Transport::Udp && c.component == 1
                            }) {
                                let local_creds = calling::ice::IceCredentials {
                                    ufrag: answer_result.audio_ice_ufrag.clone(),
                                    pwd: answer_result.audio_ice_pwd.clone(),
                                };
                                let remote_creds = calling::ice::IceCredentials {
                                    ufrag: offer_info.ice_ufrag.clone(),
                                    pwd: offer_info.ice_pwd.clone(),
                                };
                                tracing::info!(
                                    "Starting audio media session with ICE ({} candidates)",
                                    candidates.len()
                                );
                                tokio::spawn(async move {
                                    match calling::media::MediaSession::start_with_ice(
                                        0,
                                        &candidates,
                                        &local_creds,
                                        &remote_creds,
                                        &local_mat,
                                        &remote_mat,
                                    )
                                    .await
                                    {
                                        Ok(session) => {
                                            tracing::info!(
                                                "Audio session started on port {}",
                                                session.local_port().unwrap_or(0)
                                            );
                                            loop {
                                                tokio::time::sleep(std::time::Duration::from_secs(
                                                    5,
                                                ))
                                                .await;
                                                let stats = session.stats().await;
                                                tracing::info!(
                                                    "Audio stats: sent={}, recv={} ({} bytes)",
                                                    stats.packets_sent,
                                                    stats.packets_received,
                                                    stats.bytes_received
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to start audio session: {:#}",
                                                e
                                            );
                                        }
                                    }
                                });
                            } else {
                                tracing::warn!("No suitable audio ICE candidate found");
                            }
                        } else {
                            tracing::warn!(
                                "Missing audio SRTP keying material, audio session not started"
                            );
                        }

                        // Start video media session if video modality is present.
                        if has_video {
                            if let (Some(local_mat), Some(remote_mat)) =
                                (local_video_crypto, remote_video_crypto)
                            {
                                let vid_candidates =
                                    calling::ice::parse_candidates_from_sdp_section(blob, "video");
                                if let Some(remote_addr) =
                                    calling::ice::select_remote_candidate(&vid_candidates)
                                {
                                    tracing::info!(
                                        "Starting video media session to remote {}",
                                        remote_addr
                                    );
                                    tokio::spawn(async move {
                                        match calling::media::VideoMediaSession::start(
                                            0,
                                            remote_addr,
                                            &local_mat,
                                            &remote_mat,
                                        )
                                        .await
                                        {
                                            Ok(session) => {
                                                tracing::info!(
                                                    "Video session started on port {}",
                                                    session.local_port().unwrap_or(0)
                                                );
                                                loop {
                                                    tokio::time::sleep(
                                                        std::time::Duration::from_secs(5),
                                                    )
                                                    .await;
                                                    let stats = session.stats().await;
                                                    tracing::info!(
                                                        "Video stats: sent={} pkts/{} frames, recv={} pkts/{} frames ({} bytes)",
                                                        stats.packets_sent,
                                                        stats.frames_sent,
                                                        stats.packets_received,
                                                        stats.frames_received,
                                                        stats.bytes_received
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Failed to start video session: {:#}",
                                                    e
                                                );
                                            }
                                        }
                                    });
                                } else {
                                    tracing::warn!("No suitable video ICE candidate found");
                                }
                            } else {
                                tracing::warn!(
                                    "Missing video SRTP keying material, video session not started"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Could not parse SDP offer (may be compressed): {:#}", e);
                        // Still try acceptance without media answer.
                    }
                }
            }
        }
    }

    // Send acceptance.
    if let Err(e) = calling::signaling::accept_call(http, skype_token, &notification).await {
        tracing::warn!("Failed to accept call: {:#}", e);
    }
}

#[cfg(test)]
mod sendfix_tests {
    use super::{
        auto_answer_enabled, cancellable, classify_event_json, data_frame_event,
        data_frame_routed_event, handle_frame, has_message_loss_marker, socketio_event_payload,
    };
    use crate::event_hub::TrouterEventKind;

    #[test]
    fn rt06_detects_the_server_message_loss_marker() {
        assert!(has_message_loss_marker(
            r#"5::: {"name":"trouter.message_loss","args":[]}"#
        ));
        assert!(!has_message_loss_marker(
            r#"5::: {"name":"user.activity","args":[]}"#
        ));
    }

    #[test]
    fn rt03_classifies_socketio_chat_presence_and_call_frames() {
        let chat = r#"5:17::{"resourceType":"NewMessage","resource":{"id":"m-1"}}"#;
        let conversation = r#"5:18::{"resourceType":"ConversationUpdate","resource":{"id":"chat-1"}}"#;
        let roster = r#"5:19::{"__typename":"ConversationFullUpdate","resource":{"conversation":{"id":"group-1"}}}"#;
        let presence = r#"5::: {"resourceType":"PresenceUpdate","availability":"Busy"}"#;
        let call = r#"5:22::{"callInvitation":{"callModalities":["audio"],"links":{}}}"#;

        let chat_json = socketio_event_payload(chat).unwrap();
        let conversation_json = socketio_event_payload(conversation).unwrap();
        let roster_json = socketio_event_payload(roster).unwrap();
        let presence_json = socketio_event_payload(presence).unwrap();
        let call_json = socketio_event_payload(call).unwrap();

        assert_eq!(
            classify_event_json(&chat_json).unwrap().kind,
            TrouterEventKind::ChatService
        );
        assert_eq!(
            classify_event_json(&conversation_json).unwrap().kind,
            TrouterEventKind::ChatService
        );
        assert_eq!(
            classify_event_json(&roster_json).unwrap().kind,
            TrouterEventKind::ChatService
        );
        assert_eq!(
            classify_event_json(&presence_json).unwrap().kind,
            TrouterEventKind::Presence
        );
        assert_eq!(
            classify_event_json(&call_json).unwrap().kind,
            TrouterEventKind::CallInvitation
        );
    }

    #[test]
    fn rt03_data_frames_route_presence_and_call_without_calling_them_chat() {
        let presence = r#"{"resourceType":"PresenceUpdate","availability":"Available"}"#;
        let call = r#"{"callInvitation":{"callModalities":["audio"],"links":{}}}"#;
        assert_eq!(
            data_frame_routed_event(&frame(presence, serde_json::json!({}))).unwrap().kind,
            TrouterEventKind::Presence
        );
        let routed_call = data_frame_routed_event(&frame(call, serde_json::json!({}))).unwrap();
        assert_eq!(routed_call.kind, TrouterEventKind::CallInvitation);
        assert!(data_frame_event(&frame(call, serde_json::json!({}))).is_none());
    }

    #[test]
    fn rt03_malformed_ack_and_unknown_frames_are_not_events() {
        assert!(socketio_event_payload("5:4::not-json").is_none());
        assert!(socketio_event_payload("6:4::{\"private\":\"payload\"}").is_none());
        assert!(classify_event_json(r#"{"resourceType":"UnknownThing","resource":{}}"#).is_none());
        assert!(classify_event_json("not-json").is_none());
        assert!(!auto_answer_enabled(None));
        assert!(!auto_answer_enabled(Some("true".into())));
        assert!(auto_answer_enabled(Some("1".into())));
    }

    #[test]
    fn embedded_delivery_routes_events_to_the_session_channel() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(4);
        let http = reqwest::Client::new();
        let chat = r#"3:::{"id":4,"method":"POST","url":"/v4/f/x/messaging","headers":{},"body":"{\"resourceType\":\"NewMessage\",\"resource\":{\"id\":\"m-1\"}}"}"#;
        let call = r#"5:8::{"callInvitation":{"callModalities":["audio"],"links":{}}}"#;

        runtime.block_on(async {
            handle_frame(chat, &http, "fixture-token", Some(&events_tx)).await;
            handle_frame(call, &http, "fixture-token", Some(&events_tx)).await;
        });

        assert_eq!(events_rx.try_recv().unwrap().kind, TrouterEventKind::ChatService);
        assert_eq!(events_rx.try_recv().unwrap().kind, TrouterEventKind::CallInvitation);
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn embedded_connection_cancellation_wins_before_network_work() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap();
        assert_eq!(runtime.block_on(cancellable(&mut shutdown_rx, async { 7 })), None);
    }

    fn frame(body: &str, headers: serde_json::Value) -> String {
        serde_json::json!({"id": 7, "method": "POST", "url": "/v4/f/x/messaging",
            "headers": headers, "body": body})
        .to_string()
    }

    #[test]
    fn sendfix_data_frame_plain_chat_event_is_published() {
        let ev = r#"{"resourceType":"NewMessage","resource":{"id":"1","clientmessageid":"9"}}"#;
        let got = data_frame_event(&frame(ev, serde_json::json!({}))).unwrap();
        assert_eq!(got, ev);
    }

    #[test]
    fn sendfix_data_frame_gzip_body_is_decoded() {
        use base64::Engine;
        use std::io::Write;
        let ev = r#"{"eventMessages":[{"resourceType":"NewMessage","resource":{"id":"2"}}]}"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(ev.as_bytes()).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        let hdr = serde_json::json!({"X-Microsoft-Skype-Content-Encoding": "gzip"});
        assert_eq!(data_frame_event(&frame(&b64, hdr)).as_deref(), Some(ev));
        // Header missing: the base64+gzip fallback still decodes.
        assert_eq!(data_frame_event(&frame(&b64, serde_json::json!({}))).as_deref(), Some(ev));
    }

    #[test]
    fn sendfix_data_frame_body_under_data_or_as_object() {
        let ev = r#"{"resourceType":"NewMessage","resource":{"id":"3"}}"#;
        let nested = serde_json::json!({"id": 9, "data": {"body": ev}}).to_string();
        assert_eq!(data_frame_event(&nested).as_deref(), Some(ev));
        let obj = serde_json::json!({"id": 9, "body": {"resourceType": "NewMessage"}}).to_string();
        assert!(data_frame_event(&obj).unwrap().contains("NewMessage"));
    }

    #[test]
    fn sendfix_data_frame_cp_and_gp_wrappers_are_unwrapped() {
        use base64::Engine;
        use std::io::Write;
        let ev = r#"{"type":"EventMessage","resourceType":"NewMessage","resource":{"id":"4"}}"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(ev.as_bytes()).unwrap();
        let cp = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        let body = serde_json::json!({"cp": cp}).to_string();
        assert_eq!(data_frame_event(&frame(&body, serde_json::json!({}))).as_deref(), Some(ev));
        let gp = base64::engine::general_purpose::STANDARD.encode(ev);
        let body = serde_json::json!({"gp": gp}).to_string();
        assert_eq!(data_frame_event(&frame(&body, serde_json::json!({}))).as_deref(), Some(ev));
    }

    #[test]
    fn sendfix_data_frame_non_chat_payloads_are_skipped() {
        assert!(data_frame_event(&frame(r#"{"callNotification":{}}"#, serde_json::json!({}))).is_none());
        assert!(data_frame_event("not json").is_none());
        assert!(data_frame_event(r#"{"id":1,"status":200}"#).is_none());
    }
}

#[cfg(test)]
mod native_call_sink_tests {
    use super::*;
    #[test]
    fn callback_envelope_is_preserved_without_searching_chat_content() {
        let body=serde_json::json!({"callAcceptance":{"mediaContent":{"blob":"fixture"}}});
        let envelope=serde_json::json!({"url":"/opaque/callAgent/session/nonce/call/acceptance/","body":body});
        for wire in [envelope.clone(),serde_json::json!({"data":envelope}),serde_json::json!({"name":"http","args":[envelope]})] {
            let routed=call_delivery_event(&wire.to_string()).unwrap();
            assert_eq!(routed.kind,crate::event_hub::TrouterEventKind::CallSignal);
            assert_eq!(serde_json::from_str::<serde_json::Value>(&routed.payload_json).unwrap()["body"],body);
        }
        assert!(call_delivery_event(&serde_json::json!({"resourceType":"NewMessage","resource":{"content":envelope}}).to_string()).is_none());
        let invitation = serde_json::json!({"url":"/opaque/callAgent/session/incoming","body":{"callInvitation":{"callModalities":["audio"]},"participants":{"from":{"displayName":"Ana"}}}});
        let routed = call_delivery_event(&invitation.to_string()).unwrap();
        assert_eq!(routed.kind, crate::event_hub::TrouterEventKind::CallInvitation);
        let mixed = serde_json::json!({"url":"/opaque/callAgent/session/nonce/call/acceptance/","body":{"callAcceptance":{},"callInvitation":{"callModalities":["audio"]}}});
        assert_eq!(call_delivery_event(&mixed.to_string()).unwrap().kind, crate::event_hub::TrouterEventKind::CallSignal);
        let ngc_body = serde_json::json!({
            "callInvitation": {"callModalities": ["audio"], "links": {}},
            "participants": {"from": {"displayName": "Ana"}, "to": {"id": "not-a-list"}},
        });
        let ngc = serde_json::json!({"url":"https://trouter.fixture/v4/session/NGCallManagerWin","body": ngc_body});
        let routed = call_delivery_event(&ngc.to_string()).unwrap();
        assert_eq!(routed.kind, crate::event_hub::TrouterEventKind::CallInvitation);
        let parsed = calling::parse_call_notification(&routed.payload_json).unwrap();
        assert_eq!(parsed.participants.unwrap().from.unwrap().display_name.as_deref(), Some("Ana"));
        let wrapped = serde_json::json!({
            "shortUrl": "NGCallManagerWin",
            "body": {"callNotification": serde_json::json!({"callInvitation":{"callModalities":"audio"},"participants":{"from":{"displayName":"Ana"}}}).to_string()},
        });
        let routed = call_delivery_event(&wrapped.to_string()).unwrap();
        assert_eq!(routed.kind, crate::event_hub::TrouterEventKind::CallInvitation);
        assert_eq!(calling::parse_call_notification(&routed.payload_json).unwrap().call_invitation.unwrap().call_modalities.unwrap(), vec!["audio".to_owned()]);
        let inner = serde_json::json!({
            "callNotification": {"from": {"id":"8:orgid:caller","displayName":"Ana"}, "to": {"id":"8:orgid:callee"}},
            "debugContent": {"callId":"call-77"}
        });
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        enc.write_all(inner.to_string().as_bytes()).unwrap();
        let cp = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        let wire = serde_json::json!({
            "url": "https://trouter.fixture/v4/session/NGCallManagerWin",
            "body": serde_json::json!({"evt":107,"cp":cp}).to_string(),
        });
        let routed = call_delivery_event(&wire.to_string()).unwrap();
        assert_eq!(routed.kind, crate::event_hub::TrouterEventKind::CallInvitation);
        let parsed = calling::parse_call_notification(&routed.payload_json).unwrap();
        assert_eq!(parsed.participants.as_ref().unwrap().from.as_ref().unwrap().display_name.as_deref(), Some("Ana"));
        assert_eq!(parsed.debug_content.unwrap().call_id.as_deref(), Some("call-77"));
        assert!(decode_event_json(&"x".repeat(1024*1024+1)).is_none());
        let mut nested="{}".to_owned();
        use base64::Engine;
        for _ in 0..10 {nested=serde_json::json!({"gp":base64::engine::general_purpose::STANDARD.encode(nested)}).to_string();}
        assert!(decode_event_json(&nested).is_none());
    }
    #[test]
    fn lifetime_reports_disconnect_once_even_when_dropped() {
        let count=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observer=count.clone();
        let sink:CallEventSink=std::sync::Arc::new(move |event|{assert_eq!(event.kind,crate::event_hub::TrouterEventKind::TransportDisconnected);observer.fetch_add(1,std::sync::atomic::Ordering::SeqCst);});
        {let mut guard=CallSinkLifetime{sink:Some(sink.clone()),connected:true};guard.disconnect();}
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst),1);
        {let _guard=CallSinkLifetime{sink:Some(sink),connected:true};}
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst),2);
    }
}

#[cfg(test)]
mod embedded_registration_failure_tests {
    use super::*;
    #[tokio::test]
    async fn rejected_registration_returns_to_reconnect_instead_of_starting_idle_socket_loop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/registrar", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 8192];
            let _ = socket.read(&mut bytes).await.unwrap();
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let result = time::timeout(
            Duration::from_secs(2),
            register_embedded(
                &http,
                "synthetic-test-token",
                Some(&url),
                "https://fixture.trouter.skype.com/root/",
                "synthetic-endpoint",
            ),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert_eq!(connection_failure_code(&error), "registration");
        assert!(!connection_failure_code(&error).contains("synthetic"));
        server.await.unwrap();
        let missing = register_embedded(
            &http,
            "synthetic",
            None,
            "https://fixture.trouter.skype.com/",
            "fixture",
        )
        .await
        .unwrap_err();
        assert_eq!(connection_failure_code(&missing), "registration");
        assert_eq!(
            connection_failure_code(&anyhow::anyhow!("https://secret.invalid/?token=synthetic")),
            "connection"
        );
    }
}
