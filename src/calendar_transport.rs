//! Regional native calendar reads recovered from original worker34946/91960.
//! Scheduled events use Graph; instant meetings use the native middleTier
//! scheduling service recovered from the shipped Teams client.
//! No redirects, cookies or response-provided pagination origins.
use crate::{
    auth::{CalendarCredentials, TeamsSession},
    calendar,
};
use chrono::{DateTime, Utc};
use std::{sync::OnceLock, time::Duration};

fn client() -> Result<&'static reqwest::Client, String> {
    static HTTP: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| "Não foi possível iniciar a conexão do calendário.".into())
    })
    .as_ref()
    .map_err(Clone::clone)
}

#[derive(Clone)]
pub struct InstantMeeting {
    pub subject: String,
    pub join_url: String,
}

impl std::fmt::Debug for InstantMeeting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstantMeeting([REDACTED])")
    }
}

fn parse_instant_meeting(body: &serde_json::Value, fallback: &str) -> Result<InstantMeeting, String> {
    let invalid = || "A reunião pode ter sido criada, mas o Teams não devolveu um link válido. Confira suas reuniões antes de tentar novamente.".to_owned();
    let value = body.get("value").ok_or_else(invalid)?;
    let raw = value.pointer("/links/join").and_then(|link| link.as_str()).ok_or_else(invalid)?;
    let link = crate::meeting::JoinLink::parse(raw).map_err(|_| invalid())?;
    let thread = link.thread_id();
    if thread.is_none() && !link.is_meeting_id_link() { return Err(invalid()); }
    if let Some(returned) = value.pointer("/groupContext/threadId") {
        let returned = returned.as_str().ok_or_else(invalid)?;
        if !returned.starts_with("19:") || !returned.ends_with("@thread.v2")
            || returned.len() > 2048
            || returned.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#'))
            || thread.as_deref().is_some_and(|id| id != returned) {
            return Err(invalid());
        }
    }
    let subject = value.get("subject").and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 250 && !s.chars().any(char::is_control))
        .unwrap_or(fallback).to_owned();
    Ok(InstantMeeting { subject, join_url: link.browser_url().to_owned() })
}

