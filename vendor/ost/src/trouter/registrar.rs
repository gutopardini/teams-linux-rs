//! Trouter registrar — registers our endpoint with the Teams notification service

use anyhow::{Context, Result};
use std::time::Duration;

/// IDs created for one registrar session so refreshes reuse them and logout
/// can remove the registrations created by that session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistrationSet {
    registrar_url: String,
    registration_ids: Vec<String>,
}

/// Registration entry: appId, templateKey, path suffix, context
struct RegEntry {
    app_id: &'static str,
    template_key: &'static str,
    path_suffix: &'static str,
    context: &'static str,
}

const REGISTRATIONS: &[RegEntry] = &[
    RegEntry {
        // OstMac §106: business-tenant CDL registration (template 2.1, empty
        // transport context). Was 2.6 + transport context "TFL" (the
        // consumer "Teams for Life" product context, and in the wrong
        // field): registrations succeeded but no chat-service messaging
        // push ever reached the socket.
        app_id: "TeamsCDLWebWorker",
        template_key: "TeamsCDLWebWorker_2.1",
        path_suffix: "",
        context: "",
    },
    RegEntry {
        app_id: "SkypeSpacesWeb",
        template_key: "SkypeSpacesWeb_2.4",
        path_suffix: "SkypeSpacesWeb",
        context: "",
    },
    RegEntry {
        app_id: "NextGenCalling",
        template_key: "DesktopNgc_2.5:SkypeNgc",
        path_suffix: "NGCallManagerWin",
        context: "",
    },
];

/// Register our trouter endpoint with the Teams registrar service.
///
/// Performs three separate registrations (TeamsCDLWebWorker, SkypeSpacesWeb,
/// NextGenCalling) as the real Teams client does.
pub async fn register(
    http: &reqwest::Client,
    skype_token: &str,
    registrar_url: &str,
    trouter_surl: &str,
) -> Result<()> {
    register_with_endpoint(http, skype_token, registrar_url, trouter_surl, None).await
}

/// OstMac §106: [`register`] where the CDL (chat) registration's
/// `registrationId` is the socket's endpoint id (`epid`), as the Teams
/// clients do; other apps keep a random id.
pub async fn register_with_endpoint(
    http: &reqwest::Client,
    skype_token: &str,
    registrar_url: &str,
    trouter_surl: &str,
    epid: Option<&str>,
) -> Result<()> {
    register_with_endpoint_tracked(http, skype_token, registrar_url, trouter_surl, epid)
        .await
        .map(|_| ())
}

/// Register the desktop notification endpoints and retain the assigned IDs.
/// If a later registration fails, successful earlier registrations are rolled
/// back before the error is returned.
pub async fn register_with_endpoint_tracked(
    http: &reqwest::Client,
    skype_token: &str,
    registrar_url: &str,
    trouter_surl: &str,
    epid: Option<&str>,
) -> Result<RegistrationSet> {
    let registrar_url = registrar_url.trim_end_matches('/').to_owned();
    let registration_ids = REGISTRATIONS
        .iter()
        .map(|entry| match epid {
            Some(endpoint_id)
                if entry.app_id == "TeamsCDLWebWorker" && !endpoint_id.trim().is_empty() =>
            {
                endpoint_id.to_owned()
            }
            _ => uuid::Uuid::new_v4().to_string(),
        })
        .collect::<Vec<_>>();
    let mut registered = RegistrationSet {
        registrar_url,
        registration_ids: Vec::with_capacity(registration_ids.len()),
    };

    for (entry, registration_id) in REGISTRATIONS.iter().zip(registration_ids) {
        if let Err(error) =
            post_registration(http, skype_token, trouter_surl, &registered, entry, &registration_id)
                .await
        {
            let _ = unregister(http, skype_token, &registered).await;
            return Err(error);
        }
        registered.registration_ids.push(registration_id);
    }
    Ok(registered)
}

/// Refresh registrations using the same IDs, avoiding duplicate endpoints.
pub async fn refresh(
    http: &reqwest::Client,
    skype_token: &str,
    trouter_surl: &str,
    registrations: &RegistrationSet,
) -> Result<()> {
    post_registrations(http, skype_token, trouter_surl, registrations).await
}

