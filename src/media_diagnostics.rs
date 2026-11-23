//! Read-only media-shape inspection. Never serializes content, identities or URLs.
use serde_json::{Value, json};

fn url_kind(value: &str) -> &'static str {
    if crate::images::valid_media_url(value) { return "ams"; }
    match url::Url::parse(value) {
        Ok(url) if url.scheme()=="https" && url.host_str().is_some_and(|host| host.ends_with(".sharepoint.com")) => "sharepoint",
        Ok(_) => "other",
        Err(_) => "absent_or_invalid",
    }
}

fn shape(message: &ost::api::MessageInfo) -> Value {
    let html = scraper::Html::parse_fragment(&message.raw);
    let attachments = scraper::Selector::parse("attachment[id]").unwrap();
    let images = scraper::Selector::parse("img[src]").unwrap();
    json!({
        "body_present": !message.content.trim().is_empty(),
        "html_attachment_count": html.select(&scraper::Selector::parse("attachment").unwrap()).count(),
        "html_images": html.select(&images).map(|image|url_kind(image.value().attr("src").unwrap_or_default())).collect::<Vec<_>>(),
        "files": message.files.iter().take(32).map(|file|json!({
            "raster_name": file.name.rsplit_once('.').is_some_and(|(_,ext)| ["png","jpg","jpeg","gif","webp","bmp","tiff"].iter().any(|kind|ext.eq_ignore_ascii_case(kind))),
            "type_matches_name": file.name.rsplit_once('.').zip(file.file_type.as_deref()).is_some_and(|((_,ext),kind)|ext.eq_ignore_ascii_case(kind)),
            "preview": file.preview.as_ref().map(|preview|url_kind(&preview.url)),
            "object": url_kind(&file.object_url),
            "share": file.share_url.as_deref().map(url_kind),
            "attachment_matches": file.attachment_id.as_deref().map(|id|html.select(&attachments).filter(|slot|slot.value().attr("id")==Some(id)).count()),
        })).collect::<Vec<_>>()
    })
}

pub fn inspect(config: crate::auth::AuthConfig, query: &str) -> Result<Value, &'static str> {
    let session = crate::auth::restore_session(config).map_err(|_|"authentication")?.ok_or("no_saved_session")?;
    let client = session.native_client().map_err(|_|"native_client")?;
    let (owner, _) = session.account_hint();
    let runtime = tokio::runtime::Runtime::new().map_err(|_|"runtime")?;
    runtime.block_on(async {
        let chats = ost::api::list_chats_data_for_owner(&client,100,&owner).await.map_err(|_|"directory")?;
        let query = query.to_lowercase();
        let matching:Vec<_> = chats.iter().filter(|chat|chat.is_group && chat.name.to_lowercase().contains(&query)).collect();
        if matching.len()!=1 { return Err("chat_not_unique"); }
        let page = ost::api::read_messages_page(&client,&matching[0].id,100,None).await.map_err(|_|"history")?;
        let previews = page.messages.iter().flat_map(|message| &message.files)
            .filter_map(crate::images::file_preview).take(4).collect::<Vec<_>>();
        let downloads = previews.iter().map(|image| match crate::images::download(session.clone(), &image.url) {
            Ok(loaded) => json!({"decoded":true,"width":loaded.width,"height":loaded.height}),
            Err(_) => json!({"decoded":false}),
        }).collect::<Vec<_>>();
        Ok(json!({"schema":1,"read_only":true,"messages_inspected":page.messages.len(),"media_messages":page.messages.iter().filter(|message|!message.files.is_empty() || message.raw.contains("<img")).map(shape).collect::<Vec<_>>(),"file_preview_downloads":downloads}))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn classification_never_emits_url_or_identity() {
        for value in ["https://private.sharepoint.com/personal/secret/capture.png?token=secret", "https://api.asm.skype.com/v1/objects/private/views/imgo", "https://private.example/secret"] {
            let result=serde_json::to_string(&super::url_kind(value)).unwrap();
            assert!(!result.contains("private") && !result.contains("secret"));
        }
    }
}

/// Aggregate notification eligibility only; no identifiers, titles or previews.
pub fn notification_directory(config: crate::auth::AuthConfig) -> Result<Value, &'static str> {
    let session = crate::auth::restore_session(config).map_err(|_|"authentication")?.ok_or("no_saved_session")?;
    let client = session.native_client().map_err(|_|"native_client")?;
    let (owner, _) = session.account_hint();
    tokio::runtime::Runtime::new().map_err(|_|"runtime")?.block_on(async {
        let chats = ost::api::list_chats_data_for_owner(&client,100,&owner).await.map_err(|_|"directory")?;
        let owner = crate::avatars::person_id(&owner);
        Ok(json!({"read_only":true,"total":chats.len(),"owner_identity_valid":owner.is_some(),
            "sender_present":chats.iter().filter(|c|c.last_message_sender_mri.is_some()).count(),
            "remote_confirmed":chats.iter().filter(|c| c.last_message_sender_mri.as_deref().and_then(crate::avatars::person_id).zip(owner.as_ref()).is_some_and(|(s,o)|s!=*o)).count(),
            "valid_times":chats.iter().filter(|c|c.last_message_time.as_deref().is_some_and(|t|chrono::DateTime::parse_from_rfc3339(t).is_ok())).count(),
            "muted":chats.iter().filter(|c|c.muted==Some(true)).count(),"hidden":chats.iter().filter(|c|c.hidden==Some(true)).count()}))
    })
}

/// Read-only probe: outputs counts and bounded error categories, never event data.
pub fn calendar_status(config: crate::auth::AuthConfig) -> Result<Value, &'static str> {
    let session = crate::auth::restore_session(config).map_err(|_| "authentication")?.ok_or("no_saved_session")?;
    let credentials = session.calendar_credentials().map_err(|_| "credentials")?;
    let (start,end) = crate::calendar::range_for(chrono::Local::now().date_naive(),crate::calendar::CalendarView::WorkWeek).map_err(|_| "range")?;
    let target = crate::calendar::native_view_url(&credentials.root,start,end,0).map_err(|_| "target")?;
    crate::graph::runtime().map_err(|_| "runtime")?.block_on(async {
        let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(20)).build().map_err(|_|"client")?;
        let mut response=client.get(target).bearer_auth(&credentials.aad).header("X-Skypetoken",&credentials.skype)
            .header("x-ms-client-type","desktop").header("x-ms-client-version",env!("CARGO_PKG_VERSION"))
            .header("Origin","https://teams.microsoft.com").send().await.map_err(|_|"request")?;
        let status=response.status().as_u16();
        let mut body=Vec::new();
        while let Some(chunk)=response.chunk().await.map_err(|_|"response")? {
            if chunk.len()>crate::calendar::MAX_RESPONSE_BYTES.saturating_sub(body.len()){return Err("response_limit");}
            body.extend_from_slice(&chunk);
        }
        let value:Value=serde_json::from_slice(&body).unwrap_or(Value::Null);
        let code=value.pointer("/error/code").and_then(Value::as_str).unwrap_or("");
        let category=match code {"ErrorMailboxNotEnabledForRESTAPI"|"MailboxNotEnabledForRESTAPI"=>"mailbox_unavailable", "ErrorAccessDenied"|"AccessDenied"=>"access_denied", "InvalidAuthenticationToken"=>"authentication", "InternalServerError"|"ErrorInternalServerError"=>"internal_server_error", _=>"unclassified"};
        Ok(json!({"read_only":true,"status":status,"event_count":value.get("value").and_then(Value::as_array).map(Vec::len),"error_category":category}))
    })
}