async fn post_instant_meeting(target: &str, credentials: &CalendarCredentials, subject: &str) -> Result<Vec<u8>, String> {
    let mut response = client()?.post(target)
        .bearer_auth(&credentials.aad)
        .header("X-Skypetoken", &credentials.skype)
        .header("x-ms-client-type", "desktop")
        .header("x-ms-client-version", env!("CARGO_PKG_VERSION"))
        .header("X-Client-UI-Language", "pt-BR")
        .header("Origin", "https://teams.microsoft.com")
        .header("Accept", "application/json")
        .json(&serde_json::json!({"meetingType":"MeetNow", "isStreamEnabled":false, "subject":subject, "unhideChatThread":true}))
        .send().await.map_err(|_| "Não foi possível confirmar a criação. A reunião pode ter sido criada; confira suas reuniões antes de tentar novamente.".to_owned())?;
    let status = response.status();
    crate::live_trace::record("instant_meeting_create", 0, usize::from(status.as_u16()), 0);
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "A sessão do Teams expirou. Entre novamente para criar a reunião.",
            403 => "O Teams não permitiu criar uma reunião com esta conta.",
            429 => "O Teams recebeu muitas solicitações. Aguarde um pouco antes de tentar novamente.",
            300..=399 => "O Teams retornou um redirecionamento inesperado ao criar a reunião.",
            _ => "O Teams não confirmou a criação. A reunião pode ter sido criada; confira suas reuniões antes de tentar novamente.",
        }.to_owned());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "A reunião pode ter sido criada, mas a resposta foi interrompida. Confira suas reuniões antes de tentar novamente.".to_owned())? {
        if chunk.len() > calendar::MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err("A reunião pode ter sido criada, mas a resposta foi grande demais. Confira suas reuniões antes de tentar novamente.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn instant_meeting_with_credentials(credentials: &CalendarCredentials, target: &str, subject: &str) -> Result<InstantMeeting, String> {
    let subject = subject.trim();
    if subject.is_empty() || subject.chars().count() > 250 || subject.chars().any(char::is_control) {
        return Err("Informe um título de reunião válido.".into());
    }
    let bytes = crate::graph::runtime()?.block_on(post_instant_meeting(target, credentials, subject))?;
    let body = serde_json::from_slice(&bytes).map_err(|_| "A reunião pode ter sido criada, mas a resposta não pôde ser lida. Confira suas reuniões antes de tentar novamente.".to_owned())?;
    parse_instant_meeting(&body, subject)
}

pub fn instant_meeting(session: TeamsSession, subject: String) -> Result<InstantMeeting, String> {
    let credentials = session.calendar_credentials()?;
    let target = calendar::native_base(&credentials.root)?
        .join("beta/me/calendarEvents/privateMeeting/schedulingService/create")
        .map_err(|_| "Não foi possível preparar a criação da reunião.".to_owned())?;
    instant_meeting_with_credentials(&credentials, target.as_str(), &subject)
}

async fn fetch_bytes(target: &str, credentials: &CalendarCredentials) -> Result<Vec<u8>, String> {
    let mut response = client()?
        .get(target)
        .bearer_auth(&credentials.aad)
        .header("X-Skypetoken", &credentials.skype)
        .header("x-ms-client-type", "desktop")
        .header("x-ms-client-version", env!("CARGO_PKG_VERSION"))
        .header("Origin", "https://teams.microsoft.com")
        .send()
        .await
        .map_err(|_| {
            "Não foi possível carregar a agenda. Verifique a conexão e tente novamente.".to_owned()
        })?;
    let status = response.status();
    crate::live_trace::record("calendar_http", 0, usize::from(status.as_u16()), 0);
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "A sessão do Teams expirou. Entre novamente para carregar a agenda.",
            403 => "O serviço do Teams não permitiu ler o calendário desta conta.",
            404 => "O serviço de calendário não está disponível nesta região ou conta.",
            429 => "O calendário recebeu muitas consultas. Aguarde um pouco e tente novamente.",
            300..=399 => "O calendário retornou um redirecionamento inesperado.",
            500..=599 => "O serviço do Teams está com erro ao carregar a agenda. Tente novamente em instantes.",
            _ => "Não foi possível carregar a agenda. Tente novamente em instantes.",
        }
        .to_owned());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "A resposta do calendário foi interrompida.".to_owned())?
    {
        if chunk.len() > calendar::MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err("A resposta do calendário excede o limite local.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn load(
    session: TeamsSession,
    next_link: Option<String>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(TeamsSession, calendar::CalendarPage), String> {
    let credentials = session.calendar_credentials()?;
    let target = match next_link {
        Some(link) => calendar::validate_native_page(&credentials.root, &link, start, end)?,
        None => calendar::native_view_url(&credentials.root, start, end, 0)?,
    };
    let skip = url::Url::parse(&target)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "$skip")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let started = std::time::Instant::now();
    let bytes = crate::graph::runtime()?.block_on(fetch_bytes(&target, &credentials))?;
    let body = std::str::from_utf8(&bytes)
        .map_err(|_| "O calendário retornou texto inválido.".to_owned())?;
    let page = calendar::parse_native_page(body, &credentials.root, start, end, skip)?;
    crate::live_trace::record(
        "calendar_page",
        started.elapsed().as_micros() as u64,
        page.events.len(),
        bytes.len(),
    );
    Ok((session, page))
}

fn meeting_url(root: &str, link: &str) -> Result<String, String> {
    let link = crate::meeting::JoinLink::parse(link)?;
    let mut target = calendar::native_base(root)?
        .join("v2.0/unified/calendarEvent/joinUrl")
        .map_err(|_| "Não foi possível preparar a consulta da reunião.".to_owned())?;
    target
        .query_pairs_mut()
        .append_pair("joinUrl", link.browser_url());
    Ok(target.into())
}

/// Read-only metadata lookup. It does not join a lobby or start media.
pub fn resolve_meeting(
    session: TeamsSession,
    link: &str,
) -> Result<(TeamsSession, calendar::CalendarEvent), String> {
    crate::meeting::JoinLink::parse(link)?;
    let credentials = session.calendar_credentials()?;
    let target = meeting_url(&credentials.root, link)?;
    let started = std::time::Instant::now();
    let bytes = crate::graph::runtime()?.block_on(fetch_bytes(&target, &credentials))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| "Não foi possível interpretar os detalhes da reunião.".to_owned())?;
    let event = calendar::parse_native_event(&value)?;
    crate::live_trace::record(
        "meeting_metadata",
        started.elapsed().as_micros() as u64,
        1,
        0,
    );
    Ok((session, event))
}

const GRAPH_EVENTS: &str = "https://graph.microsoft.com/v1.0/me/calendar/events";