/// Remove all registrations owned by this session. Missing registrations are
/// treated as already removed, making cleanup safe after expiry or retries.
pub async fn unregister(
    http: &reqwest::Client,
    skype_token: &str,
    registrations: &RegistrationSet,
) -> Result<()> {
    let mut first_error = None;
    for registration_id in registrations.registration_ids.iter().rev() {
        let url = registration_url(&registrations.registrar_url, registration_id)?;
        match http
            .delete(url)
            .header("X-Skypetoken", skype_token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(response)
                if response.status().is_success()
                    || matches!(response.status().as_u16(), 404 | 410) => {}
            Ok(response) => {
                first_error.get_or_insert_with(|| {
                    anyhow::anyhow!("Registrar DELETE returned HTTP {}", response.status())
                });
            }
            Err(_) => {
                first_error.get_or_insert_with(|| anyhow::anyhow!("Registrar DELETE failed"));
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

async fn post_registrations(
    http: &reqwest::Client,
    skype_token: &str,
    trouter_surl: &str,
    registrations: &RegistrationSet,
) -> Result<()> {
    anyhow::ensure!(
        registrations.registration_ids.len() == REGISTRATIONS.len(),
        "Trouter registration set is incomplete"
    );
    for (entry, registration_id) in REGISTRATIONS.iter().zip(&registrations.registration_ids) {
        post_registration(
            http,
            skype_token,
            trouter_surl,
            registrations,
            entry,
            registration_id,
        )
        .await?;
    }
    Ok(())
}

async fn post_registration(
    http: &reqwest::Client,
    skype_token: &str,
    trouter_surl: &str,
    registrations: &RegistrationSet,
    entry: &RegEntry,
    registration_id: &str,
) -> Result<()> {
    let payload = serde_json::json!({
        "clientDescription": {
            "appId": entry.app_id,
            "aesKey": "",
            "languageId": "en-US",
            "platform": "edge",
            "templateKey": entry.template_key,
            "platformUIVersion": "49/1.0.0"
        },
        "registrationId": registration_id,
        "nodeId": "",
        "transports": {
            "TROUTER": [{
                "context": entry.context,
                "path": format!("{}{}", trouter_surl, entry.path_suffix),
                "ttl": 86400
            }]
        }
    });
    let response = http
        .post(&registrations.registrar_url)
        .header("X-Skypetoken", skype_token)
        .json(&payload)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("Registrar POST failed")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Registrar {} returned HTTP {}",
            entry.app_id,
            response.status()
        );
    }
    tracing::info!("Registrar {} registration succeeded", entry.app_id);
    Ok(())
}

fn registration_url(registrar_url: &str, registration_id: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(registrar_url).context("Invalid registrar URL")?;
    let path = format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        registration_id
    );
    url.set_path(&path);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{register_with_endpoint_tracked, refresh, unregister};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "request ended before headers/body completed");
            bytes.extend_from_slice(&chunk[..count]);
            let Some(headers_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..headers_end]);
            let body_len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= headers_end + 4 + body_len {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }

    fn server(statuses: &'static [&'static str]) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/v1/registrations/", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (address, worker)
    }

    #[tokio::test]
    async fn tracked_registrations_refresh_and_delete_the_same_ids() {
        let (base_url, server) = server(&[
            "204 No Content",
            "204 No Content",
            "204 No Content",
            "204 No Content",
            "204 No Content",
            "204 No Content",
            "404 Not Found",
            "404 Not Found",
            "404 Not Found",
        ]);
        let http = reqwest::Client::new();
        let registrations = register_with_endpoint_tracked(
            &http,
            "fixture-skype-token",
            &base_url,
            "https://trouter.fixture/session",
            Some("endpoint-fixture"),
        )
        .await
        .unwrap();
        assert_eq!(registrations.registration_ids.len(), 3);
        assert_eq!(registrations.registration_ids[0], "endpoint-fixture");

        refresh(
            &http,
            "fixture-skype-token",
            "https://trouter.fixture/session",
            &registrations,
        )
        .await
        .unwrap();
        unregister(&http, "fixture-skype-token", &registrations)
            .await
            .unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 9);
        for request in &requests[..6] {
            assert!(request.starts_with("POST /v1/registrations HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("x-skypetoken: fixture-skype-token"));
        }
        for (request, registration_id) in requests[6..]
            .iter()
            .zip(registrations.registration_ids.iter().rev())
        {
            assert!(request.starts_with(&format!(
                "DELETE /v1/registrations/{registration_id} HTTP/1.1"
            )));
            assert!(request
                .to_ascii_lowercase()
                .contains("x-skypetoken: fixture-skype-token"));
        }
        let first_ids = requests[..3]
            .iter()
            .map(|request| {
                let body = request.split_once("\r\n\r\n").unwrap().1;
                serde_json::from_str::<serde_json::Value>(body).unwrap()["registrationId"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let refreshed_ids = requests[3..6]
            .iter()
            .map(|request| {
                let body = request.split_once("\r\n\r\n").unwrap().1;
                serde_json::from_str::<serde_json::Value>(body).unwrap()["registrationId"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(first_ids, refreshed_ids);
    }

    #[tokio::test]
    async fn partial_registration_rolls_back_only_completed_posts() {
        let (base_url, server) = server(&[
            "204 No Content",
            "503 Service Unavailable",
            "204 No Content",
        ]);
        let result = register_with_endpoint_tracked(
            &reqwest::Client::new(),
            "fixture-skype-token",
            &base_url,
            "https://trouter.fixture/session",
            Some("endpoint-fixture"),
        )
        .await;
        assert!(result.is_err());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /v1/registrations HTTP/1.1"));
        assert!(requests[1].starts_with("POST /v1/registrations HTTP/1.1"));
        assert!(requests[2].starts_with("DELETE /v1/registrations/endpoint-fixture HTTP/1.1"));
        assert_eq!(requests.len(), 3);
    }
}

/// OstMac §106: re-register only the CDL (chat) entry under `template_key`
/// with `registrationId` = `epid` (same id → replaces the first one). The
/// Teams clients do this with `TeamsCDLWebWorker_1.9` when the server
/// reports `trouter.message_loss` right after connect.
pub async fn register_cdl(
    http: &reqwest::Client,
    skype_token: &str,
    registrar_url: &str,
    trouter_surl: &str,
    epid: &str,
    template_key: &str,
) -> Result<()> {
    let payload = serde_json::json!({
        "clientDescription": {
            "appId": "TeamsCDLWebWorker",
            "aesKey": "",
            "languageId": "en-US",
            "platform": "edge",
            "templateKey": template_key,
            "platformUIVersion": "49/1.0.0"
        },
        "registrationId": epid,
        "nodeId": "",
        "transports": {"TROUTER": [{"context": "", "path": trouter_surl, "ttl": 86400}]}
    });
    let resp = http
        .post(registrar_url.trim_end_matches('/'))
        .header("X-Skypetoken", skype_token)
        .json(&payload)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("Registrar POST failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Registrar CDL {} returned {}", template_key, status);
    }
    tracing::info!("Registrar CDL {} registration succeeded", template_key);
    Ok(())
}