async fn post_event(url: &str, token: &str, body: &serde_json::Value) -> Result<Vec<u8>, String> {
    let mut response = client()?
        .post(url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|_| {
            "Não foi possível criar a reunião. Verifique a conexão e tente novamente.".to_owned()
        })?;
    let status = response.status();
    crate::live_trace::record("calendar_create", 0, usize::from(status.as_u16()), 0);
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "A sessão expirou. Entre novamente para criar a reunião.",
            403 => "Esta conta não tem permissão para criar reuniões (Calendars.ReadWrite).",
            429 => "O calendário recebeu muitas consultas. Aguarde um pouco e tente novamente.",
            300..=399 => "O calendário retornou um redirecionamento inesperado.",
            _ => "Não foi possível criar a reunião. Tente novamente em instantes.",
        }
        .to_owned());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "A resposta do calendário foi interrompida.".to_owned())?
    {
        if chunk.len() > calendar::MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err("A resposta do calendário excede o limite local.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn create_with_token(
    token: &str,
    request: &calendar::ScheduleRequest,
    url: &str,
) -> Result<calendar::CalendarEvent, String> {
    ost::api::validate_schedule(
        &request.subject,
        &request.start,
        &request.end,
        request.time_zone,
    )
    .map_err(|_| "Revise o título e o horário da reunião.".to_owned())?;
    let body = calendar::graph_event_body(request);
    let bytes = crate::graph::runtime()?.block_on(post_event(url, token, &body))?;
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|_| "A reunião foi enviada, mas a resposta não pôde ser lida.".to_owned())?;
    let created = ost::api::parse_created_event(&value)
        .map_err(|_| "A reunião foi enviada, mas a resposta não pôde ser lida.".to_owned())?;
    calendar::scheduled_event(request, &created.id, created.join_url.as_deref())
}

pub fn create(
    session: TeamsSession,
    request: calendar::ScheduleRequest,
) -> Result<(TeamsSession, calendar::CalendarEvent), String> {
    let token = session.graph_bearer()?;
    let event = create_with_token(&token, &request, GRAPH_EVENTS)?;
    Ok((session, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credentials() -> CalendarCredentials {
        CalendarCredentials {
            root: String::new(),
            aad: "fixture-aad".into(),
            skype: "fixture-skype".into(),
        }
    }
    #[test]
    fn instant_meeting_posts_native_contract_and_uses_returned_join_link() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_string();
        let worker = std::thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert_eq!(request.method(), &tiny_http::Method::Post);
            assert_eq!(request.url(), "/api/mt/amer/beta/me/calendarEvents/privateMeeting/schedulingService/create");
            assert!(request.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == "Bearer fixture-aad"));
            assert!(request.headers().iter().any(|h| h.field.equiv("X-Skypetoken") && h.value.as_str() == "fixture-skype"));
            let mut data=String::new(); request.as_reader().read_to_string(&mut data).unwrap();
            let body: serde_json::Value=serde_json::from_str(&data).unwrap();
            assert_eq!(body, serde_json::json!({"meetingType":"MeetNow","isStreamEnabled":false,"subject":"Reunião de teste","unhideChatThread":true}));
            let answer=serde_json::json!({"value":{"subject":"Reunião de teste","groupContext":{"threadId":"19:meeting_fixture@thread.v2"},"links":{"join":"https://teams.microsoft.com/l/meetup-join/19%3Ameeting_fixture%40thread.v2/0"}}});
            request.respond(tiny_http::Response::from_string(answer.to_string()).with_header(tiny_http::Header::from_bytes("Content-Type","application/json").unwrap())).unwrap();
        });
        let target=format!("http://{address}/api/mt/amer/beta/me/calendarEvents/privateMeeting/schedulingService/create");
        let meeting = instant_meeting_with_credentials(&credentials(), &target, "Reunião de teste").unwrap();
        assert_eq!(meeting.join_url, "https://teams.microsoft.com/l/meetup-join/19%3Ameeting_fixture%40thread.v2/0");
        assert_eq!(meeting.subject, "Reunião de teste");
        worker.join().unwrap();
    }

    #[test]
    fn instant_meeting_rejects_untrusted_or_mismatched_returned_links() {
        let body=serde_json::json!({"value":{"groupContext":{"threadId":"19:meeting_other@thread.v2"},"links":{"join":"https://teams.microsoft.com/l/meetup-join/19%3Ameeting_fixture%40thread.v2/0"}}});
        assert!(parse_instant_meeting(&body, "Teste").is_err());
        let body=serde_json::json!({"value":{"links":{"join":"https://evil.test/meet/123"}}});
        assert!(parse_instant_meeting(&body, "Teste").is_err());
    }

    #[test]
    fn instant_meeting_failure_does_not_retry_post() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let target = format!("http://{}/create", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
            request.respond(tiny_http::Response::empty(503)).unwrap();
            assert!(server.recv_timeout(Duration::from_millis(300)).unwrap().is_none());
        });
        let error = instant_meeting_with_credentials(&credentials(), &target, "Teste").err().unwrap();
        assert!(error.contains("pode ter sido criada"));
        worker.join().unwrap();
    }

    #[test]
    fn meeting_lookup_keeps_nested_invitation_query_in_one_parameter() {
        let link = "https://teams.microsoft.com/l/meetup-join/fixture/0?context=%7B%22Tid%22%3A%22fake%22%7D&p=fake";
        let target = meeting_url("https://teams.microsoft.com/api/mt/amer/", link).unwrap();
        let url = url::Url::parse(&target).unwrap();
        assert_eq!(
            url.path(),
            "/api/mt/amer/v2.0/unified/calendarEvent/joinUrl"
        );
        let query: Vec<_> = url.query_pairs().collect();
        assert_eq!(query.len(), 1);
        assert_eq!(query[0].0, "joinUrl");
        assert_eq!(query[0].1, link);
        assert!(meeting_url("https://evil.test/api/mt/amer/", link).is_err());
        assert!(
            meeting_url(
                "https://teams.microsoft.com/api/mt/amer/",
                "https://evil.test/meet/1"
            )
            .is_err()
        );
    }
    #[test]
    fn redirect_never_receives_calendar_credentials() {
        let source = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let sink = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/calendar", source.server_addr());
        let location = format!("http://{}/other", sink.server_addr());
        let server = std::thread::spawn(move || {
            let request = source
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert!(request.headers().iter().any(
                |h| h.field.equiv("Authorization") && h.value.as_str() == "Bearer fixture-aad"
            ));
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("X-Skypetoken") && h.value.as_str() == "fixture-skype")
            );
            request
                .respond(
                    tiny_http::Response::empty(302)
                        .with_header(tiny_http::Header::from_bytes("Location", location).unwrap()),
                )
                .unwrap();
            assert!(
                sink.recv_timeout(Duration::from_millis(150))
                    .unwrap()
                    .is_none()
            );
        });
        let result = crate::graph::runtime()
            .unwrap()
            .block_on(fetch_bytes(&url, &credentials()));
        assert!(result.unwrap_err().contains("redirecionamento"));
        server.join().unwrap();
    }
    #[test]
    fn oversized_calendar_response_is_rejected_during_read() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/calendar", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let _ = request.respond(tiny_http::Response::from_data(vec![
                b' ';
                calendar::MAX_RESPONSE_BYTES
                    + 1
            ]));
        });
        assert!(
            crate::graph::runtime()
                .unwrap()
                .block_on(fetch_bytes(&url, &credentials()))
                .unwrap_err()
                .contains("limite")
        );
        worker.join().unwrap();
    }

    #[test]
    fn create_posts_the_graph_event_and_hides_a_forbidden_body() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let request = calendar::prepare_schedule("Planejamento", date, "09:00", "09:30", true).unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/me/calendar/events", server.server_addr());
        let denied = url.clone();
        let worker = std::thread::spawn(move || {
            let mut request = server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
            assert_eq!(request.method(), &tiny_http::Method::Post);
            assert!(request.url().starts_with("/me/calendar/events"));
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["subject"], "Planejamento");
            assert_eq!(body["isOnlineMeeting"], true);
            assert_eq!(body["onlineMeetingProvider"], "teamsForBusiness");
            assert_eq!(body["showAs"], "busy");
            assert!(body.get("attendees").is_none());
            assert!(
                request.headers().iter().any(|header| {
                    header.field.equiv("Authorization") && header.value.as_str() == "Bearer fixture-graph"
                })
            );
            request
                .respond(tiny_http::Response::from_string("SECRET_LEAK").with_status_code(403))
                .unwrap();
        });
        let denied = create_with_token("fixture-graph", &request, &denied).unwrap_err();
        assert!(denied.contains("Calendars.ReadWrite"));
        assert!(!denied.contains("SECRET"));
        worker.join().unwrap();

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/me/calendar/events", server.server_addr());
        let created = url.clone();
        let worker = std::thread::spawn(move || {
            let request = server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"id":"graph-1","subject":"Planejamento","isOnlineMeeting":true,"onlineMeeting":{"joinUrl":"https://teams.microsoft.com/l/meetup-join/fixture/0"}}"#,
                    )
                    .with_status_code(201),
                )
                .unwrap();
        });
        let event = create_with_token("fixture-graph", &request, &created).unwrap();
        assert_eq!(event.id, "scheduled:graph-1");
        assert_eq!(
            event.join_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/fixture/0")
        );
        assert!(event.provisional);
        worker.join().unwrap();
    }
}
