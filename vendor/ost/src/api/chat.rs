//! Native Teams chat API (chatsvcagg / chat service)
//!
//! Uses the Skype token with `Authentication: skypetoken={token}` header,
//! bypassing Graph API which requires tenant admin consent for Chat.Read.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;

use super::client::TeamsClient;
use super::me::whoami_data;

// -- Response types for the native chat API --

#[derive(Debug, Deserialize)]
struct ConversationsResponse {
    conversations: Option<Vec<Conversation>>,
}

#[derive(Debug, Deserialize)]
struct Conversation {
    id: Option<String>,
    #[serde(rename = "threadProperties")]
    thread_properties: Option<ThreadProperties>,
    /// Resolver-visible unread marker. A missing value stays unknown.
    #[serde(rename = "showRedDot")]
    show_red_dot: Option<Value>,
    #[serde(default)]
    properties: Option<Value>,
    #[serde(rename = "lastMessage")]
    last_message: Option<NativeMessage>,
}

#[derive(Debug, Deserialize)]
struct ThreadProperties {
    topic: Option<String>,
    picture: Option<String>,
    #[serde(rename = "threadType")]
    thread_type: Option<String>,
    #[serde(rename = "lastjoinat")]
    last_join_at: Option<String>,
    /// For 1:1 chats, contains member MRIs
    members: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NativeMessage {
    id: Option<String>,
    #[serde(rename = "composetime")]
    compose_time: Option<String>,
    #[serde(rename = "originalarrivaltime")]
    original_arrival_time: Option<String>,
    #[serde(rename = "imdisplayname")]
    im_display_name: Option<String>,
    content: Option<String>,
    messagetype: Option<String>,
    from: Option<String>,
    /// OstMac om-reactions: per-message reactions when the server sends
    /// them (Graph-like list). Absent on old payloads → no counts.
    /// Kept raw (core-a) so reactor ids/names survive unknown shapes.
    reactions: Option<serde_json::Value>,
    /// Alternate nesting some payloads use (`properties.reactions`).
    properties: Option<MessageProperties>,
    /// Unknown top-level wire fields (om-lt2-quotelink): channel thread
    /// parents (`rootMessageId` / `replyToId`) land here; mined
    /// case-insensitively, never fatal.
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MessageProperties {
    reactions: Option<serde_json::Value>,
    /// Unknown `properties.*` fields (om-lt2-quotelink): same parent
    /// mining as top-level, for nested channel shapes.
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    messages: Option<Vec<NativeMessage>>,
    #[serde(rename = "_metadata")]
    metadata: Option<MessagesMetadata>,
}

#[derive(Debug, Deserialize)]
struct MessagesMetadata {
    #[serde(rename = "backwardLink")]
    backward_link: Option<String>,
}

/// Block-level tags: a boundary here separates words, so it yields one
/// space (pending, only between two non-space chars). Inline tags
/// (`b`, `i`, `at`, `code`, …) vanish silently so `a<b>x</b>b` stays glued.
const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "br",
    "section",
    "article",
    "header",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "tr",
    "td",
    "th",
    "blockquote",
    "pre",
    "hr",
];

/// Tag name of a raw `<…>` body: attributes and the `/` of closing
/// or self-closed tags stripped, case preserved (caller matches
/// case-insensitively). `<>` yields "".
fn tag_name(body: &str) -> &str {
    let b = body.strip_prefix('/').unwrap_or(body);
    let end = b
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(b.len());
    &b[..end]
}

/// Strip HTML tags from content for CLI display.
///
/// Spacing-aware (om-chatnames): block-level tag boundaries become a
/// single space so `</p><p>` never glues words ("tag-boundary glue").
/// No leading/trailing space is added (`<p>hi</p>` → `hi`).
fn directory_preview_body(message: &NativeMessage) -> String {
    let raw = message.content.as_deref().unwrap_or_default();
    let text = preview_without_quotes(raw);
    let kind = message.messagetype.as_deref().unwrap_or_default();
    if kind.eq_ignore_ascii_case("RichText/Media_GenericFile") {
        "Enviou um arquivo".into()
    } else if !text.trim().is_empty() { text
    } else if raw.to_ascii_lowercase().contains("/amsimage") {
        "Enviou uma imagem".into()
    } else { text }
}

fn preview_without_quotes(raw: &str) -> String {
    let (_, body) = split_reply_quote(raw);
    let lower = body.to_ascii_lowercase();
    let mut result = String::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<blockquote") {
        let start = cursor + offset;
        result.push_str(&body[cursor..start]);
        let mut depth = 1;
        let mut position = start + "<blockquote".len();
        loop {
            let close = lower[position..].find("</blockquote>").map(|n| position + n);
            let open = lower[position..].find("<blockquote").map(|n| position + n);
            match (open, close) {
                (Some(open), Some(close)) if open < close => { depth += 1; position = open + "<blockquote".len(); }
                (_, Some(close)) => {
                    depth -= 1; position = close + "</blockquote>".len();
                    if depth == 0 { cursor = position; break; }
                }
                _ => { result.push_str(&body[start..]); return strip_html(&result); }
            }
        }
    }
    result.push_str(&body[cursor..]);
    strip_html(&result)
}

fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut tag = String::new();
    let mut in_tag = false;
    let mut pending_space = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                if BLOCK_TAGS.contains(&tag_name(&tag).to_lowercase().as_str()) {
                    pending_space = true;
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else {
            if pending_space {
                pending_space = false;
                if !result.is_empty()
                    && !result.ends_with(char::is_whitespace)
                    && !ch.is_whitespace()
                {
                    result.push(' ');
                }
            }
            result.push(ch);
        }
    }
    // Decode common HTML entities
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Human label for a chat with no topic, mate, or sender — never the
/// raw thread id (om-chatnames). `48:xxx` system chats humanize their
/// suffix (`48:notifications` → `Notifications`); anything else gets a
/// shape-based label (`[Direct message]`, `[Group chat]`, …).
fn system_label_for(chat_id: &str) -> String {
    if let Some(rest) = chat_id.strip_prefix("48:") {
        let mut chars = rest.chars();
        match chars.next() {
            None => return "[System chat]".to_string(),
            Some(first) => {
                return format!(
                    "{}{}",
                    first.to_uppercase().collect::<String>(),
                    chars.as_str()
                );
            }
        }
    }
    if chat_id.contains("meeting") {
        "[Meeting chat]"
    } else if chat_id.contains("@thread") {
        "[Group chat]"
    } else if chat_id.starts_with("19:") {
        "[Direct message]"
    } else {
        "[Chat]"
    }
    .to_string()
}

/// Unresolved direct chats have a neutral label. Only apply_peer_names may
/// attach a peer name after MRI attribution. Other thread labels are unchanged.
fn conversation_name(conv: &Conversation) -> String {
    if is_onetoone_id(conv.id.as_deref().unwrap_or("")) {
        return system_label_for(conv.id.as_deref().unwrap_or(""));
    }
    if let Some(ref props) = conv.thread_properties {
        if let Some(ref topic) = props.topic {
            if !topic.trim().is_empty() {
                return topic.clone();
            }
        }
    }
    if let Some(ref msg) = conv.last_message {
        if let Some(ref name) = msg.im_display_name {
            if !name.trim().is_empty() {
                return name.clone();
            }
        }
    }
    system_label_for(conv.id.as_deref().unwrap_or(""))
}

/// List recent chats using the native Teams API (prints to stdout).
pub async fn list_chats(limit: usize) -> Result<()> {
    let client = TeamsClient::new().await?;
    let chats = list_chats_data(&client, limit).await?;

    println!("\nRecent Chats:");
    println!("{:-<60}", "");

    if chats.is_empty() {
        println!("  (no chats found)");
        return Ok(());
    }

    for chat in &chats {
        println!("{}", chat.name);
        println!("  ID: {}", chat.id);

        if let Some(ref time) = chat.last_message_time {
            println!("  Last: {}", time);
        }
        if let Some(ref preview) = chat.last_message_preview {
            if !preview.trim().is_empty() {
                let sender = chat.last_message_sender.as_deref().unwrap_or("?");
                println!("  [{}]: {}", sender, preview.trim());
            }
        }

        println!();
    }

    Ok(())
}

/// Read messages from a specific chat thread (prints to stdout).
pub async fn read_messages(chat_id: &str, limit: usize) -> Result<()> {
    let client = TeamsClient::new().await?;
    let msgs = read_messages_data(&client, chat_id, limit).await?;

    if msgs.is_empty() {
        println!("(no messages)");
        return Ok(());
    }

    for msg in &msgs {
        // OstMac om-botposts: kept card posts carry empty `content` (the
        // embedder mines rows from `raw`) — never print a blank line.
        // Image-only bubbles keep their own shape (empty body, raw has
        // `<img>`); only card payloads get the marker.
        let body = if msg.content.trim().is_empty() && has_card_payload(&msg.raw) {
            "(card post)"
        } else {
            msg.content.as_str()
        };
        match &msg.reply_to {
            Some(parent) => println!(
                "[{}] {}: {} (reply to {})",
                msg.timestamp, msg.sender, body, parent
            ),
            None => println!("[{}] {}: {}", msg.timestamp, msg.sender, body),
        }
    }

    Ok(())
}

/// Send a message to a chat thread using the native API.
pub async fn send_message(chat_id: &str, message: &str) -> Result<()> {
    let client = TeamsClient::new().await?;
    send_message_with_client(&client, chat_id, message).await?;
    println!("Message sent.");
    Ok(())
}

/// Reply to one message in a chat thread (quote reply).
///
/// Resolves the parent from the newest history page for quote attribution;
/// errors clearly when the parent id is not in recent history.
pub async fn reply_message(chat_id: &str, parent_id: &str, message: &str) -> Result<()> {
    let client = TeamsClient::new().await?;
    let msgs = read_messages_data(&client, chat_id, 50).await?;
    let parent = msgs.iter().find(|m| m.id == parent_id).ok_or_else(|| {
        anyhow::anyhow!("parent message {} not found in recent history", parent_id)
    })?;
    reply_message_with_client(
        &client,
        chat_id,
        &parent.id,
        &parent.sender,
        &parent.content,
        message,
    )
    .await?;
    println!("Reply sent.");
    Ok(())
}

/// HTML-escape text for embedding in Teams RichText/Html messages.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Max fenced code blocks parsed per outbound message (hostile-input
/// cap, mirrors Swift `CodeBlocks.maxBlocks`); extra fences stay prose.
pub const WIRE_FENCE_MAX_BLOCKS: usize = 50;

/// One line classified as a fence opener: (fence char, run length).
/// Mirrors Swift `CodeBlocks.fenceMarker`: any indent, ``` or ~~~ runs
/// of ≥ 3, info strings must not contain the fence char (CommonMark).
fn wire_fence_opener(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches([' ', '\t']);
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = t.chars().take_while(|&ch| ch == c).count();
    if len < 3 {
        return None;
    }
    let rest: String = t.chars().skip(len).collect();
    if rest.trim().is_empty() {
        return Some((c, len));
    }
    if rest.contains(c) {
        return None;
    }
    Some((c, len))
}

/// A closer line: same-char run ≥ opening length + nothing but
/// whitespace after (info-carrying lines never close).
fn wire_fence_closer(line: &str, ch: char, len: usize) -> bool {
    let t = line.trim_start_matches([' ', '\t']);
    let run = t.chars().take_while(|&c| c == ch).count();
    if run < len {
        return false;
    }
    t.chars().skip(run).collect::<String>().trim().is_empty()
}

/// Outbound wire HTML for a composer body. Fence-less messages keep the
/// legacy single-`<p>` shape bit-identical; each fenced block becomes a
/// `<pre>` (HTML preserves its newlines/indents in every client) and
/// surrounding prose becomes `<p>` chunks. Fence lines are consumed,
/// info strings dropped (the wire carries no highlighter), code
/// interiors byte-exact modulo HTML-escaping. Unclosed fences run to
/// end of text (Swift `CodeBlocks` parity).
pub fn build_message_html(message: &str) -> String {
    if !message.lines().any(|l| wire_fence_opener(l).is_some()) {
        return format!("<p>{}</p>", html_escape(message));
    }
    enum Seg {
        Prose(String),
        Code(String),
    }
    let mut segs: Vec<Seg> = Vec::new();
    let mut prose = String::new();
    let mut code: Option<(Vec<String>, char, usize)> = None;
    let mut blocks = 0;
    for line in message.split('\n') {
        if let Some((mut lines, ch, len)) = code.take() {
            if wire_fence_closer(line, ch, len) {
                segs.push(Seg::Code(lines.join("\n")));
            } else {
                lines.push(line.to_string());
                code = Some((lines, ch, len));
            }
            continue;
        }
        if blocks < WIRE_FENCE_MAX_BLOCKS {
            if let Some((ch, len)) = wire_fence_opener(line) {
                if !prose.is_empty() {
                    segs.push(Seg::Prose(std::mem::take(&mut prose)));
                }
                code = Some((Vec::new(), ch, len));
                blocks += 1;
                continue;
            }
        }
        if !prose.is_empty() {
            prose.push('\n');
        }
        prose.push_str(line);
    }
    if let Some((lines, _, _)) = code.take() {
        segs.push(Seg::Code(lines.join("\n")));
    } else if !prose.is_empty() {
        segs.push(Seg::Prose(prose));
    }
    let mut out = String::new();
    for s in segs {
        match s {
            Seg::Prose(t) => {
                // Whitespace-only prose renders blank either way; skip it
                // so whole-message fences emit a lone <pre>.
                if !t.trim().is_empty() {
                    out.push_str(&format!("<p>{}</p>", html_escape(&t)));
                }
            }
            Seg::Code(t) => out.push_str(&format!("<pre>{}</pre>", html_escape(&t))),
        }
    }
    out
}

/// POST body for sending one chat message (captured-body seam for
/// tests: the exact JSON `chat_post` receives, minus transport).
pub fn send_message_body(message: &str) -> serde_json::Value {
    serde_json::json!({
        "content": build_message_html(message),
        "messagetype": "RichText/Html",
        "contenttype": "text"
    })
}

/// POST body for semantic HTML serialized by the native rich-text editor.
/// The editor owns escaping and URL validation before calling this seam.
pub fn send_message_html_body(html: &str) -> serde_json::Value {
    serde_json::json!({
        "content": html,
        "messagetype": "RichText/Html",
        "contenttype": "text"
    })
}

/// Send a message using an existing client (shared helper).
pub async fn send_message_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message: &str,
) -> Result<()> {
    let base = client.chat_service_url();
    let url = format!("{}/v1/users/ME/conversations/{}/messages", base, chat_id);

    let body = send_message_body(message);

    tracing::debug!("Sending message to {}", url);
    client.chat_post(&url, &body).await?;
    Ok(())
}

/// Publish the native transient typing control for one conversation.
pub async fn send_typing_indicator_with_client(
    client: &TeamsClient,
    chat_id: &str,
) -> Result<()> {
    let id = chat_id.trim();
    if id.is_empty() {
        bail!("empty chat_id");
    }
    let url = format!(
        "{}/v1/users/ME/conversations/{id}/messages",
        client.chat_service_url()
    );
    let body = serde_json::json!({
        "messagetype": "Control/Typing",
        "contenttype": "Application/Message",
        "content": ""
    });
    client.chat_post(&url, &body).await?;
    Ok(())
}

// OstMac §106 (SENDFIX): idempotent sends. Every post carries a
// `clientmessageid`; the server receipt (Location / OriginalArrivalTime)
// names the posted copy, and `find_message_by_client_id` lets a caller
// whose POST timed out check whether it landed before re-posting with the
// SAME id — one logical send is never two server messages.

/// New client message id: a 19-digit decimal string (the shape Teams
/// clients use). Idempotency key for one logical send.
pub fn new_client_message_id() -> String {
    let v = u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes());
    let n = 1_000_000_000_000_000_000u128 + v % 9_000_000_000_000_000_000u128;
    n.to_string()
}

/// Server receipt for one chat-service POST.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SentMessage {
    /// Posted message id, when the answer named it.
    pub id: Option<String>,
    /// The `clientmessageid` the post carried.
    pub client_message_id: String,
}

/// Server message id from a POST answer (pure): the `Location` header's
/// numeric last path segment wins, else `OriginalArrivalTime` from the
/// JSON body (chat-service ids are the arrival epoch ms). None when the
/// answer names neither.
pub fn sent_id_from_response(location: Option<&str>, body: &str) -> Option<String> {
    if let Some(loc) = location {
        let path = loc.split('?').next().unwrap_or(loc);
        let last = path.rsplit('/').next().unwrap_or("").trim();
        if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
            return Some(last.to_string());
        }
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = v.as_object()?;
    let t = obj
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("OriginalArrivalTime"))
        .map(|(_, v)| v)?;
    match t {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s)
            if !s.trim().is_empty() && s.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(s.clone())
        }
        _ => None,
    }
}

/// POST one body carrying `clientmessageid` and read the receipt.
async fn post_with_receipt(
    client: &TeamsClient,
    url: &str,
    mut body: serde_json::Value,
    client_message_id: &str,
) -> Result<SentMessage> {
    body["clientmessageid"] = serde_json::json!(client_message_id);
    let resp = client.chat_post(url, &body).await?;
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // A 2xx already means posted; an unreadable body only loses the id.
    let text = resp.text().await.unwrap_or_default();
    Ok(SentMessage {
        id: sent_id_from_response(location.as_deref(), &text),
        client_message_id: client_message_id.to_string(),
    })
}

/// Send with a caller-owned `clientmessageid` (retries reuse it).
pub async fn send_message_with_client_id(
    client: &TeamsClient,
    chat_id: &str,
    message: &str,
    client_message_id: &str,
) -> Result<SentMessage> {
    let url = format!(
        "{}/v1/users/ME/conversations/{}/messages",
        client.chat_service_url(),
        chat_id
    );
    post_with_receipt(client, &url, send_message_body(message), client_message_id).await
}

/// Send serialized editor HTML with a caller-owned idempotency key.
pub async fn send_rich_message_with_client_id(
    client: &TeamsClient,
    chat_id: &str,
    html: &str,
    client_message_id: &str,
) -> Result<SentMessage> {
    let url = format!(
        "{}/v1/users/ME/conversations/{}/messages",
        client.chat_service_url(),
        chat_id
    );
    post_with_receipt(
        client,
        &url,
        send_message_html_body(html),
        client_message_id,
    )
    .await
}

/// Quote reply with a caller-owned `clientmessageid`.
pub async fn reply_message_with_client_id(
    client: &TeamsClient,
    chat_id: &str,
    parent_id: &str,
    parent_sender: &str,
    parent_text: &str,
    text: &str,
    client_message_id: &str,
) -> Result<SentMessage> {
    let url = format!(
        "{}/v1/users/ME/conversations/{}/messages",
        client.chat_service_url(),
        chat_id
    );
    let body = serde_json::json!({
        "content": build_reply_html(parent_id, parent_sender, parent_text, text),
        "messagetype": "RichText/Html",
        "contenttype": "text"
    });
    post_with_receipt(client, &url, body, client_message_id).await
}

/// Quote reply with semantic HTML from a trusted native composer and a
/// caller-owned idempotency key.
pub async fn reply_rich_message_with_client_id(
    client: &TeamsClient,
    chat_id: &str,
    parent_id: &str,
    parent_sender: &str,
    parent_text: &str,
    body_html: &str,
    client_message_id: &str,
) -> Result<SentMessage> {
    let url = format!(
        "{}/v1/users/ME/conversations/{}/messages",
        client.chat_service_url(),
        chat_id
    );
    let body = serde_json::json!({
        "content": build_reply_html_with_body(parent_id, parent_sender, parent_text, body_html),
        "messagetype": "RichText/Html",
        "contenttype": "text"
    });
    post_with_receipt(client, &url, body, client_message_id).await
}

/// Channel thread reply with a caller-owned `clientmessageid`. Same
/// argument guards as [`thread_reply_with_client`].
pub async fn thread_reply_with_client_id(
    client: &TeamsClient,
    channel_id: &str,
    root_id: &str,
    text: &str,
    client_message_id: &str,
) -> Result<SentMessage> {
    if !is_channel_conversation_id(channel_id) {
        bail!("not a channel conversation id");
    }
    if root_id.trim().is_empty() || root_id.contains(';') || root_id.contains('/') {
        bail!("bad root message id");
    }
    if text.trim().is_empty() {
        bail!("empty text");
    }
    let url = thread_reply_url(&client.chat_service_url(), channel_id, root_id);
    post_with_receipt(client, &url, send_message_body(text), client_message_id).await
}

/// Channel thread reply with trusted semantic HTML from the native composer.
/// The thread root is carried by the conversation path; the body stays an
/// ordinary rich message and does not gain the quote block used in chats.
pub async fn thread_reply_rich_with_client_id(
    client: &TeamsClient,
    channel_id: &str,
    root_id: &str,
    body_html: &str,
    client_message_id: &str,
) -> Result<SentMessage> {
    if !is_channel_conversation_id(channel_id) {
        bail!("not a channel conversation id");
    }
    if root_id.trim().is_empty() || root_id.contains(';') || root_id.contains('/') {
        bail!("bad root message id");
    }
    if body_html.trim().is_empty() {
        bail!("empty message body");
    }
    let url = thread_reply_url(&client.chat_service_url(), channel_id, root_id);
    post_with_receipt(
        client,
        &url,
        send_message_html_body(body_html),
        client_message_id,
    )
    .await
}

/// The message carrying `client_message_id` in one page, if any (pure).
pub fn find_by_client_id<'a>(
    messages: &'a [MessageInfo],
    client_message_id: &str,
) -> Option<&'a MessageInfo> {
    let want = client_message_id.trim();
    if want.is_empty() {
        return None;
    }
    messages
        .iter()
        .find(|m| m.client_message_id.as_deref() == Some(want))
}

/// Verify one send: read the newest page of `chat_id` and return the
/// message carrying `client_message_id` (None = not posted, as far as
/// the newest page shows). Channel ids read the channel conversation,
/// whose newest page includes thread replies.
pub async fn find_message_by_client_id(
    client: &TeamsClient,
    chat_id: &str,
    client_message_id: &str,
) -> Result<Option<MessageInfo>> {
    let page = read_messages_page(client, chat_id, 50, None).await?;
    let want = client_message_id.trim();
    if want.is_empty() {
        return Ok(None);
    }
    Ok(page
        .messages
        .into_iter()
        .find(|m| m.client_message_id.as_deref() == Some(want)))
}

/// Max quoted chars carried in a reply `<quote>` block.
pub const REPLY_SNIPPET_MAX: usize = 140;

/// Collapse whitespace and truncate to a one-line quote snippet.
/// Over-long text is cut at a char boundary with a trailing `…`.
pub fn reply_snippet(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= REPLY_SNIPPET_MAX {
        return one_line;
    }
    let end = one_line
        .char_indices()
        .nth(REPLY_SNIPPET_MAX)
        .map(|(i, _)| i)
        .unwrap_or(one_line.len());
    format!("{}…", &one_line[..end])
}

/// Build reply HTML: a Skype-style `<quote author guid>` block carrying the
/// parent id, then the [`build_message_html`] body (fenced replies get
/// `<pre>`, prose keeps `<p>`). Official clients render the quote;
/// [`split_reply_quote`] recovers the parent id on read.
pub fn build_reply_html(
    parent_id: &str,
    parent_sender: &str,
    parent_text: &str,
    text: &str,
) -> String {
    build_reply_html_with_body(
        parent_id,
        parent_sender,
        parent_text,
        &build_message_html(text),
    )
}

/// Build a quote reply around semantic HTML from a native composer. Quote
/// metadata stays escaped here; the caller owns `body_html`.
pub fn build_reply_html_with_body(
    parent_id: &str,
    parent_sender: &str,
    parent_text: &str,
    body_html: &str,
) -> String {
    format!(
        "<quote author=\"{}\" guid=\"{}\">{}</quote>{}",
        html_escape(parent_sender),
        html_escape(parent_id),
        html_escape(&reply_snippet(parent_text)),
        body_html,
    )
}

/// Split the first `<quote … guid="…">…</quote>` block off raw content.
/// Returns the parent id plus the remaining HTML. Missing or malformed
/// quotes yield `(None, content)` unchanged.
pub fn split_reply_quote(content: &str) -> (Option<String>, String) {
    let Some(open) = content.find("<quote") else {
        return (None, content.to_string());
    };
    let rest = &content[open..];
    let Some(tag_end) = rest.find('>') else {
        return (None, content.to_string());
    };
    let tag = &rest[..tag_end];
    let id = parse_guid(tag).filter(|s| !s.is_empty());
    let after_tag = &rest[tag_end + 1..];
    let Some(close) = after_tag.find("</quote>") else {
        return (None, content.to_string());
    };
    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..open]);
    out.push_str(&after_tag[close + "</quote>".len()..]);
    (id, out)
}

/// `guid="…"` (double or single quotes) from a `<quote …>` open tag.
fn parse_guid(tag: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let mark = format!("guid={}", quote);
        if let Some(start) = tag.find(&mark) {
            let val_start = start + mark.len();
            if let Some(end) = tag[val_start..].find(quote) {
                return Some(tag[val_start..val_start + end].to_string());
            }
        }
    }
    None
}

/// Channel thread parent from wire fields (om-lt2-quotelink).
/// Top-level wins, then `properties.*`, then content-embedded forms.
/// Missing/odd shapes → None, never fatal.
/// OstMac §106: wire `clientmessageid` (case-insensitive, string or
/// number), trimmed; None when absent/blank.
fn client_message_id_of(extra: &HashMap<String, serde_json::Value>) -> Option<String> {
    let v = extra
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("clientmessageid"))
        .map(|(_, v)| v)?;
    let s = match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if s.is_empty() { None } else { Some(s) }
}

fn message_parent_id(msg: &NativeMessage) -> Option<String> {
    if let Some(s) = wire_parent_from_map(&msg.extra) {
        return Some(s);
    }
    if let Some(props) = msg.properties.as_ref() {
        if let Some(s) = wire_parent_from_map(&props.extra) {
            return Some(s);
        }
    }
    if let Some(content) = msg.content.as_deref() {
        if let Some(s) = parent_id_from_content(content) {
            return Some(s);
        }
    }
    None
}

/// One wire value as a parent id: trimmed non-empty strings pass,
/// numbers stringify, everything else drops.
fn wire_parent_value(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Case-insensitive parent-key lookup over one flattened map.
/// Graph sends `replyToId`; native channel cards send `rootMessageId`
/// (H0 live probe, om-channel-history). Both mean "replies to <id>".
fn wire_parent_from_map(map: &HashMap<String, serde_json::Value>) -> Option<String> {
    for (k, v) in map {
        let lk = k.to_lowercase();
        if lk == "rootmessageid" || lk == "replytoid" || lk == "parentmessageid" || lk == "parentid"
        {
            if let Some(s) = wire_parent_value(v) {
                return Some(s);
            }
        }
    }
    None
}

/// Scan raw content for embedded `rootMessageId` / `replyToId` forms:
/// `"key":"val"`, `key="val"`, `key:123` (any quote/sep mix).
/// Case-insensitive key, first non-empty wins. Byte-wise so Unicode
/// text never misaligns indices; unterminated values drop.
fn parent_id_from_content(html: &str) -> Option<String> {
    const KEYS: &[&[u8]] = &[b"rootmessageid", b"replytoid"];
    let bytes = html.as_bytes();
    for key in KEYS {
        let mut i = 0;
        while i + key.len() <= bytes.len() {
            if bytes[i..i + key.len()].eq_ignore_ascii_case(key) {
                let boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                if boundary {
                    if let Some(v) = parent_value_after(bytes, i + key.len()) {
                        return Some(v);
                    }
                }
                i += key.len();
            } else {
                i += 1;
            }
        }
    }
    None
}

/// Value after a matched parent key: skips an optional closing quote,
/// requires `:` or `=`, then reads a quoted or bare token. None when
/// the key is not a key/value pair or the value is empty/unterminated.
fn parent_value_after(bytes: &[u8], mut j: usize) -> Option<String> {
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
        j += 1;
    }
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() || (bytes[j] != b':' && bytes[j] != b'=') {
        return None;
    }
    j += 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    let val: String;
    if bytes[j] == b'"' || bytes[j] == b'\'' {
        let q = bytes[j];
        j += 1;
        let start = j;
        while j < bytes.len() && bytes[j] != q {
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        val = String::from_utf8_lossy(&bytes[start..j]).trim().to_string();
    } else {
        let start = j;
        while j < bytes.len()
            && !matches!(
                bytes[j],
                b'"' | b'\''
                    | b','
                    | b';'
                    | b'<'
                    | b'>'
                    | b'}'
                    | b']'
                    | b')'
                    | b' '
                    | b'\t'
                    | b'\n'
                    | b'\r'
            )
        {
            j += 1;
        }
        val = String::from_utf8_lossy(&bytes[start..j]).trim().to_string();
    }
    if val.is_empty() { None } else { Some(val) }
}

/// Reply using an existing client (shared helper). The parent attribution
/// comes from the caller (no extra history fetch); the quote block keeps
/// the thread link readable in every client.
pub async fn reply_message_with_client(
    client: &TeamsClient,
    chat_id: &str,
    parent_id: &str,
    parent_sender: &str,
    parent_text: &str,
    text: &str,
) -> Result<()> {
    let base = client.chat_service_url();
    let url = format!("{}/v1/users/ME/conversations/{}/messages", base, chat_id);

    let body = serde_json::json!({
        "content": build_reply_html(parent_id, parent_sender, parent_text, text),
        "messagetype": "RichText/Html",
        "contenttype": "text"
    });

    tracing::debug!("Sending reply to {}", url);
    client.chat_post(&url, &body).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Channel thread replies (OstMac core-a)
// ---------------------------------------------------------------------------
//
// Teams channels are reply chains: a reply is a message posted to the
// thread conversation `<channelId>;messageid=<rootId>` (the same link
// shape the server stamps on channel replies' `conversationLink`), not
// a new top-level post carrying a quote block. Chats (1:1, group,
// meeting) have no chains — they keep the quote-reply path.

/// True for channel conversation ids (`19:…@thread.tacv2`, legacy
/// `19:…@thread.skype`). Group chats (`@thread.v2`), meetings and
/// 1:1s are not channels. Pure so tests pin it.
pub fn is_channel_conversation_id(id: &str) -> bool {
    let t = id.trim();
    t.starts_with("19:") && (t.ends_with("@thread.tacv2") || t.ends_with("@thread.skype"))
}

/// Thread conversation id for a channel reply chain. Pure.
pub fn thread_reply_conversation(channel_id: &str, root_id: &str) -> String {
    format!("{};messageid={}", channel_id.trim(), root_id.trim())
}

/// POST URL for one channel thread reply. Pure so tests pin it.
pub fn thread_reply_url(base: &str, channel_id: &str, root_id: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/messages",
        base,
        thread_reply_conversation(channel_id, root_id)
    )
}

/// Post one reply into a channel thread (reply chain under `root_id`).
/// Body is the plain send body (no quote block — the chain is the
/// link). Non-channel ids and blank args are rejected before network.
pub async fn thread_reply_with_client(
    client: &TeamsClient,
    channel_id: &str,
    root_id: &str,
    text: &str,
) -> Result<()> {
    if !is_channel_conversation_id(channel_id) {
        bail!("not a channel conversation id");
    }
    if root_id.trim().is_empty() || root_id.contains(';') || root_id.contains('/') {
        bail!("bad root message id");
    }
    if text.trim().is_empty() {
        bail!("empty text");
    }
    let url = thread_reply_url(&client.chat_service_url(), channel_id, root_id);
    tracing::debug!("Sending thread reply");
    client.chat_post(&url, &send_message_body(text)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reactions (OstMac om-reactions lane)
// ---------------------------------------------------------------------------
//
// Wire shape mirrors the Graph chatMessageReaction resource
// (`POST .../messages/{id}/reactions`, `{"reactionType": "like"}`) against
// the native chat service with skypetoken auth. Best-effort: NOT yet
// verified live against the server (see OSTMAC-PATCHES.md §17).

/// Picker emoji → Teams reaction type, in picker order.
/// (like, heart, laugh, surprised, sad, angry — the Graph-supported six.)
pub const REACTION_EMOJI: &[(&str, &str)] = &[
    ("👍", "like"),
    ("❤️", "heart"),
    ("😂", "laugh"),
    ("😮", "surprised"),
    ("😢", "sad"),
    ("😠", "angry"),
];

/// Reaction type for a picker emoji, or None when unsupported.
pub fn reaction_type_for_emoji(emoji: &str) -> Option<&'static str> {
    REACTION_EMOJI
        .iter()
        .find(|(e, _)| *e == emoji)
        .map(|(_, t)| *t)
}

/// Picker emoji for a server reaction type (case-insensitive), or None
/// when unknown. Unknown types are dropped from counts, never fatal.
pub fn emoji_for_reaction_type(reaction_type: &str) -> Option<&'static str> {
    REACTION_EMOJI
        .iter()
        .find(|(_, t)| t.eq_ignore_ascii_case(reaction_type))
        .map(|(e, _)| *e)
}

/// POST target for adding a reaction to one message.
pub fn reaction_add_url(base: &str, chat_id: &str, message_id: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/messages/{}/reactions",
        base, chat_id, message_id
    )
}

/// Per-message URL for edits and deletes (pure so embedders/tests pin it).
pub fn message_url(base: &str, chat_id: &str, message_id: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/messages/{}",
        base, chat_id, message_id
    )
}

/// JS `encodeURIComponent` (unreserved plus `!'()*` kept, rest %XX).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'!'
            | b'\''
            | b'('
            | b')'
            | b'*' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// DELETE target for deleting one own message, exactly as the Teams web
/// client sends it: `.../conversations/{encodeURIComponent(conv)}/messages/{id}?behavior=softDelete`.
/// A bare DELETE (no `behavior`) is not what the web client does.
pub fn message_delete_url(base: &str, chat_id: &str, message_id: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/messages/{}?behavior=softDelete",
        base,
        encode_uri_component(chat_id),
        message_id
    )
}

/// POST body for adding a reaction.
pub fn reaction_add_body(reaction_type: &str) -> serde_json::Value {
    serde_json::json!({ "reactionType": reaction_type })
}

/// DELETE target for removing one reaction type from a message.
pub fn reaction_remove_url(
    base: &str,
    chat_id: &str,
    message_id: &str,
    reaction_type: &str,
) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/messages/{}/reactions/{}",
        base, chat_id, message_id, reaction_type
    )
}

/// Add one emoji reaction to a message. Unknown emoji is rejected before
/// any network.
pub async fn send_reaction_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
    emoji: &str,
) -> Result<()> {
    let reaction_type = reaction_type_for_emoji(emoji)
        .with_context(|| format!("unsupported reaction emoji: {}", emoji))?;
    let base = client.chat_service_url();
    let url = reaction_add_url(&base, chat_id, message_id);
    let body = reaction_add_body(reaction_type);
    tracing::debug!("Adding {} reaction to {}", reaction_type, url);
    client.chat_post(&url, &body).await?;
    Ok(())
}

/// Remove one emoji reaction from a message. Unknown emoji is rejected
/// before any network.
pub async fn remove_reaction_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
    emoji: &str,
) -> Result<()> {
    let reaction_type = reaction_type_for_emoji(emoji)
        .with_context(|| format!("unsupported reaction emoji: {}", emoji))?;
    let base = client.chat_service_url();
    let url = reaction_remove_url(&base, chat_id, message_id, reaction_type);
    tracing::debug!("Removing {} reaction from {}", reaction_type, url);
    client.chat_delete(&url, None).await?;
    Ok(())
}

/// Add or remove a reaction (prints to stdout). CLI entry point.
pub async fn react(chat_id: &str, message_id: &str, emoji: &str, remove: bool) -> Result<()> {
    let client = TeamsClient::new().await?;
    if remove {
        remove_reaction_with_client(&client, chat_id, message_id, emoji).await?;
        println!("Reaction removed.");
    } else {
        send_reaction_with_client(&client, chat_id, message_id, emoji).await?;
        println!("Reaction added.");
    }
    Ok(())
}

/// Edit body for the native chat API. `skypeeditedid` carries the original
/// id so receivers (and our realtime parser) classify it as an edit.
pub fn edit_message_body(message_id: &str, text: &str) -> serde_json::Value {
    let mut body = send_message_body(text);
    body["skypeeditedid"] = serde_json::json!(message_id);
    body
}

/// Edit one own message's text via PUT (prints to stdout).
pub async fn edit_message(chat_id: &str, message_id: &str, text: &str) -> Result<()> {
    let client = TeamsClient::new().await?;
    edit_message_with_client(&client, chat_id, message_id, text).await?;
    println!("Message edited.");
    Ok(())
}

/// Edit one own message using an existing client (shared helper).
pub async fn edit_message_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
    text: &str,
) -> Result<()> {
    let base = client.chat_service_url();
    let url = message_url(&base, chat_id, message_id);
    let body = edit_message_body(message_id, text);
    tracing::debug!("Editing message at {}", url);
    client.chat_put(&url, &body).await?;
    Ok(())
}

/// Delete one own message via DELETE (prints to stdout).
pub async fn delete_message(chat_id: &str, message_id: &str) -> Result<()> {
    let client = TeamsClient::new().await?;
    delete_message_with_client(&client, chat_id, message_id).await?;
    println!("Message deleted.");
    Ok(())
}

/// Delete one own message using an existing client (shared helper).
pub async fn delete_message_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
) -> Result<()> {
    let base = client.chat_service_url();
    let url = message_delete_url(&base, chat_id, message_id);
    tracing::debug!("Deleting message at {}", url);
    client.chat_delete(&url, None).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Leave chat (OstMac om-leave-block lane)
// ---------------------------------------------------------------------------
//
// Self-removal from a thread's roster: DELETE .../v1/threads/{id}/members/{mri}
// with skypetoken auth, where the member MRI is the signed-in user's own
// (`8:orgid:{oid}` from whoami). Best-effort: NOT yet verified live against
// the server (see OSTMAC-PATCHES.md §27). Targets group threads; 1:1
// threads are hidden client-side instead (the block flow).

/// Own roster MRI for an Entra object id (`8:orgid:{oid}`).
pub fn own_member_mri(oid: &str) -> String {
    format!("8:orgid:{}", oid.trim())
}

/// DELETE target for removing one member from a thread's roster.
pub fn leave_member_url(base: &str, chat_id: &str, member_mri: &str) -> String {
    format!("{}/v1/threads/{}/members/{}", base, chat_id, member_mri)
}

/// Leave one chat: remove self from the thread roster. Empty ids are
/// rejected before any network; the own MRI resolves via whoami.
pub async fn leave_chat_with_client(client: &TeamsClient, chat_id: &str) -> Result<()> {
    if chat_id.trim().is_empty() {
        anyhow::bail!("empty chat_id");
    }
    let me = whoami_data(client).await?;
    if me.id.trim().is_empty() {
        anyhow::bail!("empty owner id");
    }
    let base = client.chat_service_url();
    let url = leave_member_url(&base, chat_id.trim(), &own_member_mri(&me.id));
    tracing::debug!("Leaving chat at {}", url);
    client.chat_delete(&url, None).await?;
    Ok(())
}

/// Leave one chat thread (prints to stdout).
pub async fn leave_chat(chat_id: &str) -> Result<()> {
    let client = TeamsClient::new().await?;
    leave_chat_with_client(&client, chat_id).await?;
    println!("Left chat.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Read receipts (OstMac om-receipts lane)
// ---------------------------------------------------------------------------
//
// Wire shape mirrors the native chat service consumption horizon:
// PUT .../v1/users/ME/conversations/{id}/properties?name=consumptionhorizon
// with {"consumptionhorizon": "<t1>;<t2>;<messageId>"} marks read;
// GET .../v1/threads/{id}/consumptionhorizons lists peer positions.

/// One peer read position: user key + last-read message id.
fn message_application_id(message: &NativeMessage) -> Option<String> {
    let value = message.properties.as_ref()?.extra.get("fromAppMetadata")?;
    let parsed;
    let value = if let Some(text) = value.as_str() {
        if text.len() > 4096 { return None; }
        parsed = serde_json::from_str::<Value>(text).ok()?;
        &parsed
    } else { value };
    value.get("id").and_then(Value::as_str).filter(|id| id.len() <= 128)
        .map(str::to_owned)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReceipt {
    pub user: String,
    pub message_id: String,
    pub horizon: String,
}

/// PUT target for marking one conversation read up to a message.
pub fn consumptionhorizon_url(base: &str, chat_id: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/properties?name=consumptionhorizon",
        base, chat_id
    )
}

/// GET target for listing peer read positions in one thread.
pub fn consumptionhorizons_url(base: &str, thread_id: &str) -> String {
    format!("{}/v1/threads/{}/consumptionhorizons", base, thread_id)
}

/// Horizon value: "<now_ms>;<now_ms>;<message_id>". Both stamps are the
/// send time (server accepts equal stamps; the id is the read frontier).
pub fn consumptionhorizon_value(message_id: &str, now_ms: u64) -> String {
    format!("{};{};{}", now_ms, now_ms, message_id)
}

/// PUT body for marking read.
pub fn consumptionhorizon_body(message_id: &str, now_ms: u64) -> serde_json::Value {
    serde_json::json!({ "consumptionhorizon": consumptionhorizon_value(message_id, now_ms) })
}

/// Message id from a horizon value: text after the last ';'.
/// Empty/blank horizons yield None (caller drops the entry).
pub fn receipt_message_id(horizon: &str) -> Option<String> {
    let id = horizon.rsplit(';').next()?.trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// User key from one consumptionhorizon entry: `mri` → `id` → `user`
/// → display name, else "". Never fails (unknown shapes stay parseable).
fn receipt_user(entry: &serde_json::Value) -> String {
    for k in ["mri", "id", "user", "imdisplayname", "displayName"] {
        if let Some(s) = entry.get(k).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Parse the GET consumptionhorizons envelope into receipts. Tolerant:
/// missing/empty lists yield vec![], entries without a parseable horizon
/// are dropped, bare-string entries use "" as the user key.
pub fn parse_consumptionhorizons(value: &serde_json::Value) -> Vec<ReadReceipt> {
    let list = value.get("consumptionhorizons").and_then(|v| v.as_array());
    let Some(list) = list else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in list {
        if let Some(s) = e.as_str() {
            let horizon = s.trim().to_string();
            if let Some(mid) = receipt_message_id(&horizon) {
                out.push(ReadReceipt {
                    user: String::new(),
                    message_id: mid,
                    horizon,
                });
            }
            continue;
        }
        let horizon = e
            .get("consumptionhorizon")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let Some(mid) = receipt_message_id(&horizon) else {
            continue;
        };
        out.push(ReadReceipt {
            user: receipt_user(e),
            message_id: mid,
            horizon,
        });
    }
    out
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mark one conversation read up to `message_id` via PUT. Empty ids are
/// rejected before any network.
pub async fn mark_read_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
) -> Result<()> {
    if chat_id.trim().is_empty() {
        anyhow::bail!("empty chat_id");
    }
    if message_id.trim().is_empty() {
        anyhow::bail!("empty message_id");
    }
    let base = client.chat_service_url();
    let url = consumptionhorizon_url(&base, chat_id.trim());
    let body = consumptionhorizon_body(message_id.trim(), now_millis());
    tracing::debug!("Marking read at {}", url);
    client.chat_put(&url, &body).await?;
    Ok(())
}

/// Peer read positions for one thread (read-only). Unknown users/shapes
/// parse tolerantly via [`parse_consumptionhorizons`].
pub async fn read_receipts_data(client: &TeamsClient, thread_id: &str) -> Result<Vec<ReadReceipt>> {
    if thread_id.trim().is_empty() {
        anyhow::bail!("empty thread_id");
    }
    let base = client.chat_service_url();
    let url = consumptionhorizons_url(&base, thread_id.trim());
    let resp = client.chat_get(&url).await?;
    let value: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse consumptionhorizons response")?;
    Ok(parse_consumptionhorizons(&value))
}

// ---------------------------------------------------------------------------
// Data-returning API functions for TUI integration
// ---------------------------------------------------------------------------

/// Chat metadata for TUI display.
#[allow(dead_code)]
pub struct ChatInfo {
    pub id: String,
    pub name: String,
    /// The name was attributed to the peer's MRI, not a topic/last-sender fallback.
    pub peer_name_verified: bool,
    pub is_group: bool,
    /// Shared thread picture, independent from per-user notification properties.
    pub picture: Option<String>,
    pub is_meeting: bool,
    /// Per-user notification state when the service returned `properties.alerts`.
    pub muted: Option<bool>,
    /// Hidden state when the service returned the per-user `unpinnedTime` property.
    pub hidden: Option<bool>,
    /// Resolver-visible unread marker; None when the conversation response omits it.
    pub unread: Option<bool>,
    pub last_message_time: Option<String>,
    pub last_message_sender: Option<String>,
    pub last_message_sender_mri: Option<String>,
    pub last_message_preview: Option<String>,
}

/// Native chat importance. `normal` stays unmarked; `high` and `urgent`
/// are the two flags the official client paints on the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageImportance {
    #[default]
    Normal,
    High,
    Urgent,
}

/// A single message for TUI display.
pub struct MessageInfo {
    /// Server message id; embedders match realtime edits by this.
    /// OstMac: synthetic `timestamp@sender` fallback when the server omits it.
    pub id: String,
    /// Sender MRI parsed from the `from` user link
    /// (`…/v1/users/ME/contacts/8:orgid:<guid>` → `8:orgid:<guid>`);
    /// "" when the server omits `from`. Om-chatnames: 1:1 mate
    /// attribution matches this, never display-name spelling.
    pub sender_mri: String,
    /// Application identity from native properties.fromAppMetadata.
    pub application_id: Option<String>,
    pub sender: String,
    pub timestamp: String,
    pub content: String,
    /// Unstripped server HTML (om-convrich: embedders mine `<at>` mentions
    /// and `<pre>` code blocks from it; `content` stays the stripped text).
    pub raw: String,
    /// Presence bits for fixed card fields, only for compatibility placeholders.
    pub card_wire_shape: Option<u8>,
    /// Grouped reaction counts (om-reactions). Empty when the server sent
    /// none; unknown reaction types are dropped, never fatal.
    pub reactions: Vec<ReactionCount>,
    /// Uninterpreted reaction properties for a host-owned extended catalog.
    pub reaction_properties: Option<Value>,
    /// Typed mention item IDs and MRIs, preserved for the host renderer.
    pub mention_properties: Option<Value>,
    /// Parent message id for quote replies (om-replies: mined from the
    /// `<quote guid>` block; `content` excludes the quoted text).
    pub reply_to: Option<String>,
    /// OstMac §106: the `clientmessageid` the sender posted with (the
    /// idempotency key; an own send's pending bubble reconciles by it).
    pub client_message_id: Option<String>,
    /// Original-message details attached to a forwarded message. This is
    /// optional because most messages do not carry forwarding context.
    pub forwarded: Option<ForwardedMessageContext>,
    /// Files attached through the chat service's `properties.files` field.
    pub files: Vec<ChatFileRef>,
    /// `properties.importance` or the top-level field. Unknown values stay normal.
    pub importance: MessageImportance,
    /// The service kept an edit marker (`skypeeditedid` or `edittime`).
    pub edited: bool,
}

/// Bounded metadata projected from the original-message context on forwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForwardedMessageContext {
    pub message_id: Option<String>,
    pub sender_mri: Option<String>,
    pub arrival_time: Option<String>,
    pub content: Option<String>,
    pub message_type: Option<String>,
    pub channel_subject: Option<String>,
    pub announcement: Option<bool>,
    pub deleted: Option<bool>,
}

/// One grouped reaction count: picker emoji + number of reactors.
/// `reactors` (core-a) lists who reacted when the wire names them;
/// it may be shorter than `count` (entries without an id still count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionCount {
    pub emoji: String,
    pub count: usize,
    pub reactors: Vec<Reactor>,
}

/// One reactor (core-a who-reacted). `id` is whatever the wire names
/// the user by: an MRI (`8:orgid:<guid>`, native `emotions`) or a
/// Graph user id (Graph-like `reactions[].user.user.id`). `name` is
/// the display name when the wire carries one, else "" (the embedder
/// resolves MRIs through the chat roster).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reactor {
    pub id: String,
    pub name: String,
}

/// Picker index for a wire reaction type (case-insensitive).
fn reaction_index(t: &str) -> Option<usize> {
    let key = t.trim();
    let common_key = key
        .rsplit_once("-tone")
        .filter(|(base, tone)| {
            !base.is_empty()
                && base.len() <= 40
                && tone.len() == 1
                && tone.as_bytes()[0].is_ascii_digit()
        })
        .map_or(key, |(base, _)| base);
    REACTION_EMOJI
        .iter()
        .position(|(_, known)| known.eq_ignore_ascii_case(common_key))
}

/// Case-insensitive object field lookup.
fn obj_get_ci<'a>(
    o: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    o.get(key).or_else(|| {
        o.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

fn obj_str_ci(o: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj_get_ci(o, key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn message_property<'a>(message: &'a NativeMessage, key: &str) -> Option<&'a Value> {
    message
        .properties
        .as_ref()
        .and_then(|properties| map_value_ci(&properties.extra, key))
        .or_else(|| map_value_ci(&message.extra, key))
}

fn message_importance(message: &NativeMessage) -> MessageImportance {
    let Some(Value::String(text)) = message_property(message, "importance") else {
        return MessageImportance::Normal;
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "high" => MessageImportance::High,
        "urgent" => MessageImportance::Urgent,
        _ => MessageImportance::Normal,
    }
}

fn message_was_edited(message: &NativeMessage) -> bool {
    ["skypeeditedid", "edittime"].into_iter().any(|key| match message_property(message, key) {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Number(_)) | Some(Value::Bool(true)) => true,
        _ => false,
    })
}

fn map_value_ci<'a>(map: &'a HashMap<String, Value>, key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .map(|(_, value)| value)
}

fn bounded_context_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    max_chars: usize,
) -> Option<String> {
    obj_str_ci(object, key).map(|value| value.chars().take(max_chars).collect())
}

fn context_bool(object: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    match obj_get_ci(object, key)? {
        Value::Bool(value) => Some(*value),
        Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn forwarded_message_context(message: &NativeMessage) -> Option<ForwardedMessageContext> {
    let raw = message
        .properties
        .as_ref()
        .and_then(|properties| map_value_ci(&properties.extra, "originalMessageContext"))
        .or_else(|| map_value_ci(&message.extra, "originalMessageContext"))?;
    let value = match raw {
        Value::String(encoded) => serde_json::from_str::<Value>(encoded).ok()?,
        value => value.clone(),
    };
    let object = value.as_object()?;
    let context = ForwardedMessageContext {
        message_id: bounded_context_string(object, "originalMessageId", 256),
        sender_mri: bounded_context_string(object, "originalSenderMri", 256),
        arrival_time: bounded_context_string(object, "originalArrivalTime", 128),
        content: bounded_context_string(object, "originalMessageContent", 16_384),
        message_type: bounded_context_string(object, "originalMessageType", 128),
        channel_subject: bounded_context_string(object, "originalChannelPostSubject", 512),
        announcement: context_bool(object, "originalAnnouncement"),
        deleted: context_bool(object, "originalMessageIsDeleted"),
    };
    (context.message_id.is_some()
        || context.sender_mri.is_some()
        || context.content.is_some()
        || context.channel_subject.is_some()
        || context.announcement.is_some())
    .then_some(context)
}

/// Reactor from one Graph-like entry: `user.user.{id,displayName}`
/// (chatMessageReaction) or a flat `user.{id,displayName}`.
fn graph_reactor(entry: &serde_json::Map<String, serde_json::Value>) -> Option<Reactor> {
    let user = obj_get_ci(entry, "user")?.as_object()?;
    let identity = obj_get_ci(user, "user")
        .and_then(|v| v.as_object())
        .unwrap_or(user);
    let id = obj_str_ci(identity, "id")?;
    let name = obj_str_ci(identity, "displayName").unwrap_or_default();
    Some(Reactor { id, name })
}

/// JSON arrays may arrive stringified inside `properties` (native
/// chat service habit); borrow normal arrays and decode strings once.
fn as_array_lenient(v: &Value) -> Option<Cow<'_, [Value]>> {
    match v {
        Value::Array(a) => Some(Cow::Borrowed(a)),
        Value::String(s) => serde_json::from_str::<Vec<Value>>(s).ok().map(Cow::Owned),
        _ => None,
    }
}

/// Grouped reactions from raw wire values (core-a, shared with the
/// realtime parser). `reactions` is the Graph-like list
/// (`[{reactionType, user?}]`, one entry per reactor); `emotions` is
/// the native chat-service form (`[{key, users:[{mri}]}]`, array or
/// stringified). The Graph-like list wins when present. Canonical
/// picker order; unknown types drop; never fatal.
pub fn reaction_counts_from_values(
    reactions: Option<&serde_json::Value>,
    emotions: Option<&serde_json::Value>,
) -> Vec<ReactionCount> {
    reaction_counts_from_values_with_summary(reactions, emotions, None)
}

/// Grouped reactions from raw wire values, including the native
/// `emotionsSummary` count projection. When present, its explicit counts take
/// precedence over the number of user identities carried in `emotions`.
pub fn reaction_counts_from_values_with_summary(
    reactions: Option<&Value>,
    emotions: Option<&Value>,
    emotions_summary: Option<&Value>,
) -> Vec<ReactionCount> {
    let mut counts = vec![0usize; REACTION_EMOJI.len()];
    let mut reactors: Vec<Vec<Reactor>> = vec![Vec::new(); REACTION_EMOJI.len()];
    let mut push = |i: usize, increment: usize, r: Option<Reactor>| {
        counts[i] = counts[i].saturating_add(increment);
        if let Some(r) = r {
            if r.id.is_empty()
                || !reactors[i]
                    .iter()
                    .any(|x| !x.id.is_empty() && x.id.eq_ignore_ascii_case(&r.id))
            {
                reactors[i].push(r);
            }
        }
    };
    if let Some(list) = reactions.and_then(as_array_lenient) {
        for e in list.iter() {
            let Some(o) = e.as_object() else { continue };
            let Some(t) = obj_str_ci(o, "reactionType") else {
                continue;
            };
            if let Some(i) = reaction_index(&t) {
                push(i, 1, graph_reactor(o));
            }
        }
    } else if let Some(summary) = emotions_summary.and_then(as_array_lenient) {
        let emotion_list = emotions.and_then(as_array_lenient);
        for entry in summary.iter() {
            let Some(object) = entry.as_object() else {
                continue;
            };
            let Some(key) = obj_str_ci(object, "key") else {
                continue;
            };
            let Some(index) = reaction_index(&key) else {
                continue;
            };
            let users = emotion_list
                .as_deref()
                .and_then(|items| {
                    items.iter().find(|item| {
                        item.as_object()
                            .and_then(|emotion| obj_str_ci(emotion, "key"))
                            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&key))
                    })
                })
                .and_then(Value::as_object)
                .and_then(|emotion| obj_get_ci(emotion, "users"))
                .and_then(as_array_lenient);
            let count = obj_get_ci(object, "count")
                .and_then(Value::as_u64)
                .and_then(|count| usize::try_from(count).ok())
                .or_else(|| users.as_ref().map(|items| items.len()))
                .unwrap_or_default();
            if count == 0 {
                continue;
            }
            push(index, count, None);
            for user in users.iter().flat_map(|items| items.iter()) {
                let Some(user_object) = user.as_object() else {
                    continue;
                };
                let id = obj_str_ci(user_object, "mri")
                    .or_else(|| obj_str_ci(user_object, "id"));
                let name = obj_str_ci(user_object, "displayName").unwrap_or_default();
                push(
                    index,
                    0,
                    Some(Reactor {
                        id: id.unwrap_or_default(),
                        name,
                    }),
                );
            }
        }
    } else if let Some(list) = emotions.and_then(as_array_lenient) {
        for e in list.iter() {
            let Some(o) = e.as_object() else { continue };
            let Some(t) = obj_str_ci(o, "key") else {
                continue;
            };
            let Some(i) = reaction_index(&t) else {
                continue;
            };
            let users = obj_get_ci(o, "users")
                .and_then(as_array_lenient)
                .unwrap_or_default();
            for u in users.iter() {
                let Some(uo) = u.as_object() else { continue };
                let id = obj_str_ci(uo, "mri").or_else(|| obj_str_ci(uo, "id"));
                let name = obj_str_ci(uo, "displayName").unwrap_or_default();
                push(i, 1, id.map(|id| Reactor { id, name }));
            }
        }
    }
    REACTION_EMOJI
        .iter()
        .zip(counts.into_iter().zip(reactors))
        .filter(|(_, (c, _))| *c > 0)
        .map(|((emoji, _), (count, reactors))| ReactionCount {
            emoji: emoji.to_string(),
            count,
            reactors,
        })
        .collect()
}

/// Reaction entries for one message: top-level `reactions` wins, then
/// `properties.reactions`, then native `properties.emotions`. None
/// present → empty.
fn message_reaction_values(msg: &NativeMessage) -> (Option<Value>, Option<Value>, Option<Value>) {
    let props = msg.properties.as_ref();
    let graph = msg
        .reactions
        .as_ref()
        .filter(|v| !reaction_list_is_empty(v))
        .or_else(|| {
            props
                .and_then(|p| p.reactions.as_ref())
                .filter(|v| !reaction_list_is_empty(v))
        })
        .cloned();
    let emotions = props
        .and_then(|p| {
            p.extra
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("emotions"))
                .map(|(_, v)| v)
        })
        .or_else(|| {
            props.and_then(|p| {
                p.extra
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("deltaEmotions"))
                    .map(|(_, v)| v)
            })
        })
        .cloned();
    let emotions_summary = annotation_emotion_summary(msg).or_else(|| {
        props
            .and_then(|p| map_value_ci(&p.extra, "emotionsSummary"))
            .filter(|value| !reaction_list_is_empty(value))
            .cloned()
    });
    (graph, emotions, emotions_summary)
}

fn reaction_list_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::String(encoded) => serde_json::from_str::<Vec<Value>>(encoded)
            .ok()
            .is_some_and(|items| items.is_empty()),
        _ => false,
    }
}

/// Count-only projection some pages send beside the property bag:
/// `annotationsSummary.emotions` is a map of key to count.
fn annotation_emotion_summary(msg: &NativeMessage) -> Option<Value> {
    let raw = msg
        .extra
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("annotationsSummary"))
        .map(|(_, value)| value)?;
    let value = match raw {
        Value::String(encoded) => serde_json::from_str::<Value>(encoded).ok()?,
        other => other.clone(),
    };
    let emotions = value.get("emotions").or_else(|| {
        value
            .as_object()?
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("emotions"))
            .map(|(_, item)| item)
    })?;
    match emotions {
        Value::Array(items) if !items.is_empty() => Some(emotions.clone()),
        Value::String(encoded) => serde_json::from_str::<Value>(encoded).ok(),
        Value::Object(map) => {
            let items: Vec<Value> = map
                .iter()
                .filter_map(|(key, count)| {
                    count
                        .as_u64()
                        .map(|count| serde_json::json!({"key": key, "count": count}))
                })
                .take(128)
                .collect();
            (!items.is_empty()).then(|| Value::Array(items))
        }
        _ => None,
    }
}
fn message_reaction_properties(msg: &NativeMessage) -> Option<Value> {
    let (graph, emotions, emotions_summary) = message_reaction_values(msg);
    (graph.is_some() || emotions.is_some() || emotions_summary.is_some()).then(|| serde_json::json!({
        "reactions": graph, "emotions": emotions, "emotionsSummary": emotions_summary
    }))
}
fn message_reactions(msg: &NativeMessage) -> Vec<ReactionCount> {
    let (graph, emotions, emotions_summary) = message_reaction_values(msg);
    reaction_counts_from_values_with_summary(
        graph.as_ref(),
        emotions.as_ref(),
        emotions_summary.as_ref(),
    )
}

/// One page of history plus the cursor for the next older page.
pub struct MessagesPage {
    pub messages: Vec<MessageInfo>,
    /// Server `_metadata.backwardLink`: full URL of the next older page,
    /// or None when history is exhausted / the server omits metadata.
    pub backward_link: Option<String>,
}

/// Roster entry from `GET /v1/threads/{id}/members`: the member MRI
/// (`8:orgid:<guid>`) is `id`. Names are resolved from the identity profile.
#[derive(Debug, Deserialize)]
struct ThreadMember {
    id: Option<String>,
    /// `Admin` / `User` (core-a roster roles; absent on some shapes).
    role: Option<String>,
    /// Some tenants stamp a display name; usually absent.
    #[serde(rename = "friendlyName")]
    friendly_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThreadMembersResponse {
    members: Option<Vec<ThreadMember>>,
}

/// Member MRIs for one thread (read-only). Errors (404 on system
/// threads like `48:notes`, network) propagate to the caller and leave the
/// corresponding identity unresolved.
async fn thread_member_mris(client: &TeamsClient, thread_id: &str) -> Result<Vec<String>> {
    let base = client.chat_service_url();
    let url = format!("{}/v1/threads/{}/members", base, thread_id);
    let resp = client.chat_get(&url).await?;
    let body: ThreadMembersResponse = resp
        .json()
        .await
        .context("Failed to parse thread members response")?;
    Ok(body
        .members
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.id)
        .filter(|id| !id.trim().is_empty())
        .collect())
}

/// Decode `%XX` runs; malformed runs pass through untouched.
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| hex(*b));
            let lo = bytes.get(i + 2).and_then(|b| hex(*b));
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sender MRI from a message `from` user link: the last path segment,
/// percent-decoded (`…/ME/contacts/8:orgid:<guid>` → `8:orgid:<guid>`).
/// Missing/empty `from` → "".
fn mri_from_user_link(from: Option<&str>) -> String {
    let seg = from.unwrap_or("").rsplit('/').next().unwrap_or("").trim();
    if seg.is_empty() {
        return String::new();
    }
    percent_decode(seg)
}

/// Exact identity equality after removing the orgid representation prefix.
/// Empty values and suffix/prefix collisions never match.
fn mri_is_self(mri: &str, self_oid: &str) -> bool {
    if self_oid.trim().is_empty() || mri.trim().is_empty() {
        return false;
    }
    let canonical = |value: &str| {
        let value = value.trim().to_ascii_lowercase();
        value.strip_prefix("8:orgid:").unwrap_or(&value).to_owned()
    };
    canonical(mri) == canonical(self_oid)
}

/// 1:1-shaped thread ids (`19:…@unq.…`): group (`@thread.v2`) and
/// meeting ids are excluded, `48:…` system ids never qualify.
fn is_onetoone_id(chat_id: &str) -> bool {
    chat_id.starts_with("19:") && !chat_id.contains("@thread") && !chat_id.contains("meeting")
}

/// Resolve the peer identity from the thread ID or its roster, then read the
/// identity's profile. Message authors are not needed to name a conversation.
async fn resolve_mate_name(client: &TeamsClient, chat_id: &str, self_oid: &str) -> Option<String> {
    if let Some(peer) = direct_peer_id(chat_id, self_oid) {
        return peer_profile_name(client, &peer).await;
    }
    let members = thread_member_mris(client, chat_id).await.ok()?;
    if !members.iter().any(|member| mri_is_self(member, self_oid)) {
        return None;
    }
    let mut mates: Vec<&str> = members
        .iter()
        .map(String::as_str)
        .filter(|m| !mri_is_self(m, self_oid))
        .collect();
    mates.sort_unstable_by_key(|mri| mri.to_ascii_lowercase());
    mates.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if mates.len() != 1 {
        return None;
    }
    let mate = oid_from_orgid_mri(mates[0])?;
    peer_profile_name(client, &mate).await
}

async fn peer_profile_name(client: &TeamsClient, peer: &str) -> Option<String> {
    let response = client.graph_get(&format!("/users/{peer}?$select=id,displayName")).await.ok()?;
    let user: Value = response.json().await.ok()?;
    let id = user.get("id")?.as_str()?;
    if !id.eq_ignore_ascii_case(peer) { return None; }
    user.get("displayName")?.as_str().map(str::trim)
        .filter(|name| !name.is_empty()).map(str::to_owned)
}

// ---------------------------------------------------------------------------
// FIXPACK F8: batched, cached 1:1 mate names for chat-list paging
// ---------------------------------------------------------------------------

/// Concurrent peer lookups per page; opaque thread IDs also require a roster read.
pub(crate) const MATE_CONCURRENCY: usize = 6;
/// A cached mate name is trusted this long (renames are rare; a stale
/// name heals on the next lookup after this).
const MATE_TTL_SECS: u64 = 14 * 24 * 3600;

/// Persisted chat id -> (mate display name, resolved-at epoch secs). One
/// file per profile beside its config. Best effort: an unreadable or
/// unwritable file just means the lookups run again.
#[derive(Debug, Default)]
pub(crate) struct MateNames {
    names: HashMap<String, (String, u64)>,
    path: Option<std::path::PathBuf>,
}

impl MateNames {
    pub(crate) fn load(path: Option<std::path::PathBuf>) -> Self {
        let mut names = HashMap::new();
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(obj) = v.as_object() {
                    for (k, e) in obj {
                        if let (Some(n), Some(t)) = (
                            e.get(0).and_then(|x| x.as_str()),
                            e.get(1).and_then(|x| x.as_u64()),
                        ) {
                            names.insert(k.clone(), (n.to_string(), t));
                        }
                    }
                }
            }
        }
        Self { names, path }
    }

    pub(crate) fn get(&self, chat_id: &str, now: u64) -> Option<String> {
        self.names
            .get(chat_id)
            .filter(|(n, t)| !n.trim().is_empty() && now.saturating_sub(*t) < MATE_TTL_SECS)
            .map(|(n, _)| n.clone())
    }

    pub(crate) fn put(&mut self, chat_id: &str, name: &str, now: u64) {
        self.names
            .insert(chat_id.to_string(), (name.to_string(), now));
    }

    /// Write the map (temp file + rename). Errors are logged, never raised.
    pub(crate) fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let obj: serde_json::Map<String, serde_json::Value> = self
            .names
            .iter()
            .map(|(k, (n, t))| (k.clone(), serde_json::json!([n, t])))
            .collect();
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&tmp, serde_json::Value::Object(obj).to_string())?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            tracing::debug!("mate name cache not saved: {}", e);
        }
    }
}

/// The process-wide cache for one profile (loaded once from its file).
fn mate_cache_for(profile: &str) -> std::sync::Arc<std::sync::Mutex<MateNames>> {
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHES: OnceLock<Mutex<HashMap<String, Arc<Mutex<MateNames>>>>> = OnceLock::new();
    let map = CACHES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
    g.entry(profile.to_string())
        .or_insert_with(|| {
            let path = crate::config::Config::config_path_for(profile)
                .ok()
                .map(|p| {
                    let stem = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("config")
                        .to_string();
                    p.with_file_name(format!("mates-v2-{}.json", stem))
                });
            Arc::new(Mutex::new(MateNames::load(path)))
        })
        .clone()
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// V2 keys bind a name to the signed-in account. Legacy unbound names are
/// ignored so a prior self/last-sender fallback cannot survive an upgrade.
fn mate_cache_key(chat_id: &str, owner: &str) -> Option<String> {
    let owner = owner.trim().to_ascii_lowercase();
    let owner = owner.strip_prefix("8:orgid:").unwrap_or(&owner);
    looks_like_guid(owner).then(|| format!("v2:{owner}:{chat_id}"))
}

/// Mate names for `ids` (untitled 1:1 threads): cache hits answer with no
/// network; the misses are looked up together, `concurrency` at a time,
/// and every name found is cached and saved once. A lookup that fails
/// leaves that chat out (the caller keeps its fallback name).
pub(crate) async fn resolve_mates_cached(
    client: &TeamsClient,
    ids: &[String],
    me: &str,
    cache: &std::sync::Mutex<MateNames>,
    now: u64,
    concurrency: usize,
) -> HashMap<String, String> {
    use futures::StreamExt;
    let mut out = HashMap::new();
    let mut misses: Vec<String> = Vec::new();
    {
        let g = cache.lock().unwrap_or_else(|e| e.into_inner());
        for id in ids {
            let Some(key) = mate_cache_key(id, me) else {
                continue;
            };
            match g.get(&key, now) {
                Some(n) => {
                    out.insert(id.clone(), n);
                }
                None => misses.push(id.clone()),
            }
        }
    }
    if misses.is_empty() {
        return out;
    }
    let found: Vec<(String, Option<String>)> = futures::stream::iter(misses)
        .map(|id| async move {
            let name = resolve_mate_name(client, &id, me).await;
            (id, name)
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    let mut any = false;
    for (id, name) in found {
        if let Some(n) = name {
            let Some(key) = mate_cache_key(&id, me) else {
                continue;
            };
            g.put(&key, &n, now);
            out.insert(id, n);
            any = true;
        } else {
            tracing::debug!("mate resolve failed for {}", id);
        }
    }
    if any {
        g.save();
    }
    out
}

// ---------------------------------------------------------------------------
// 1:1 chat create (om-lt5-person11: person-pick opens 1:1)
// ---------------------------------------------------------------------------

/// `POST /me/chats` path for 1:1 creation. Pure so tests pin it.
pub fn one_to_one_create_path() -> &'static str {
    "/me/chats"
}

/// `POST /me/chats` body for a 1:1 with `user` (AAD id or UPN).
/// Self is implied (members carries the peer only, owner role).
/// Pure so tests pin it.
pub fn one_to_one_create_body(user: &str) -> serde_json::Value {
    serde_json::json!({
        "chatType": "oneOnOne",
        "members": [
            {
                "@odata.type": "#microsoft.graph.aadUserConversationMember",
                "roles": ["owner"],
                "user@odata.bind": format!(
                    "https://graph.microsoft.com/v1.0/users('{}')",
                    user.trim()
                ),
            }
        ],
    })
}

#[derive(Debug, Deserialize)]
struct CreatedChat {
    id: String,
    topic: Option<String>,
}

/// Parse a `POST /me/chats` 1:1 response into a chat row. Graph
/// returns no topic for 1:1s — the caller names the thread after
/// the peer. Pure so tests pin it.
pub fn parse_created_chat(value: &serde_json::Value) -> Result<ChatInfo> {
    let chat: CreatedChat =
        serde_json::from_value(value.clone()).context("Failed to parse created chat response")?;
    Ok(ChatInfo {
        id: chat.id,
        name: chat.topic.unwrap_or_default(),
        peer_name_verified: false,
        is_group: false,
        picture: None,
        is_meeting: false,
        muted: None,
        hidden: None,
        unread: None,
        last_message_sender_mri: None,
        last_message_time: None,
        last_message_sender: None,
        last_message_preview: None,
    })
}

/// OstMac §106: the chat-service 1:1 thread id for two AAD object ids
/// (pure): `19:<lo>_<hi>@unq.gbl.spaces`, ids lowercased and in ascending
/// order (every 1:1 id on the owner's account has this shape). The same
/// pair always names the same thread, so an existing 1:1 re-opens and a
/// new one is created by the chat service on its first post.
pub fn one_to_one_thread_id(a: &str, b: &str) -> String {
    let (a, b) = (a.trim().to_ascii_lowercase(), b.trim().to_ascii_lowercase());
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    format!("19:{}_{}@unq.gbl.spaces", lo, hi)
}

fn looks_like_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n)
        && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
}

/// Resolve the peer only from a canonical two-person thread containing the owner.
pub fn direct_peer_id(chat_id: &str, owner: &str) -> Option<String> {
    let owner = owner.trim().to_ascii_lowercase();
    let owner = owner.strip_prefix("8:orgid:").unwrap_or(&owner);
    let pair = chat_id.strip_prefix("19:")?.strip_suffix("@unq.gbl.spaces")?;
    let (first, second) = pair.split_once('_')?;
    if !looks_like_guid(owner) || !looks_like_guid(first) || !looks_like_guid(second) { return None; }
    if first.eq_ignore_ascii_case(owner) { Some(second.to_ascii_lowercase()) }
    else if second.eq_ignore_ascii_case(owner) { Some(first.to_ascii_lowercase()) }
    else { None }
}

/// AAD object id for `me` or a user ref (AAD id passes through; UPN /
/// mail resolves via Graph `GET /users/{ref}?$select=id`).
async fn aad_object_id(client: &TeamsClient, user: &str) -> Result<String> {
    let u = user.trim();
    if looks_like_guid(u) {
        return Ok(u.to_ascii_lowercase());
    }
    let path = if u == "me" {
        "/me?$select=id".to_string()
    } else {
        let seg: String = u
            .bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'@' | b'.' | b'-' | b'_' => {
                    (b as char).to_string()
                }
                _ => format!("%{:02X}", b),
            })
            .collect();
        format!("/users/{}?$select=id", seg)
    };
    let v: serde_json::Value = client
        .graph_get(&path)
        .await?
        .json()
        .await
        .context("Failed to parse user id response")?;
    v["id"]
        .as_str()
        .filter(|s| looks_like_guid(s))
        .map(str::to_ascii_lowercase)
        .context("user id missing")
}

/// OstMac §106: chat-service body creating the 1:1 thread for two AAD
/// ids (pure). `uniquerosterthread` + `fixedRoster` make it the roster's
/// one 1:1 (`19:<lo>_<hi>@unq.gbl.spaces`), never a group thread.
pub fn one_to_one_thread_body(me: &str, peer: &str) -> serde_json::Value {
    serde_json::json!({
        "members": [
            {"id": format!("8:orgid:{}", peer.trim().to_ascii_lowercase()), "role": "Admin"},
            {"id": format!("8:orgid:{}", me.trim().to_ascii_lowercase()), "role": "Admin"},
        ],
        "properties": {
            "threadType": "chat",
            "chatFilesIndexId": "2",
            "fixedRoster": "true",
            "uniquerosterthread": "true",
        },
    })
}

/// Create (or re-open) the 1:1 chat with `user` (AAD id or UPN) and
/// return the thread. Empty refs are rejected before any network.
/// OstMac §106: Graph `POST /me/chats` answers 405 (not a create
/// endpoint) and Graph `POST /chats` needs Chat.Create, which the Teams
/// web token lacks (403) — every live 1:1 open / quick message failed.
/// Now: both AAD ids resolve to the derived thread id
/// ([`one_to_one_thread_id`]); an existing thread re-opens; a first
/// contact creates the unique-roster 1:1 on the chat service.
pub async fn create_one_to_one_chat_data(client: &TeamsClient, user: &str) -> Result<ChatInfo> {
    if user.trim().is_empty() {
        bail!("empty user");
    }
    let me = aad_object_id(client, "me").await?;
    let peer = aad_object_id(client, user).await?;
    if me == peer {
        bail!("1:1 with self (use the self-chat)");
    }
    let derived = one_to_one_thread_id(&me, &peer);
    let base = client.chat_service_url();
    let probe = format!("{}/v1/threads/{}?view=msnp24Equivalent", base, derived);
    let missing = match client.chat_get(&probe).await {
        Ok(_) => false,
        Err(e) => format!("{:#}", e).starts_with("HTTP 404"),
    };
    if missing {
        let resp = client
            .chat_post(
                &format!("{}/v1/threads", base),
                &one_to_one_thread_body(&me, &peer),
            )
            .await
            .context("1:1 thread create failed")?;
        let made = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| l.split('?').next())
            .and_then(|l| l.rsplit('/').next())
            .map(|id| {
                id.replace("%3A", ":")
                    .replace("%3a", ":")
                    .replace("%40", "@")
            })
            .unwrap_or_default();
        if !made.is_empty() && made != derived {
            bail!("1:1 thread create answered an unexpected thread");
        }
    }
    Ok(ChatInfo {
        id: derived,
        name: String::new(),
        peer_name_verified: false,
        is_group: false,
        picture: None,
        is_meeting: false,
        muted: None,
        hidden: None,
        unread: None,
        last_message_sender_mri: None,
        last_message_time: None,
        last_message_sender: None,
        last_message_preview: None,
    })
}

// ---------------------------------------------------------------------------
// Group chat create (OstMac core-a, G5)
// ---------------------------------------------------------------------------

/// Chat-service thread create URL (group and 1:1). Pure so tests pin it.
pub fn thread_create_url(base: &str) -> String {
    format!("{}/v1/threads", base.trim_end_matches('/'))
}

/// Trim, drop blanks, and de-duplicate user refs (case-insensitive,
/// first spelling wins); `self_id` is removed (it is added as the
/// creator). Pure so tests pin it.
pub fn group_chat_members(self_id: &str, users: &[String]) -> Vec<String> {
    let me = self_id.trim().to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for u in users {
        let t = u.trim();
        if t.is_empty() || t.to_lowercase() == me {
            continue;
        }
        if out.iter().any(|x| x.eq_ignore_ascii_case(t)) {
            continue;
        }
        out.push(t.to_string());
    }
    out
}

/// OstMac §GRAPHSWEEP: chat-service body creating a group chat thread
/// for AAD object ids: the creator first, then every peer, all `Admin`
/// (Teams group chats let every member manage the roster); blank topics
/// are omitted. `peers` must already be normalized
/// ([`group_chat_members`]) and resolved to object ids. Pure so tests
/// pin it.
pub fn group_chat_create_body(
    self_id: &str,
    peers: &[String],
    topic: Option<&str>,
) -> serde_json::Value {
    let member = |u: &str| {
        serde_json::json!({
            "id": format!("8:orgid:{}", u.trim().to_ascii_lowercase()),
            "role": "Admin",
        })
    };
    let mut members = vec![member(self_id)];
    members.extend(peers.iter().map(|u| member(u)));
    let mut properties = serde_json::json!({
        "threadType": "chat",
        "chatFilesIndexId": "2",
    });
    if let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty()) {
        properties["topic"] = serde_json::Value::String(t.to_string());
    }
    serde_json::json!({ "members": members, "properties": properties })
}

/// Thread id from a chat-service thread-create `Location` header
/// (`…/v1/threads/19%3A…%40thread.v2?…`), decoded; None when absent or
/// not a thread id. Pure so tests pin it.
pub fn created_thread_id(location: Option<&str>) -> Option<String> {
    let id = location?
        .split('?')
        .next()?
        .rsplit('/')
        .next()?
        .replace("%3A", ":")
        .replace("%3a", ":")
        .replace("%40", "@");
    (id.starts_with("19:") && id.contains('@')).then_some(id)
}

/// Create a group chat with `users` (AAD ids or UPNs) plus the signed-in
/// user, with an optional topic, on the chat service (`POST /v1/threads`,
/// skypetoken). OstMac §GRAPHSWEEP: Graph `POST /chats` needs Chat.Create,
/// which the Teams web token lacks (403). UPNs resolve to object ids via
/// Graph `GET /users/{ref}?$select=id` (User.ReadBasic.All, granted). At
/// least one peer is required (checked before any network). Returns the
/// new thread (`is_group` true) named after the topic.
pub async fn create_group_chat_data(
    client: &TeamsClient,
    users: &[String],
    topic: Option<&str>,
) -> Result<ChatInfo> {
    if group_chat_members("", users).is_empty() {
        bail!("no members");
    }
    let me = aad_object_id(client, "me").await?;
    let mut peers: Vec<String> = Vec::new();
    for u in group_chat_members(&me, users) {
        let oid = aad_object_id(client, &u).await?;
        if oid != me && !peers.contains(&oid) {
            peers.push(oid);
        }
    }
    if peers.is_empty() {
        bail!("no members besides self");
    }
    let resp = client
        .chat_post(
            &thread_create_url(&client.chat_service_url()),
            &group_chat_create_body(&me, &peers, topic),
        )
        .await
        .context("group chat create failed")?;
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let id = created_thread_id(location.as_deref())
        .context("group chat create answered no thread id")?;
    Ok(ChatInfo {
        id,
        name: topic.map(str::trim).unwrap_or_default().to_string(),
        peer_name_verified: false,
        is_group: true,
        picture: None,
        is_meeting: false,
        muted: None,
        hidden: None,
        unread: None,
        last_message_sender_mri: None,
        last_message_time: None,
        last_message_sender: None,
        last_message_preview: None,
    })
}

// ---------------------------------------------------------------------------
// Chat roster (OstMac core-a)
// ---------------------------------------------------------------------------

/// One chat member. `mri` is the chat-service identity
/// (`8:orgid:<guid>`); `user_id` the Graph/AAD id (presence key).
/// `is_owner` is id-based: Graph `roles` contains `owner`, or the
/// chat-service role is `Admin`. `display_name` may be "" (chat
/// service rosters carry no names — the embedder resolves them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMemberInfo {
    pub mri: String,
    pub user_id: Option<String>,
    pub display_name: String,
    pub email: Option<String>,
    pub roles: Vec<String>,
    pub is_owner: bool,
}

/// Graph `GET /chats/{id}/members` path. Pure so tests pin it.
pub fn chat_members_path(chat_id: &str) -> String {
    format!("/chats/{}/members", chat_id.trim())
}

/// AAD object id from an orgid MRI (`8:orgid:<guid>` → `<guid>`).
fn oid_from_orgid_mri(mri: &str) -> Option<String> {
    let t = mri.trim();
    let rest = t
        .get(..8)
        .filter(|p| p.eq_ignore_ascii_case("8:orgid:"))
        .map(|_| &t[8..])?;
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// Parse a Graph `conversationMember` collection (`{value:[…]}`).
/// Entries without a `userId` are skipped (bots/guests we cannot key).
/// Pure so tests pin it.
pub fn parse_graph_chat_members(value: &serde_json::Value) -> Result<Vec<ChatMemberInfo>> {
    let list = value
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("chat members response has no value[]"))?;
    let mut out = Vec::new();
    for e in list {
        let Some(o) = e.as_object() else { continue };
        let Some(user_id) = obj_str_ci(o, "userId") else {
            continue;
        };
        let roles: Vec<String> = obj_get_ci(o, "roles")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str())
                    .map(|r| r.trim().to_lowercase())
                    .filter(|r| !r.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let is_owner = roles.iter().any(|r| r == "owner");
        out.push(ChatMemberInfo {
            mri: format!("8:orgid:{}", user_id),
            user_id: Some(user_id),
            display_name: obj_str_ci(o, "displayName").unwrap_or_default(),
            email: obj_str_ci(o, "email"),
            roles,
            is_owner,
        });
    }
    Ok(out)
}

/// Parse a chat-service `GET /v1/threads/{id}/members` body. Roles
/// lowercase (`admin`/`user`); `admin` is the owner. Blank ids skip.
/// Pure so tests pin it.
pub fn parse_thread_members(value: &serde_json::Value) -> Result<Vec<ChatMemberInfo>> {
    let body: ThreadMembersResponse =
        serde_json::from_value(value.clone()).context("Failed to parse thread members response")?;
    Ok(body
        .members
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let mri =
                m.id.map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())?;
            let roles: Vec<String> = m
                .role
                .map(|r| r.trim().to_lowercase())
                .filter(|r| !r.is_empty())
                .into_iter()
                .collect();
            let is_owner = roles.iter().any(|r| r == "admin");
            Some(ChatMemberInfo {
                user_id: oid_from_orgid_mri(&mri),
                mri,
                display_name: m
                    .friendly_name
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                email: None,
                roles,
                is_owner,
            })
        })
        .collect())
}

/// Where a roster came from (names are only on the Graph path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterSource {
    Graph,
    ChatService,
}

/// Roster for one chat: the chat-service thread roster (MRIs, friendly
/// names when the service has them, Admin/User roles; skypetoken auth).
/// OstMac §GRAPHSWEEP: the Graph `GET /chats/{id}/members` attempt is
/// gone — it needs ChatMember.Read/Chat.ReadBasic, which the Teams web
/// token lacks (live 403 on every open). Failure returns the chat-service
/// error (never an empty roster).
pub async fn list_chat_members_data(
    client: &TeamsClient,
    chat_id: &str,
) -> Result<(RosterSource, Vec<ChatMemberInfo>)> {
    if chat_id.trim().is_empty() {
        bail!("empty chat_id");
    }
    let url = format!(
        "{}/v1/threads/{}/members",
        client.chat_service_url(),
        chat_id.trim()
    );
    let resp = client.chat_get(&url).await?;
    let v: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse thread members response")?;
    let mut members = parse_thread_members(&v)?;
    fill_member_names(client, &mut members).await;
    Ok((RosterSource::ChatService, members))
}

/// Most chat-service rosters carry no names (live: 2/2 blank). Blank
/// names with an AAD id are filled from Graph `GET /users/{oid}`
/// (`$select=displayName,mail,userPrincipalName`; User.ReadBasic.All is
/// granted and proven live), [`ROSTER_NAME_FILL_MAX`] at most, a few at
/// a time. A failed lookup leaves that member blank (the row shows its
/// email or "Unknown"); it never drops the member.
pub const ROSTER_NAME_FILL_MAX: usize = 60;

/// Graph path for one roster name lookup (pure; `oid` must be a GUID).
pub fn roster_user_path(oid: &str) -> String {
    format!(
        "/users/{}?$select=displayName,mail,userPrincipalName",
        oid.trim()
    )
}

/// Apply one `/users/{oid}` answer to a blank member (pure).
pub fn apply_roster_user(member: &mut ChatMemberInfo, user: &serde_json::Value) {
    let text = |k: &str| {
        user[k]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if member.display_name.trim().is_empty() {
        if let Some(n) = text("displayName") {
            member.display_name = n;
        }
    }
    if member.email.is_none() {
        member.email = text("mail").or_else(|| text("userPrincipalName"));
    }
}

async fn fill_member_names(client: &TeamsClient, members: &mut [ChatMemberInfo]) {
    let todo: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, m)| m.display_name.trim().is_empty())
        .filter(|(_, m)| m.user_id.as_deref().map(looks_like_guid).unwrap_or(false))
        .map(|(i, _)| i)
        .take(ROSTER_NAME_FILL_MAX)
        .collect();
    for chunk in todo.chunks(8) {
        let reads = chunk.iter().map(|&i| {
            let path = roster_user_path(members[i].user_id.as_deref().unwrap_or(""));
            async move {
                let v: Option<serde_json::Value> = match client.graph_get(&path).await {
                    Ok(r) => r.json().await.ok(),
                    Err(e) => {
                        tracing::debug!("roster name lookup failed: {:#}", e);
                        None
                    }
                };
                (i, v)
            }
        });
        for (i, v) in futures::future::join_all(reads).await {
            if let Some(v) = v {
                apply_roster_user(&mut members[i], &v);
            }
        }
    }
}

/// List recent chats and return structured data.
///
/// Direct-chat names come only from the peer MRI. Unresolved direct chats
/// keep a neutral label; a topic or last sender never impersonates the peer.
/// The account identity is required before resolving or reading cached names.
pub async fn list_chats_data(client: &TeamsClient, limit: usize) -> Result<Vec<ChatInfo>> {
    let owner = whoami_data(client).await?;
    list_chats_data_for_owner(client, limit, &owner.id).await
}

/// One owner identity, one naming rule. The embedding client reuses its /me
/// response; this entry point never performs another profile lookup.
pub async fn list_chats_data_for_owner(
    client: &TeamsClient,
    limit: usize,
    owner: &str,
) -> Result<Vec<ChatInfo>> {
    let cache = mate_cache_for(&crate::config::active_profile());
    list_chats_with_cache(client, limit, owner, &cache, epoch_secs()).await
}

async fn list_chats_with_cache(
    client: &TeamsClient,
    limit: usize,
    owner: &str,
    cache: &std::sync::Mutex<MateNames>,
    now: u64,
) -> Result<Vec<ChatInfo>> {
    if mate_cache_key("", owner).is_none() {
        bail!("Cannot resolve chat identities without an account id");
    }
    let conversations = fetch_chat_conversations(client, limit).await?;
    let mut chats: Vec<_> = conversations.iter().filter_map(conversation_info).collect();
    let ids: Vec<_> = chats
        .iter()
        .filter(|chat| is_onetoone_id(&chat.id))
        .map(|chat| chat.id.clone())
        .collect();
    if !ids.is_empty() {
        let names = resolve_mates_cached(client, &ids, owner, cache, now, MATE_CONCURRENCY).await;
        apply_peer_names(&mut chats, &names);
    }
    Ok(chats)
}

fn apply_peer_names(chats: &mut [ChatInfo], names: &HashMap<String, String>) {
    for chat in chats.iter_mut().filter(|chat| is_onetoone_id(&chat.id)) {
        if let Some(name) = names.get(&chat.id) {
            chat.name = name.clone();
            chat.peer_name_verified = true;
        }
    }
}

fn conversation_info(conv: &Conversation) -> Option<ChatInfo> {
    let id = conv
        .id
        .as_deref()
        .filter(|id| !id.trim().is_empty())?
        .to_owned();
    let last_message = conv.last_message.as_ref();
    Some(ChatInfo {
        name: conversation_name(conv),
        peer_name_verified: false,
        is_group: id.contains("thread") || id.contains("meeting"),
        picture: conv
            .thread_properties
            .as_ref()
            .and_then(|props| props.picture.clone())
            .filter(|picture| !picture.trim().is_empty()),
        is_meeting: conv
            .thread_properties
            .as_ref()
            .and_then(|props| props.thread_type.as_deref())
            .is_some_and(|kind| kind.eq_ignore_ascii_case("meeting")),
        muted: conv.properties.as_ref().and_then(alerts_muted),
        hidden: conv.properties.as_ref().and_then(conversation_hidden),
        unread: conv.show_red_dot.as_ref().and_then(Value::as_bool),
        last_message_time: last_message
            .and_then(|msg| {
                msg.original_arrival_time
                    .as_ref()
                    .or(msg.compose_time.as_ref())
            })
            .cloned(),
        last_message_sender: last_message.and_then(|msg| msg.im_display_name.clone()),
        last_message_sender_mri: last_message.and_then(|msg| {
            let mri = mri_from_user_link(msg.from.as_deref());
            (!mri.is_empty()).then_some(mri)
        }),
        last_message_preview: last_message.map(directory_preview),
        id,
    })
}

fn directory_preview(message: &NativeMessage) -> String {
    let text = directory_preview_body(message);
    if text.len() <= 80 {
        return text;
    }
    let end = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= 77)
        .last()
        .unwrap_or(0);
    format!("{}...", &text[..end])
}

async fn fetch_chat_conversations(client: &TeamsClient, limit: usize) -> Result<Vec<Conversation>> {
    // Strategy 1: CSA AFD endpoint with Bearer auth
    let csa_url = format!(
        "{}/teams/users/ME/conversations?view=mychats&pageSize={}",
        client.csa_base(),
        limit
    );
    tracing::debug!("Trying CSA endpoint: {}", csa_url);
    let resp = match client.csa_get(&csa_url).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("CSA endpoint failed: {:#}, trying chatsvcagg", e);
            // Strategy 2: chatsvcagg with skypetoken auth
            let base = client.chatsvcagg_url();
            let url = format!(
                "{}/api/v2/users/ME/conversations?view=mychats&pageSize={}",
                base, limit
            );
            tracing::debug!("Trying chatsvcagg: {}", url);
            match client.chat_get(&url).await {
                Ok(r) => r,
                Err(e2) => {
                    tracing::debug!("chatsvcagg failed: {:#}, trying chat service", e2);
                    // Strategy 3: chat service (amer.ng.msg) with skypetoken auth
                    let base = client.chat_service_url();
                    let url = format!(
                        "{}/v1/users/ME/conversations?view=mychats&pageSize={}",
                        base, limit
                    );
                    client.chat_get(&url).await?
                }
            }
        }
    };

    let body: ConversationsResponse = resp
        .json()
        .await
        .context("Failed to parse conversations response")?;

    Ok(body.conversations.unwrap_or_default())
}

/// Read messages from a specific chat thread and return structured data.
///
/// Newest page only; use [`read_messages_page`] with the returned
/// `backward_link` to walk older history.
pub async fn read_messages_data(
    client: &TeamsClient,
    chat_id: &str,
    limit: usize,
) -> Result<Vec<MessageInfo>> {
    Ok(read_messages_page(client, chat_id, limit, None)
        .await?
        .messages)
}

/// Read one page of history. `page_url` is None for the newest page or
/// Some(previous `backward_link`) for the next older page. Messages come
/// back oldest-first; pages never overlap (verified live 2026-09-22).
pub async fn read_messages_page(
    client: &TeamsClient,
    chat_id: &str,
    limit: usize,
    page_url: Option<&str>,
) -> Result<MessagesPage> {
    let url = match page_url {
        Some(u) => with_message_properties_view(&with_page_size(u, limit)),
        None => {
            let base = client.chat_service_url();
            with_message_properties_view(&format!(
                "{}/v1/users/ME/conversations/{}/messages?pageSize={}",
                base, chat_id, limit
            ))
        }
    };

    tracing::debug!("Reading messages from {}", url);
    let resp = client.chat_get(&url).await?;
    let body: MessagesResponse = resp
        .json()
        .await
        .context("Failed to parse messages response")?;

    Ok(MessagesPage {
        messages: project_messages(&body.messages.unwrap_or_default()),
        backward_link: body.metadata.and_then(|m| m.backward_link),
    })
}

/// Read the full history projection for one message mentioned by a chat update.
pub async fn read_message_with_client(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
) -> Result<Option<MessageInfo>> {
    let url = with_message_properties_view(&message_url(
        &client.chat_service_url(),
        &encode_uri_component(chat_id),
        &encode_uri_component(message_id),
    ));
    let response = client.chat_get(&url).await?;
    let message: NativeMessage = response.json().await.context("Failed to parse message response")?;
    if message.id.as_deref() != Some(message_id) {
        anyhow::bail!("Unexpected chat message identity");
    }
    Ok(project_messages(&[message]).pop())
}

fn project_messages(messages: &[NativeMessage]) -> Vec<MessageInfo> {
    let mut result = Vec::new();
    // Messages come newest-first; reverse for chronological display.
    for msg in messages.iter().rev() {
        let msgtype = msg.messagetype.as_deref().unwrap_or("");
        let forwarded = forwarded_message_context(msg);

        let sender = msg
            .im_display_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("?")
            .to_string();
        let sender_mri = mri_from_user_link(msg.from.as_deref());
        let time = msg
            .original_arrival_time
            .as_deref()
            .or(msg.compose_time.as_deref())
            .unwrap_or("")
            .to_string();
        let content = rich_card_content(msg, msg.content.as_deref().unwrap_or(""));
        let card_wire_shape = card_wire_shape(msg);
        // OstMac om-replies: split the quote block first so `content` is
        // the reply body only; the parent id rides `reply_to`.
        // OstMac om-lt2-quotelink: channel threads carry no quote block —
        // fall back to the wire parent (`rootMessageId` / `replyToId`).
        let (quote_parent, body_html) = split_reply_quote(&content);
        let wire_parent = message_parent_id(msg);
        let mut reply_to = quote_parent.or(wire_parent);
        let text = strip_html(&body_html);

        // OstMac om-richmedia: image-only bubbles strip to "" but are
        // real messages — keep them (the embedder mines `<img>` from raw).
        // OstMac om-botposts: same for RSS/bot/card posts — attachment and
        // card payloads strip to "" but the embedder renders them as
        // title+link rows (or a placeholder when unparseable).

        // OstMac: keep the server id so embedders can match realtime edits.
        let id = msg
            .id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(String::from);
        let id = id.unwrap_or_else(|| format!("{}@{}", time, sender));
        // Self/blank parents never link (corrupt wire id guard).
        if reply_to
            .as_deref()
            .map(|p| p.trim().is_empty() || p == id)
            .unwrap_or(false)
        {
            reply_to = None;
        }
        let reactions = message_reactions(msg);
        let reaction_properties = message_reaction_properties(msg);
        let files = native_message_file_refs(msg, &id, &sender, &time);
        if !keep_message(msgtype, &text, &content, forwarded.as_ref())
            && (files.is_empty() || !message_type_kept(msgtype))
        { continue; }
        result.push(MessageInfo {
            id,
            application_id: message_application_id(msg),
            sender_mri,
            sender,
            timestamp: time,
            content: text.trim().to_string(),
            raw: content,
            card_wire_shape,
            reactions,
            reaction_properties,
            mention_properties: msg.properties.as_ref().and_then(|properties|properties.extra.iter().find(|(key,_)|key.eq_ignore_ascii_case("mentions")).map(|(_,value)|value.clone())),
            reply_to,
            client_message_id: client_message_id_of(&msg.extra),
            forwarded,
            files,
            importance: message_importance(msg),
            edited: message_was_edited(msg),
        });
    }

    result
}

/// Type gate for history messages (pure so tests pin it).
///
/// Keeps Text/RichText, including RichText/Media_Card: bot/card posts
/// (RSS, roadmap, connector cards) strip to their readable summary
/// text, and whole channels carry nothing else — dropping them renders
/// long channels blank (H0: 100 raw Media_Card → 0 kept). Still drops
/// RichText/Media_CallRecording (strips to "TitlePlay" fragments) and
/// RichText/Media_CallTranscript (strips to raw JSON), which are not
/// readable bubbles (see task-0011), plus all ThreadActivity/* noise.
fn message_type_kept(messagetype: &str) -> bool {
    if !messagetype.contains("Text") && !messagetype.contains("RichText") {
        return false;
    }
    if messagetype.contains("Media_") && !messagetype.contains("Media_Card") {
        return false;
    }
    true
}

fn keep_message(
    message_type: &str,
    text: &str,
    raw_content: &str,
    forwarded: Option<&ForwardedMessageContext>,
) -> bool {
    (message_type_kept(message_type) || forwarded.is_some())
        && (!text.trim().is_empty()
            || has_image(raw_content)
            || has_card_payload(raw_content)
            || forwarded.is_some())
}

/// Render card attachments carried separately from the compatibility text.
/// This only consumes attachment data already present on the message.
fn card_attachment_html(message: &NativeMessage) -> Option<String> {
    // The native chat service carries cards as JSON in properties.cards;
    // Graph responses expose the same content as an attachments array.
    let encoded = message.properties.as_ref().and_then(|p| p.extra.get("cards"));
    let parsed = match encoded {
        Some(Value::String(raw)) if raw.len() <= 256 * 1024 => serde_json::from_str::<Value>(raw).ok(),
        _ => None,
    };
    let Some(attachments) = parsed.as_ref().or(encoded).and_then(Value::as_array)
        .or_else(|| message.extra.get("attachments").and_then(Value::as_array)) else {
        return message.content.as_deref().and_then(swift_card_html);
    };
    let mut html = String::new();
    for attachment in attachments.iter().take(8) {
        let Some(kind) = attachment.get("contentType").and_then(Value::as_str) else { continue; };
        if !kind.eq_ignore_ascii_case("application/vnd.microsoft.card.adaptive") {
            continue;
        }
        let Some(content) = attachment.get("content") else { continue; };
        let value: Value = match content {
            Value::String(raw) if raw.len() <= 256 * 1024 => {
                let Ok(value) = serde_json::from_str(raw) else { continue; };
                value
            },
            Value::Object(_) => content.clone(),
            _ => continue,
        };
        if !value.get("type").and_then(Value::as_str).is_some_and(|kind| kind.eq_ignore_ascii_case("AdaptiveCard")) {
            continue;
        }
        let Ok(json) = serde_json::to_string(&value) else { continue; };
        if json.len() > 256 * 1024 { continue; }
        let escaped = json.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        html.push_str("<attachment contenttype=\"application/vnd.microsoft.card.adaptive\">");
        html.push_str(&escaped);
        html.push_str("</attachment>");
    }
    if !html.is_empty() {
        return Some(html);
    }
    message.content.as_deref().and_then(swift_card_html)
}

/// Workflows sends the Adaptive Card inside `<URIObject><swift b64>`, not in
/// `properties.cards`. The official worker decodes that attribute and reads
/// `attachments[0].content`. The compatibility sentence stays in the HTML.
fn swift_card_html(content: &str) -> Option<String> {
    let unescaped;
    let content = if content.to_ascii_lowercase().contains("&lt;uriobject")
        || content.to_ascii_lowercase().contains("&lt;swift")
    {
        unescaped = content
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'");
        unescaped.as_str()
    } else {
        content
    };
    let mut html = String::new();
    let mut found = false;
    let lower = content.to_ascii_lowercase();
    let mut cursor = 0;
    while html.len() < 256 * 1024 {
        let Some(rel) = lower[cursor..].find("<swift") else { break };
        let tag_at = cursor + rel;
        let Some(end) = content[tag_at..].find('>') else { break };
        let tag = &content[tag_at..tag_at + end];
        cursor = tag_at + end + 1;
        let Some(encoded) = attr_value(tag, "b64") else { continue };
        if encoded.len() > 256 * 1024 { continue; }
        let Some(json) = decode_card_b64(encoded) else { continue };
        if append_swift_cards(&mut html, &json) { found = true; }
    }
    if !found {
        if let Some(json) = uriobject_json(content) {
            found = append_swift_cards(&mut html, &json);
        }
    }
    found.then_some(html)
}

fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(name) {
        let at = search + rel;
        let boundary = at == 0
            || lower.as_bytes()[at - 1].is_ascii_whitespace()
            || lower.as_bytes()[at - 1] == b'<';
        let after = at + name.len();
        search = after;
        if !boundary || after > tag.len() { continue; }
        let rest = tag[after..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' { continue; }
        let rest = &rest[quote.len_utf8()..];
        let end = rest.find(quote)?;
        return Some(&rest[..end]);
    }
    None
}

fn decode_card_b64(encoded: &str) -> Option<String> {
    use base64::Engine;
    let cleaned: String = encoded.chars().filter(|ch| !ch.is_ascii_whitespace()).collect();
    if cleaned.is_empty() || cleaned.len() > 256 * 1024 { return None; }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&cleaned))
        .ok()?;
    if bytes.len() > 256 * 1024 { return None; }
    String::from_utf8(bytes).ok()
}

fn append_swift_cards(html: &mut String, json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(json) else { return false };
    let attachments = if let Some(list) = value.get("attachments").and_then(Value::as_array) {
        list.clone()
    } else if value.get("contentType").is_some() {
        vec![value.clone()]
    } else if value.get("type").and_then(Value::as_str).is_some_and(|kind| kind.eq_ignore_ascii_case("AdaptiveCard")) {
        vec![serde_json::json!({"contentType":"application/vnd.microsoft.card.adaptive","content":value})]
    } else {
        Vec::new()
    };
    let before = html.len();
    for attachment in attachments.iter().take(8) {
        let Some(kind) = attachment.get("contentType").and_then(Value::as_str) else { continue };
        if !kind.eq_ignore_ascii_case("application/vnd.microsoft.card.adaptive")
            && !kind.eq_ignore_ascii_case("application/vnd.microsoft.card.o365connector")
        {
            continue;
        }
        let Some(content) = attachment.get("content") else { continue };
        let value = match content {
            Value::String(raw) if raw.len() <= 256 * 1024 => {
                let Ok(value) = serde_json::from_str::<Value>(raw) else { continue };
                value
            }
            Value::Object(_) => content.clone(),
            _ => continue,
        };
        let Ok(json) = serde_json::to_string(&value) else { continue };
        if json.len() > 256 * 1024 { continue; }
        let escaped = json.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        html.push_str("<attachment contenttype=\"");
        html.push_str(kind);
        html.push_str("\">");
        html.push_str(&escaped);
        html.push_str("</attachment>");
    }
    html.len() > before
}

fn uriobject_json(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find("<uriobject") {
        let at = cursor + rel;
        let Some(gt) = content[at..].find('>') else { return None };
        let body_at = at + gt + 1;
        let Some(end_rel) = lower[body_at..].find("</uriobject") else { return None };
        let body = content[body_at..body_at + end_rel].trim();
        if body.to_ascii_lowercase().contains("<swift") {
            cursor = body_at + end_rel;
            continue;
        }
        let text = if body.contains('%') { percent_decode(body) } else { body.to_owned() };
        let text = text.trim().to_owned();
        if text.starts_with('{') || text.starts_with('[') { return Some(text); }
        cursor = body_at + end_rel;
    }
    None
}

fn compatibility_only(content: &str) -> bool {
    let plain = strip_html(content);
    let folded = plain.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    let phrase = "card - access it on https://go.skype.com/cards.unsupported";
    if !folded.contains(phrase) { return false; }
    let mut rest = folded.as_str();
    let mut removed = false;
    loop {
        let next = rest.trim().trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
        if let Some(next) = next.strip_prefix(phrase) {
            let next = next.trim_start_matches(|ch: char| ch == '.' || ch.is_whitespace());
            rest = next.strip_prefix("card").unwrap_or(next);
            removed = true;
            continue;
        }
        break;
    }
    removed && rest.trim().trim_matches(|ch: char| ch == '.' || ch.is_whitespace()).is_empty()
}

fn card_wire_shape(message: &NativeMessage) -> Option<u8> {
    let content = message.content.as_deref()?;
    if !content.contains("go.skype.com/cards.unsupported") { return None; }
    let properties = message.properties.as_ref().map(|value| &value.extra);
    let mut bits = 0;
    bits |= u8::from(message.extra.contains_key("attachments"));
    bits |= u8::from(properties.is_some_and(|p| p.contains_key("attachments"))) << 1;
    bits |= u8::from(message.extra.contains_key("cards")) << 2;
    bits |= u8::from(properties.is_some_and(|p| p.contains_key("cards"))) << 3;
    bits |= u8::from(content.to_ascii_lowercase().contains("<attachment")) << 4;
    bits |= u8::from(card_attachment_html(message).is_some()) << 5;
    let folded = content.to_ascii_lowercase();
    bits |= u8::from(folded.contains("uriobject") || folded.contains("<swift") || folded.contains("&lt;swift")) << 6;
    Some(bits)
}

fn rich_card_content(message: &NativeMessage, content: &str) -> String {
    let Some(cards) = card_attachment_html(message).or_else(|| swift_card_html(content)) else {
        return content.to_owned();
    };
    if compatibility_only(content) {
        cards
    } else {
        format!("{content}{cards}")
    }
}

/// True when raw content carries an RSS/bot/card payload: an
/// `<attachment>` block (case-insensitive) or a card content-type marker
/// (O365 connector / Adaptive / MessageCard). Such posts strip to empty
/// text but must survive filtering — the embedder renders title+link
/// rows from the payload, or a placeholder when it cannot parse it.
fn has_card_payload(html: &str) -> bool {
    let lower = html.to_lowercase();
    lower.contains("<attachment")
        || lower.contains("o365connector")
        || lower.contains("adaptivecard")
        || lower.contains("messagecard")
        || lower.contains("application/vnd.microsoft")
}

/// True when raw HTML carries an `<img` tag (case-insensitive).
/// Image-only messages strip to empty text but must survive filtering.
fn has_image(html: &str) -> bool {
    html.as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"<img"))
}

/// Rewrite the `pageSize=` query value so a followed `backwardLink` honors
/// the caller's limit. No-op when the marker is absent.
// -- Files shared in a chat (chat Shared tab) --

/// One file shared in a chat, from a chat-service message's
/// `properties.files` (the page the timeline reads). The Graph message
/// endpoints need `Chat.Read`, which the Teams web token lacks (403),
/// so this is the chat Shared tab's source. GET only: the consumption
/// horizon (read state) is a separate PUT and never moves here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFilePreview {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFileRef {
    /// Native properties.files.filePreview; a URL is not permission to fetch it.
    pub preview: Option<ChatFilePreview>,
    /// File attachment GUID (the body's `<attachment id>`).
    pub attachment_id: Option<String>,
    pub name: String,
    pub file_type: Option<String>,
    /// SharePoint/OneDrive URL of the file itself.
    pub object_url: String,
    /// Sharing link, when the sender's client made one.
    pub share_url: Option<String>,
    pub sender: Option<String>,
    /// Message arrival time (ISO 8601).
    pub time: Option<String>,
}

fn native_message_file_refs(
    message: &NativeMessage,
    id: &str,
    sender: &str,
    time: &str,
) -> Vec<ChatFileRef> {
    let Some(properties) = message.properties.as_ref() else {
        return Vec::new();
    };
    if properties
        .extra
        .get("deletetime")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Vec::new();
    }
    let Some(files) = properties.extra.get("files") else {
        return Vec::new();
    };
    parse_chat_file_refs(&serde_json::json!({
        "messages": [{
            "id": id,
            "imdisplayname": sender,
            "originalarrivaltime": time,
            "properties": { "files": files }
        }]
    }))
    .0
}

/// Chat-service page size for the Shared tab walk (server maximum).
pub const CHAT_FILES_PAGE_SIZE: usize = 200;

/// Files shared on one chat-service messages page, newest first, and
/// the page's `backwardLink` (older history). Deleted messages and
/// files marked deleted are skipped; `properties.files` may arrive as a
/// JSON string or an array. Pure (no network).
pub fn parse_chat_file_refs(page: &serde_json::Value) -> (Vec<ChatFileRef>, Option<String>) {
    let str_of = |v: &serde_json::Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let mut out = Vec::new();
    for msg in page["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let props = &msg["properties"];
        if str_of(&props["deletetime"]).is_some() {
            continue;
        }
        let files: Vec<serde_json::Value> = match &props["files"] {
            serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
            serde_json::Value::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        for f in &files {
            if f["state"].as_str() == Some("deleted") {
                continue;
            }
            let Some(object_url) =
                str_of(&f["objectUrl"]).or_else(|| str_of(&f["fileInfo"]["fileUrl"]))
            else {
                continue;
            };
            let name = str_of(&f["fileName"])
                .or_else(|| str_of(&f["title"]))
                .or_else(|| object_url.rsplit('/').next().map(String::from))
                .unwrap_or_else(|| "[unnamed]".to_string());
            out.push(ChatFileRef {
                attachment_id: str_of(&f["id"]).or_else(|| str_of(&f["itemid"])),
                preview: str_of(&f["filePreview"]["previewUrl"]).map(|url| {
                    let dimension = |key: &str| f["filePreview"][key].as_u64()
                        .filter(|value| (1..=16_384).contains(value)).unwrap_or(0) as u32;
                    ChatFilePreview { url, width: dimension("previewWidth"), height: dimension("previewHeight") }
                }),
                name,
                file_type: str_of(&f["fileType"]).or_else(|| str_of(&f["type"])),
                object_url,
                share_url: str_of(&f["fileInfo"]["shareUrl"]),
                sender: str_of(&msg["imdisplayname"]),
                time: str_of(&msg["originalarrivaltime"]).or_else(|| str_of(&msg["composetime"])),
            });
        }
    }
    let back = str_of(&page["_metadata"]["backwardLink"]);
    (out, back)
}

/// OstMac §83: the files one chat-service message shares, from a single
/// message object (`GET …/messages/{id}`) or a page holding it. Pure.
pub fn message_file_refs(value: &serde_json::Value, message_id: &str) -> Option<Vec<ChatFileRef>> {
    let want = message_id.trim();
    let one = |m: &serde_json::Value| {
        let id = m["id"]
            .as_str()
            .or_else(|| m["clientmessageid"].as_str())
            .unwrap_or("");
        id == want
    };
    let msg = if let Some(list) = value["messages"].as_array() {
        list.iter().find(|m| one(m))?.clone()
    } else if one(value) {
        value.clone()
    } else {
        return None;
    };
    Some(parse_chat_file_refs(&serde_json::json!({ "messages": [msg] })).0)
}

/// Chat-service message ids are the arrival time in ms: a history page
/// whose oldest message is newer than `message_id` must go further
/// back. Pure.
pub fn page_reaches(page: &serde_json::Value, message_id: &str) -> bool {
    let Ok(want) = message_id.trim().parse::<i64>() else {
        return true;
    };
    page["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str()?.parse::<i64>().ok())
                .any(|id| id <= want)
        })
        .unwrap_or(true)
}

/// OstMac §83: the files one chat message shares, read from the chat
/// service (the Graph message route needs Chat.Read: 403 on the Teams
/// web token). Single-message GET first, then a history walk of at most
/// `max_pages` pages until the page reaching the message. GET only.
pub async fn chat_message_file_refs_data(
    client: &TeamsClient,
    chat_id: &str,
    message_id: &str,
    max_pages: usize,
) -> Result<Vec<ChatFileRef>> {
    let (chat_id, message_id) = (chat_id.trim(), message_id.trim());
    let bad = |s: &str| s.is_empty() || s.contains(['/', '?', '#', ' ']);
    if bad(chat_id) || bad(message_id) {
        bail!("bad chat or message id");
    }
    let one = message_url(&client.chat_service_url(), chat_id, message_id);
    if let Ok(resp) = client.chat_get(&one).await {
        if let Ok(v) = resp.json::<serde_json::Value>().await {
            if let Some(refs) = message_file_refs(&v, message_id) {
                return Ok(refs);
            }
        }
    }
    let mut url = format!(
        "{}/v1/users/ME/conversations/{}/messages?pageSize={}",
        client.chat_service_url(),
        chat_id,
        CHAT_FILES_PAGE_SIZE
    );
    for _ in 0..max_pages.max(1) {
        let page: serde_json::Value = client
            .chat_get(&url)
            .await?
            .json()
            .await
            .context("Failed to parse messages response")?;
        if let Some(refs) = message_file_refs(&page, message_id) {
            return Ok(refs);
        }
        let back = page["_metadata"]["backwardLink"]
            .as_str()
            .map(str::to_string);
        let empty = page["messages"].as_array().map_or(true, |a| a.is_empty());
        match back {
            Some(b) if !empty && !page_reaches(&page, message_id) => {
                url = with_page_size(&b, CHAT_FILES_PAGE_SIZE)
            }
            _ => break,
        }
    }
    bail!("message not found in chat history")
}

/// Files shared in a chat, newest first, deduplicated by file URL: walks
/// chat-service history pages (newest first) until `limit` files or
/// `max_pages` pages. Read-only GETs.
pub async fn chat_file_refs_data(
    client: &TeamsClient,
    chat_id: &str,
    limit: usize,
    max_pages: usize,
) -> Result<Vec<ChatFileRef>> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() || chat_id.contains(['/', '?', '#', ' ']) {
        bail!("bad chat id");
    }
    let mut url = format!(
        "{}/v1/users/ME/conversations/{}/messages?pageSize={}",
        client.chat_service_url(),
        chat_id,
        CHAT_FILES_PAGE_SIZE
    );
    let mut out: Vec<ChatFileRef> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..max_pages.max(1) {
        let page: serde_json::Value = client
            .chat_get(&url)
            .await?
            .json()
            .await
            .context("Failed to parse messages response")?;
        let (refs, back) = parse_chat_file_refs(&page);
        let empty_page = page["messages"].as_array().map_or(true, |a| a.is_empty());
        for r in refs {
            if seen.insert(r.object_url.to_lowercase()) {
                out.push(r);
            }
        }
        match back {
            Some(b) if out.len() < limit && !empty_page => {
                url = with_page_size(&b, CHAT_FILES_PAGE_SIZE)
            }
            _ => break,
        }
    }
    out.truncate(limit.max(1));
    Ok(out)
}

/// History without this view omits `properties.emotions`. The official client
/// asks for both the classic message shape and the property bag, so a like
/// that already exists on the server comes back with the page.
const MESSAGE_PROPERTIES_VIEW: &str = "msnp24Equivalent|supportsMessageProperties";

fn with_message_properties_view(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return format!("{url}?view={MESSAGE_PROPERTIES_VIEW}");
    };
    let mut parts = Vec::new();
    let mut seen = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let Some(value) = pair.strip_prefix("view=") else {
            parts.push(pair.to_owned());
            continue;
        };
        seen = true;
        let already = value.split(['|', '%']).any(|part| {
            part.eq_ignore_ascii_case("supportsMessageProperties")
                || part.eq_ignore_ascii_case("7CsupportsMessageProperties")
        });
        if already {
            parts.push(pair.to_owned());
        } else if value.is_empty() {
            parts.push(format!("view={MESSAGE_PROPERTIES_VIEW}"));
        } else {
            parts.push(format!("view={value}|supportsMessageProperties"));
        }
    }
    if !seen {
        parts.push(format!("view={MESSAGE_PROPERTIES_VIEW}"));
    }
    format!("{path}?{}", parts.join("&"))
}

fn with_page_size(url: &str, limit: usize) -> String {
    const MARK: &str = "pageSize=";
    let Some(start) = url.find(MARK) else {
        return url.to_string();
    };
    let val_start = start + MARK.len();
    let val_end = url[val_start..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| val_start + i)
        .unwrap_or(url.len());
    format!("{}{}{}", &url[..val_start], limit, &url[val_end..])
}

// ---------------------------------------------------------------------------
// Chat list actions: mute, hide, folders (OstMac chatmenu lane, §79)
// ---------------------------------------------------------------------------
//
// Mute: the per-user conversation property `alerts` on the chat service
// (`"false"` = muted, `"true"` = notify), the same property the chat list
// returns under `properties.alerts`. Hide: Graph v1.0
// `POST /chats/{id}/hideForUser` / `unhideForUser`. Folders: the chat
// service aggregator's `conversationFolders` (read-only here), which needs
// an AAD token for `https://chatsvcagg.teams.microsoft.com`.

/// PUT target for one conversation's `alerts` property.
pub fn alerts_url(base: &str, chat_id: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/properties?name=alerts",
        base, chat_id
    )
}

/// PUT body: muted chats carry `"false"` (the server stores strings).
pub fn alerts_body(muted: bool) -> serde_json::Value {
    serde_json::json!({ "alerts": if muted { "false" } else { "true" } })
}

/// Muted state from a conversation's `properties` object: `Some(true)`
/// for `alerts: "false"`, `Some(false)` for `"true"`, None when absent.
pub fn alerts_muted(properties: &serde_json::Value) -> Option<bool> {
    match properties
        .get("alerts")?
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "false" => Some(true),
        "true" => Some(false),
        _ => None,
    }
}

/// Hidden state from `properties.unpinnedTime`, the per-user property used by
/// the chat service when hiding and restoring a conversation.
pub fn conversation_hidden(properties: &serde_json::Value) -> Option<bool> {
    let value = properties.get("unpinnedTime")?;
    match value {
        serde_json::Value::Null => Some(false),
        serde_json::Value::Number(number) => number.as_i64().map(|stamp| stamp > 0),
        serde_json::Value::String(stamp) => {
            let stamp = stamp.trim();
            if stamp.is_empty() || stamp == "0" {
                Some(false)
            } else if stamp.parse::<i128>().is_ok() {
                Some(true)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Mute or unmute one chat for the signed-in user.
pub async fn set_chat_muted_with_client(
    client: &TeamsClient,
    chat_id: &str,
    muted: bool,
) -> Result<()> {
    let id = chat_id.trim();
    if id.is_empty() {
        bail!("empty chat_id");
    }
    let url = alerts_url(&client.chat_service_url(), id);
    client.chat_put(&url, &alerts_body(muted)).await?;
    Ok(())
}

// OstMac §GRAPHSWEEP3: hide/unhide go to the chat service the way the
// Teams web client's `setChatVisibility` resolver does (CDL worker):
// PUT the conversation property `unpinnedTime` (hide = now in ms, unhide
// = null) and, when hiding, `historyHiddenTime` = the same stamp as a
// string. Graph `hideForUser`/`unhideForUser` need Chat.ReadWrite, which
// the Teams token lacks (still refused by `graph_denied`).

/// PUT target for one conversation property of the signed-in user.
pub fn conversation_property_url(base: &str, chat_id: &str, name: &str) -> String {
    format!(
        "{}/v1/users/ME/conversations/{}/properties?name={}",
        base,
        chat_id.trim(),
        name
    )
}

/// Property writes, in order: hide (`Some(now_ms)`) or unhide (`None`).
/// Body is `{<name>: <value>}` like the web client's. Pure.
pub fn hide_chat_properties(hidden_at_ms: Option<u64>) -> Vec<(&'static str, serde_json::Value)> {
    match hidden_at_ms {
        Some(t) => vec![
            ("unpinnedTime", serde_json::json!({ "unpinnedTime": t })),
            (
                "historyHiddenTime",
                serde_json::json!({ "historyHiddenTime": t.to_string() }),
            ),
        ],
        None => vec![("unpinnedTime", serde_json::json!({ "unpinnedTime": null }))],
    }
}

/// Hide or unhide one chat for the signed-in user (chat service).
pub async fn set_chat_hidden_with_client(
    client: &TeamsClient,
    chat_id: &str,
    hidden: bool,
) -> Result<()> {
    let id = chat_id.trim();
    if id.is_empty() {
        bail!("empty chat_id");
    }
    let now = if hidden {
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        )
    } else {
        None
    };
    let base = client.chat_service_url();
    for (name, body) in hide_chat_properties(now) {
        client
            .chat_put(&conversation_property_url(&base, id, name), &body)
            .await?;
    }
    Ok(())
}

/// Chat service aggregator resource for folder reads.
pub const CHATSVCAGG_SCOPE: &str = "https://chatsvcagg.teams.microsoft.com/.default";

/// Folder list URL (system folders included so Favorites comes back).
pub fn conversation_folders_url() -> String {
    "https://teams.microsoft.com/api/csa/api/v1/teams/users/me/conversationFolders?supportsAdditionalSystemGeneratedFolders=true&supportsSliceItems=true".to_string()
}

/// Server folder types that are views, not folders a chat is moved into.
pub const SYSTEM_FOLDER_TYPES: &[&str] = &[
    "RecentChats",
    "TeamsAndChannels",
    "QuickViews",
    "MutedChats",
    "MeetingChats",
    "EngageCommunities",
];

/// One chat folder from the server: Favorites and user folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationFolder {
    pub id: String,
    pub name: String,
    pub folder_type: String,
    /// Conversation ids in folder order (chats and channels alike).
    pub item_ids: Vec<String>,
}

/// Parse a `conversationFolders` payload: deleted and system folders
/// dropped, blank ids dropped, `conversationFolderOrder` order first
/// (folders it omits keep payload order after it).
pub fn parse_conversation_folders(v: &serde_json::Value) -> Vec<ConversationFolder> {
    let mut out: Vec<ConversationFolder> = Vec::new();
    for f in v
        .get("conversationFolders")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        let s = |k: &str| {
            f.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (id, folder_type) = (s("id"), s("folderType"));
        if id.is_empty()
            || f.get("isDeleted")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
        {
            continue;
        }
        if SYSTEM_FOLDER_TYPES
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&folder_type))
        {
            continue;
        }
        let mut name = s("name");
        if name.is_empty() {
            name = folder_type.clone();
        }
        let item_ids = f
            .get("conversationFolderItems")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
            .filter_map(|i| i.get("conversationId").and_then(|x| x.as_str()))
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        out.push(ConversationFolder {
            id,
            name,
            folder_type,
            item_ids,
        });
    }
    let order: Vec<&str> = v
        .get("conversationFolderOrder")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str())
        .collect();
    out.sort_by_key(|f| order.iter().position(|o| *o == f.id).unwrap_or(usize::MAX));
    out
}

/// Read the signed-in user's chat folders with a chatsvcagg bearer token.
pub async fn conversation_folders_data(bearer: &str) -> Result<Vec<ConversationFolder>> {
    if bearer.trim().is_empty() {
        bail!("no chat service aggregator token");
    }
    let url = conversation_folders_url();
    let resp = super::client::shared_http()
        .get(&url)
        .bearer_auth(bearer)
        .header("x-ms-client-version", "1415/24080616421")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .context("conversationFolders GET failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("conversationFolders GET: {}", status);
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse conversationFolders")?;
    Ok(parse_conversation_folders(&v))
}

/// One folder edit: `("AddItem" | "RemoveItem", folder id, conversation id)`.
pub type FolderAction = (&'static str, String, String);

/// Pure: the edits that leave `chat_id` in `target` (blank = no folder)
/// and in no other movable folder, from a raw `conversationFolders`
/// payload. Returns `(folderHierarchyVersion, actions)`; no actions when
/// the chat is already where it should be. System folders (views) are
/// never edited; an unknown target is an error.
pub fn folder_move_actions(
    v: &serde_json::Value,
    chat_id: &str,
    target: &str,
) -> Result<(i64, Vec<FolderAction>)> {
    let version = v
        .get("folderHierarchyVersion")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let folders = parse_conversation_folders(v);
    let target = target.trim();
    if !target.is_empty() && !folders.iter().any(|f| f.id == target) {
        bail!("unknown folder");
    }
    let mut actions: Vec<FolderAction> = Vec::new();
    for f in &folders {
        let holds = f.item_ids.iter().any(|i| i == chat_id);
        if holds && f.id != target {
            actions.push(("RemoveItem", f.id.clone(), chat_id.to_string()));
        }
        if !holds && f.id == target {
            actions.push(("AddItem", f.id.clone(), chat_id.to_string()));
        }
    }
    Ok((version, actions))
}

/// Folder edit POST body (same URL as the folder GET).
pub fn folder_move_body(version: i64, actions: &[FolderAction]) -> serde_json::Value {
    let list: Vec<serde_json::Value> = actions
        .iter()
        .map(|(action, folder, item)| {
            serde_json::json!({"action": action, "folderId": folder, "itemId": item})
        })
        .collect();
    serde_json::json!({"folderHierarchyVersion": version, "actions": list})
}

async fn conversation_folders_raw(bearer: &str) -> Result<serde_json::Value> {
    let resp = super::client::shared_http()
        .get(conversation_folders_url())
        .bearer_auth(bearer)
        .header("x-ms-client-version", "1415/24080616421")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .context("conversationFolders GET failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("conversationFolders GET: {}", status);
    }
    resp.json()
        .await
        .context("Failed to parse conversationFolders")
}

/// Move one chat into `target` (blank = out of every folder): fresh GET
/// for the version and membership, one POST with the edits, then check
/// the returned folders. Returns the folders after the move; an answer
/// that does not show the chat where it was asked to go is an error.
pub async fn conversation_folder_move_with_client(
    client: &TeamsClient,
    bearer: &str,
    chat_id: &str,
    target: &str,
) -> Result<Vec<ConversationFolder>> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() || bearer.trim().is_empty() {
        bail!("missing chat id or token");
    }
    let current = conversation_folders_raw(bearer).await?;
    let (version, actions) = folder_move_actions(&current, chat_id, target)?;
    if actions.is_empty() {
        return Ok(parse_conversation_folders(&current));
    }
    let skype = client.skype_token()?;
    let resp = super::client::shared_http()
        .post(conversation_folders_url())
        .bearer_auth(bearer)
        .header("Authentication", format!("skypetoken={}", skype))
        .header("x-ms-client-version", "1415/24080616421")
        .json(&folder_move_body(version, &actions))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .context("conversationFolders POST failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("conversationFolders POST: {}", status);
    }
    // The answer carries the folder state; re-read when it does not.
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    let after = if body.get("conversationFolders").is_some() {
        body
    } else {
        conversation_folders_raw(bearer).await?
    };
    let (_, left) = folder_move_actions(&after, chat_id, target)?;
    if !left.is_empty() {
        bail!("folder move not applied");
    }
    Ok(parse_conversation_folders(&after))
}

#[cfg(test)]
mod chatmenu_tests {
    use super::*;

    fn folders_payload() -> serde_json::Value {
        serde_json::json!({
            "folderHierarchyVersion": 7,
            "conversationFolders": [
                {"id": "t~u~Favorites", "folderType": "Favorites",
                 "conversationFolderItems": [{"conversationId": "19:a@thread.v2"}]},
                {"id": "f1", "name": "Work", "folderType": "UserCreated",
                 "conversationFolderItems": [{"conversationId": "48:notes"}]},
                {"id": "q", "folderType": "QuickViews",
                 "conversationFolderItems": [{"conversationId": "48:notes"}]}
            ]
        })
    }

    #[test]
    fn folder_move_request_shape() {
        let v = folders_payload();
        let (version, actions) = folder_move_actions(&v, "48:notes", "t~u~Favorites").unwrap();
        assert_eq!(version, 7);
        // Out of the user folder, into Favorites; the QuickViews view is untouched.
        assert_eq!(
            actions,
            vec![
                (
                    "AddItem",
                    "t~u~Favorites".to_string(),
                    "48:notes".to_string()
                ),
                ("RemoveItem", "f1".to_string(), "48:notes".to_string()),
            ]
        );
        assert_eq!(
            folder_move_body(version, &actions),
            serde_json::json!({"folderHierarchyVersion": 7, "actions": [
                {"action": "AddItem", "folderId": "t~u~Favorites", "itemId": "48:notes"},
                {"action": "RemoveItem", "folderId": "f1", "itemId": "48:notes"}
            ]})
        );
        // Blank target = out of every folder; already-there = no edits.
        let (_, out) = folder_move_actions(&v, "48:notes", "").unwrap();
        assert_eq!(
            out,
            vec![("RemoveItem", "f1".to_string(), "48:notes".to_string())]
        );
        let (_, none) = folder_move_actions(&v, "19:a@thread.v2", "t~u~Favorites").unwrap();
        assert!(none.is_empty());
        // System views and unknown ids are not move targets.
        assert!(folder_move_actions(&v, "48:notes", "q").is_err());
        assert!(folder_move_actions(&v, "48:notes", "nope").is_err());
    }

    #[test]
    fn alerts_request_shape() {
        assert_eq!(
            alerts_url("https://h", "19:a@thread.v2"),
            "https://h/v1/users/ME/conversations/19:a@thread.v2/properties?name=alerts"
        );
        assert_eq!(alerts_body(true), serde_json::json!({"alerts": "false"}));
        assert_eq!(alerts_body(false), serde_json::json!({"alerts": "true"}));
        assert_eq!(
            alerts_muted(&serde_json::json!({"alerts": "false"})),
            Some(true)
        );
        assert_eq!(
            alerts_muted(&serde_json::json!({"alerts": "True"})),
            Some(false)
        );
        assert_eq!(alerts_muted(&serde_json::json!({"favorite": "true"})), None);
    }

    #[test]
    fn hidden_state_uses_only_returned_unpinned_time() {
        assert_eq!(
            conversation_hidden(&serde_json::json!({"unpinnedTime": 1727600000123u64})),
            Some(true)
        );
        assert_eq!(
            conversation_hidden(&serde_json::json!({"unpinnedTime": null})),
            Some(false)
        );
        assert_eq!(
            conversation_hidden(&serde_json::json!({"unpinnedTime": ""})),
            Some(false)
        );
        assert_eq!(conversation_hidden(&serde_json::json!({"alerts": "false"})), None);
    }

    #[test]
    fn chat_directory_mute_state_is_unknown_when_service_omits_property() {
        let muted: Conversation = serde_json::from_value(serde_json::json!({
            "id": "19:muted@thread.v2",
            "properties": {"alerts": "false"}
        }))
        .expect("conversation with alert property");
        let active: Conversation = serde_json::from_value(serde_json::json!({
            "id": "19:active@thread.v2",
            "properties": {"alerts": "true"}
        }))
        .expect("conversation with alert property");
        let unknown: Conversation = serde_json::from_value(serde_json::json!({
            "id": "19:unknown@thread.v2"
        }))
        .expect("conversation without properties");

        assert_eq!(muted.properties.as_ref().and_then(alerts_muted), Some(true));
        assert_eq!(active.properties.as_ref().and_then(alerts_muted), Some(false));
        assert_eq!(unknown.properties.as_ref().and_then(alerts_muted), None);
    }

    #[test]
    fn chat_directory_keeps_shared_picture_and_meeting_type_separate_from_user_properties() {
        let conversation: Conversation = serde_json::from_value(serde_json::json!({
            "id":"19:group@thread.v2",
            "properties":{"alerts":"false"},
            "threadProperties":{"topic":"Project", "picture":"image@https://example.test/trophy.png", "threadType":"meeting"}
        })).unwrap();
        let shared = conversation.thread_properties.unwrap();
        assert_eq!(shared.picture.as_deref(), Some("image@https://example.test/trophy.png"));
        assert_eq!(shared.thread_type.as_deref(), Some("meeting"));
        assert_eq!(conversation.properties.as_ref().and_then(alerts_muted), Some(true));
    }

    #[test]
    fn hide_request_shape() {
        // §GRAPHSWEEP3: chat-service conversation properties, not Graph.
        assert_eq!(
            conversation_property_url("https://h", " 19:a@thread.v2 ", "unpinnedTime"),
            "https://h/v1/users/ME/conversations/19:a@thread.v2/properties?name=unpinnedTime"
        );
        let hide = hide_chat_properties(Some(1727600000123));
        assert_eq!(hide.len(), 2);
        assert_eq!(
            hide[0],
            (
                "unpinnedTime",
                serde_json::json!({"unpinnedTime": 1727600000123u64})
            )
        );
        assert_eq!(
            hide[1],
            (
                "historyHiddenTime",
                serde_json::json!({"historyHiddenTime": "1727600000123"})
            )
        );
        let show = hide_chat_properties(None);
        assert_eq!(
            show,
            vec![("unpinnedTime", serde_json::json!({"unpinnedTime": null}))]
        );
        assert!(
            crate::api::client::graph_denied("POST", "/chats/19:a@thread.v2/hideForUser").is_some()
        );
    }

    #[test]
    fn folders_parse_drops_system_and_deleted_and_orders() {
        let v = serde_json::json!({
            "conversationFolderOrder": ["f-work", "f-fav", "f-recent"],
            "conversationFolders": [
                {"id": "f-fav", "name": "Favorites", "folderType": "Favorites",
                 "conversationFolderItems": [{"conversationId": "19:a@thread.v2"}, {"conversationId": " "}]},
                {"id": "f-recent", "name": "Chats", "folderType": "RecentChats", "conversationFolderItems": []},
                {"id": "f-gone", "name": "Old", "folderType": "UserCreated", "isDeleted": true},
                {"id": "f-work", "name": "Work", "folderType": "UserCreated",
                 "conversationFolderItems": [{"conversationId": "19:b@unq.gbl.spaces"}]},
                {"id": "", "name": "Blank", "folderType": "UserCreated"}
            ]
        });
        let f = parse_conversation_folders(&v);
        assert_eq!(
            f.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["f-work", "f-fav"]
        );
        assert_eq!(f[0].item_ids, ["19:b@unq.gbl.spaces"]);
        assert_eq!(f[1].item_ids, ["19:a@thread.v2"]);
        assert!(parse_conversation_folders(&serde_json::json!({})).is_empty());
        assert!(conversation_folders_url().contains("/conversationFolders?"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_preview_labels_empty_file_and_ams_image_messages() {
        let file: NativeMessage = serde_json::from_value(serde_json::json!({"messagetype":"RichText/Media_GenericFile", "content":"document.pdf"})).unwrap();
        assert_eq!(directory_preview_body(&file), "Enviou um arquivo");
        let image: NativeMessage = serde_json::from_value(serde_json::json!({"messagetype":"RichText/Html", "content":"<img itemtype='http://schema.skype.com/AMSImage' src='https://example.test/image'>"})).unwrap();
        assert_eq!(directory_preview_body(&image), "Enviou uma imagem");
    }

    #[test]
    fn directory_preview_omits_nested_quotes_but_preserves_reply_and_malformed_text() {
        assert_eq!(preview_without_quotes("<blockquote><p>Ana</p><blockquote>Older</blockquote>Quote</blockquote><p>Já estamos validando</p>"), "Já estamos validando");
        assert_eq!(preview_without_quotes("<p>Antes</p><blockquote>incompleta"), "Antes incompleta");
        assert_eq!(preview_without_quotes("<quote guid='parent'>Old quote</quote><p>Reply</p>"), "Reply");
    }

    #[test]
    fn application_metadata_retains_catalog_id_for_bot_icons() {
        for metadata in [serde_json::json!({"id":"app-id","name":"Example"}), serde_json::json!("{\"id\":\"app-id\",\"name\":\"Example\"}")] {
            let message: NativeMessage = serde_json::from_value(serde_json::json!({"properties":{"fromAppMetadata":metadata}})).unwrap();
            assert_eq!(message_application_id(&message).as_deref(), Some("app-id"));
        }
        let message: NativeMessage = serde_json::from_value(serde_json::json!({"properties":{"fromAppMetadata":"bad json"}})).unwrap();
        assert!(message_application_id(&message).is_none());
    }

    #[test]
    fn conversation_unread_marker_is_tri_state() {
        let unread: Conversation =
            serde_json::from_value(serde_json::json!({"id":"chat-1","showRedDot":true}))
                .unwrap();
        let read: Conversation =
            serde_json::from_value(serde_json::json!({"id":"chat-2","showRedDot":false}))
                .unwrap();
        let unknown: Conversation =
            serde_json::from_value(serde_json::json!({"id":"chat-3"})).unwrap();

        assert_eq!(unread.show_red_dot.as_ref().and_then(Value::as_bool), Some(true));
        assert_eq!(read.show_red_dot.as_ref().and_then(Value::as_bool), Some(false));
        assert_eq!(unknown.show_red_dot.as_ref().and_then(Value::as_bool), None);
        let unexpected: Conversation = serde_json::from_value(
            serde_json::json!({"id":"chat-4","showRedDot":"unknown"}),
        )
        .unwrap();
        assert_eq!(unexpected.show_red_dot.as_ref().and_then(Value::as_bool), None);
    }

    #[test]
    fn directory_exposes_last_message_sender_mri() {
        let conversation: Conversation = serde_json::from_value(serde_json::json!({
            "id": "chat-1",
            "lastMessage": {"from": "https://example.test/v1/users/ME/contacts/8%3Aorgid%3A11111111-1111-1111-1111-111111111111"}
        })).unwrap();
        assert_eq!(conversation_info(&conversation).unwrap().last_message_sender_mri.as_deref(),
            Some("8:orgid:11111111-1111-1111-1111-111111111111"));
    }

    #[test]
    fn one_to_one_create_shape() {
        assert_eq!(one_to_one_create_path(), "/me/chats");
        let b = one_to_one_create_body("  aad-1 ");
        assert_eq!(b["chatType"], "oneOnOne");
        let m = &b["members"][0];
        assert_eq!(
            m["@odata.type"],
            "#microsoft.graph.aadUserConversationMember"
        );
        assert_eq!(m["roles"][0], "owner");
        assert_eq!(
            m["user@odata.bind"],
            "https://graph.microsoft.com/v1.0/users('aad-1')"
        );
    }

    #[test]
    fn created_chat_parse_tolerates_shapes() {
        let v: serde_json::Value = serde_json::from_str(r#"{"id":"19:one@unq.v1"}"#).unwrap();
        let c = parse_created_chat(&v).unwrap();
        assert_eq!(c.id, "19:one@unq.v1");
        assert_eq!(c.name, "");
        assert!(!c.is_group);
        let v: serde_json::Value = serde_json::from_str(r#"{"id":"19:g@t","topic":"T"}"#).unwrap();
        assert_eq!(parse_created_chat(&v).unwrap().name, "T");
        let v: serde_json::Value = serde_json::from_str(r#"{"nope":1}"#).unwrap();
        assert!(parse_created_chat(&v).is_err());
    }

    #[test]
    fn delete_url_is_web_client_soft_delete() {
        assert_eq!(
            message_delete_url("https://h", "19:abc@thread.v2", "123"),
            "https://h/v1/users/ME/conversations/19%3Aabc%40thread.v2/messages/123?behavior=softDelete"
        );
        // Edit keeps the raw id shape (unchanged).
        assert_eq!(
            message_url("https://h", "19:chat", "123"),
            "https://h/v1/users/ME/conversations/19:chat/messages/123"
        );
    }

    #[test]
    fn edit_url_and_body_shape() {
        assert_eq!(
            message_url("https://h", "19:chat", "123"),
            "https://h/v1/users/ME/conversations/19:chat/messages/123"
        );
        let b = edit_message_body("123", "a<b>&\"'");
        assert_eq!(b["messagetype"], "RichText/Html");
        assert_eq!(b["contenttype"], "text");
        assert_eq!(b["skypeeditedid"], "123");
        assert_eq!(b["content"], "<p>a&lt;b&gt;&amp;&quot;&#39;</p>");
    }

    #[test]
    fn leave_url_and_mri_shape() {
        assert_eq!(own_member_mri("abc-123"), "8:orgid:abc-123");
        assert_eq!(own_member_mri("  abc-123  "), "8:orgid:abc-123");
        assert_eq!(
            leave_member_url("https://h", "19:t@thread.v2", "8:orgid:abc-123"),
            "https://h/v1/threads/19:t@thread.v2/members/8:orgid:abc-123"
        );
    }

    #[test]
    fn message_type_gate_keeps_cards_drops_noise() {
        // Plain + rich text always kept.
        assert!(message_type_kept("Text"));
        assert!(message_type_kept("RichText/Html"));
        // Cards kept: whole bot channels carry nothing else.
        assert!(message_type_kept("RichText/Media_Card"));
        // Call media still dropped (unreadable strips, task-0011).
        assert!(!message_type_kept("RichText/Media_CallRecording"));
        assert!(!message_type_kept("RichText/Media_CallTranscript"));
        // Unknown Media_* stays dropped (unprobed shapes).
        assert!(!message_type_kept("RichText/Media_Poll"));
        // ThreadActivity noise dropped; empty type dropped.
        assert!(!message_type_kept("ThreadActivity/AddMember"));
        assert!(!message_type_kept("ThreadActivity/DeleteMember"));
        assert!(!message_type_kept(""));
    }

    #[test]
    fn forwarded_metadata_keeps_an_empty_or_unknown_outer_message() {
        let context = ForwardedMessageContext {
            message_id: Some("original-42".into()),
            ..Default::default()
        };
        assert!(keep_message("", "", "", Some(&context)));
        assert!(!keep_message("", "", "", None));
        assert!(keep_message("RichText/Html", "texto", "", None));
    }

    #[test]
    fn forwarded_context_preserves_bounded_original_metadata() {
        let message: NativeMessage = serde_json::from_str(include_str!(
            "../../tests/fixtures/forwarded-message-context.json"
        ))
        .unwrap();
        let context = forwarded_message_context(&message).unwrap();
        assert_eq!(context.message_id.as_deref(), Some("original-42"));
        assert_eq!(
            context.sender_mri.as_deref(),
            Some("8:orgid:aaaaaaaa-1111-2222-3333-444444444444")
        );
        assert_eq!(
            context.content.as_deref(),
            Some("<p><strong>Release aprovado</strong></p>")
        );
        assert_eq!(
            context.channel_subject.as_deref(),
            Some("Publicação da versão")
        );
        assert_eq!(context.announcement, Some(true));
        assert_eq!(context.deleted, Some(false));

        let encoded = serde_json::json!({
            "properties": {
                "originalMessageContext": serde_json::json!({
                    "originalMessageId": "encoded",
                    "originalMessageContent": "a".repeat(20_000),
                    "unrecognized": "ignored"
                })
                .to_string()
            }
        });
        let message: NativeMessage = serde_json::from_value(encoded).unwrap();
        let context = forwarded_message_context(&message).unwrap();
        assert_eq!(context.message_id.as_deref(), Some("encoded"));
        assert_eq!(context.content.as_ref().unwrap().chars().count(), 16_384);
        assert_eq!(context.announcement, None);
    }

    #[test]
    fn image_tag_detection() {
        assert!(has_image(
            r#"<p><img src="https://h/v1/objects/0/views/imgo"></p>"#
        ));
        assert!(has_image(r#"<IMG SRC="https://h/x.png">"#));
        assert!(has_image(
            r#"<p>hi <img
src="x">"#
        ));
        assert!(!has_image("<p>plain text</p>"));
        assert!(!has_image("<p>image word, no tag</p>"));
        assert!(!has_image(""));
    }

    #[test]
    fn card_payload_detection() {
        // Empty placeholder attachment (server holds the card): kept.
        assert!(has_card_payload(r#"<attachment id="abc123"></attachment>"#));
        assert!(has_card_payload(
            r#"<p>digest</p><ATTACHMENT><p><a href="https://h/a">Post A</a></p></ATTACHMENT>"#
        ));
        // Card content-type markers in any casing: kept.
        assert!(has_card_payload(
            r#"{"@type":"MessageCard","title":"Build green"}"#
        ));
        assert!(has_card_payload(
            r#"<div data-contenttype="application/vnd.microsoft.card.adaptive"></div>"#
        ));
        assert!(has_card_payload("connector o365connector card"));
        // Plain text, bare images, and empty payloads: not cards.
        assert!(!has_card_payload("<p>plain text</p>"));
        assert!(!has_card_payload(r#"<p><img src="https://h/x.png"></p>"#));
        assert!(!has_card_payload(""));
        assert!(!has_card_payload("   "));
    }

    #[test]
    fn adaptive_attachment_replaces_only_the_compatibility_placeholder() {
        let card = serde_json::json!({"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"<triagem> & status"}]});
        let message: NativeMessage = serde_json::from_value(serde_json::json!({
            "content":"<p>Card - access it on https://go.skype.com/cards.unsupported. Card</p>",
            "attachments":[{}, {"contentType":"application/vnd.microsoft.card.adaptive","content":card}]
        })).unwrap();
        let html = rich_card_content(&message, message.content.as_deref().unwrap());
        assert_eq!(card_wire_shape(&message), Some(33));
        assert!(!html.contains("cards.unsupported"));
        assert!(html.contains("&lt;triagem&gt; &amp; status"));
        assert!(html.contains("contenttype=\"application/vnd.microsoft.card.adaptive\""));
        let caption = rich_card_content(&message, "<p>Confira</p>");
        assert!(caption.starts_with("<p>Confira</p>"));
        let bad: NativeMessage = serde_json::from_value(serde_json::json!({
            "attachments":[{"contentType":"application/vnd.microsoft.card.adaptive","content":"not json"}]
        })).unwrap();
        assert_eq!(rich_card_content(&bad, "placeholder"), "placeholder");
        let as_text: NativeMessage = serde_json::from_value(serde_json::json!({
            "content":"Card - access it on https://go.skype.com/cards.unsupported.",
            "attachments":[{"contentType":"application/vnd.microsoft.card.adaptive", "content":"{\"type\":\"AdaptiveCard\",\"body\":[]}"}]
        })).unwrap();
        assert!(rich_card_content(&as_text, as_text.content.as_deref().unwrap()).starts_with("<attachment"));
    }

    #[test]
    fn native_cards_property_renders_an_adaptive_card() {
        let cards = serde_json::json!([{"contentType":"application/vnd.microsoft.card.adaptive",
            "content":{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Triagem"}]}}]);
        for value in [cards.clone(), Value::String(cards.to_string())] {
            let message: NativeMessage = serde_json::from_value(serde_json::json!({
                "content":"Card - access it on https://go.skype.com/cards.unsupported. Card",
                "properties":{"cards":value}
            })).unwrap();
            let html = rich_card_content(&message, message.content.as_deref().unwrap());
            assert!(html.contains("Triagem"));
            assert!(!html.contains("cards.unsupported"));
        }
    }

    #[test]
    fn swift_uriobject_replaces_repeated_workflow_placeholder() {
        use base64::Engine;
        let payload = serde_json::json!({"attachments":[
            {"contentType":"application/vnd.microsoft.card.adaptive","content":{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Triagem"}]}}
        ]});
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload.to_string());
        let content = format!(
            "<p>Card - access it on https://go.skype.com/cards.unsupported. Card</p><URIObject type=\"SWIFT.1\"><Swift b64=\"{encoded}\"/></URIObject><p>Card - access it on https://go.skype.com/cards.unsupported. Card</p>"
        );
        let message: NativeMessage = serde_json::from_value(serde_json::json!({"content": content})).unwrap();
        let html = rich_card_content(&message, message.content.as_deref().unwrap());
        assert!(html.contains("Triagem"));
        assert!(!html.contains("cards.unsupported"));
        assert_eq!(card_wire_shape(&message).unwrap() & 32, 32);
        let broken = "<p>Card - access it on https://go.skype.com/cards.unsupported. Card</p><swift b64=\"@@@@\"/>";
        let broken_message: NativeMessage = serde_json::from_value(serde_json::json!({"content": broken})).unwrap();
        assert!(rich_card_content(&broken_message, broken).contains("cards.unsupported"));
    }

    #[test]
    fn page_size_rewrite_mid_and_end() {
        assert_eq!(
            with_page_size("https://h/m?pageSize=2&view=x", 50),
            "https://h/m?pageSize=50&view=x"
        );
        assert_eq!(
            with_page_size("https://h/m?view=x&pageSize=2", 50),
            "https://h/m?view=x&pageSize=50"
        );
        assert_eq!(with_page_size("https://h/m", 50), "https://h/m");
        assert_eq!(
            with_message_properties_view("https://h/m?pageSize=50"),
            "https://h/m?pageSize=50&view=msnp24Equivalent|supportsMessageProperties"
        );
        assert_eq!(
            with_message_properties_view(
                "https://h/m?pageSize=50&view=msnp24Equivalent|supportsMessageProperties"
            ),
            "https://h/m?pageSize=50&view=msnp24Equivalent|supportsMessageProperties"
        );
        assert_eq!(
            with_message_properties_view("https://h/m?view=msnp24Equivalent&pageSize=20"),
            "https://h/m?view=msnp24Equivalent|supportsMessageProperties&pageSize=20"
        );
    }

    #[test]
    fn reply_snippet_collapses_and_truncates() {
        assert_eq!(reply_snippet("hi"), "hi");
        assert_eq!(reply_snippet("a  b\n\tc"), "a b c");
        assert_eq!(reply_snippet("  padded  "), "padded");
        let long = "w".repeat(200);
        let snip = reply_snippet(&long);
        assert_eq!(snip.chars().count(), REPLY_SNIPPET_MAX + 1);
        assert!(snip.ends_with('…'));
        // Multibyte cut lands on a char boundary (no panic, exact width).
        let uni = "é".repeat(200);
        let usnip = reply_snippet(&uni);
        assert_eq!(usnip.chars().count(), REPLY_SNIPPET_MAX + 1);
    }

    #[test]
    fn reply_html_round_trips_through_split() {
        let html = build_reply_html("m1", "Megan Harper", "Ship <it> & go", "On it!");
        assert!(html.contains("<quote"), "{}", html);
        assert!(html.contains("&lt;it&gt; &amp; go"), "{}", html);
        let (parent, body) = split_reply_quote(&html);
        assert_eq!(parent.as_deref(), Some("m1"));
        assert_eq!(strip_html(&body).trim(), "On it!");
    }

    #[test]
    fn rich_reply_html_keeps_composer_markup_and_escapes_quote_metadata() {
        let html = build_reply_html_with_body(
            "msg-42",
            "Ana <Build>",
            "Ship & verify",
            "<p><strong>Feito</strong> &amp; enviado</p>",
        );
        assert_eq!(
            html,
            "<quote author=\"Ana &lt;Build&gt;\" guid=\"msg-42\">Ship &amp; verify</quote><p><strong>Feito</strong> &amp; enviado</p>"
        );
        let (parent, body) = split_reply_quote(&html);
        assert_eq!(parent.as_deref(), Some("msg-42"));
        assert_eq!(body, "<p><strong>Feito</strong> &amp; enviado</p>");
    }

    #[test]
    fn split_quote_rejects_malformed() {
        let (p, b) = split_reply_quote("<p>plain</p>");
        assert_eq!(p, None);
        assert_eq!(b, "<p>plain</p>");
        // Unterminated quote: keep the whole content, no parent.
        let (p, b) = split_reply_quote("<quote guid=\"m1\"><p>oops</p>");
        assert_eq!(p, None);
        assert_eq!(b, "<quote guid=\"m1\"><p>oops</p>");
        // Quote without guid still strips (body-only bubble, unknown parent).
        let (p, b) = split_reply_quote("<quote author=\"A\">old</quote><p>new</p>");
        assert_eq!(p, None);
        assert_eq!(strip_html(&b).trim(), "new");
        // Single-quoted guid parses.
        let (p, _) = split_reply_quote("<quote guid='m9'>x</quote><p>y</p>");
        assert_eq!(p.as_deref(), Some("m9"));
    }

    #[test]
    fn metadata_backward_link_parses() {
        let body: MessagesResponse = serde_json::from_str(
            r#"{"messages":[],"_metadata":{"backwardLink":"https://h/back"},"tenantId":"t"}"#,
        )
        .unwrap();
        assert_eq!(
            body.metadata.unwrap().backward_link.as_deref(),
            Some("https://h/back")
        );
        let bare: MessagesResponse = serde_json::from_str(r#"{"messages":[]}"#).unwrap();
        assert!(bare.metadata.is_none());
    }

    #[test]
    fn reaction_emoji_round_trip() {
        assert_eq!(REACTION_EMOJI.len(), 6);
        for (emoji, rtype) in REACTION_EMOJI {
            assert_eq!(reaction_type_for_emoji(emoji), Some(*rtype));
            assert_eq!(emoji_for_reaction_type(rtype), Some(*emoji));
        }
        assert_eq!(reaction_type_for_emoji("🎉"), None);
        assert_eq!(reaction_type_for_emoji(""), None);
        assert_eq!(emoji_for_reaction_type("party"), None);
        assert_eq!(emoji_for_reaction_type("LIKE"), Some("👍"));
        assert_eq!(reaction_index("like-tone4"), Some(0));
        assert_eq!(reaction_index("like-tone11"), None);
    }

    #[test]
    fn reaction_endpoint_shapes() {
        let base = "https://h";
        assert_eq!(
            reaction_add_url(base, "19:thread", "42"),
            "https://h/v1/users/ME/conversations/19:thread/messages/42/reactions"
        );
        assert_eq!(
            reaction_add_body("like"),
            serde_json::json!({ "reactionType": "like" })
        );
        assert_eq!(
            reaction_remove_url(base, "19:thread", "42", "like"),
            "https://h/v1/users/ME/conversations/19:thread/messages/42/reactions/like"
        );
    }

    fn reacted(content: &str, reactions_json: &str, via_properties: bool) -> NativeMessage {
        let payload = if via_properties {
            format!(
                r#"{{"id":"1","messagetype":"RichText/Html","content":{},"properties":{{"reactions":{}}}}}"#,
                serde_json::to_string(content).unwrap(),
                reactions_json
            )
        } else {
            format!(
                r#"{{"id":"1","messagetype":"RichText/Html","content":{},"reactions":{}}}"#,
                serde_json::to_string(content).unwrap(),
                reactions_json
            )
        };
        serde_json::from_str(&payload).unwrap()
    }

    #[test]
    fn reactions_group_in_picker_order() {
        let msg = reacted(
            "<p>hi</p>",
            r#"[{"reactionType":"laugh"},{"reactionType":"like"},{"reactionType":"like"}]"#,
            false,
        );
        assert_eq!(
            message_reactions(&msg),
            vec![
                ReactionCount {
                    emoji: "👍".to_string(),
                    count: 2,
                    reactors: vec![]
                },
                ReactionCount {
                    emoji: "😂".to_string(),
                    count: 1,
                    reactors: vec![]
                },
            ]
        );
    }

    #[test]
    fn reactions_nested_and_unknown_shapes() {
        // properties.reactions nesting works; unknown/missing types drop.
        let msg = reacted(
            "<p>hi</p>",
            r#"[{"reactionType":"Heart"},{"reactionType":"party"},{"reactionType":null},{}]"#,
            true,
        );
        assert_eq!(
            message_reactions(&msg),
            vec![ReactionCount {
                emoji: "❤️".to_string(),
                count: 1,
                reactors: vec![]
            }]
        );
        // No reactions key at all → empty, old payloads unaffected.
        let bare: NativeMessage =
            serde_json::from_str(r#"{"id":"1","content":"<p>hi</p>"}"#).unwrap();
        assert!(message_reactions(&bare).is_empty());
    }

    #[test]
    fn native_emotion_summary_count_survives_a_short_user_list() {
        let msg: NativeMessage = serde_json::from_str(
            r#"{"id":"1","properties":{"emotions":[{"key":"like","users":[{"mri":"8:orgid:user-1","displayName":"Alex"}]}],"emotionsSummary":[{"key":"like","count":5}]}}"#,
        )
        .unwrap();

        let reactions = message_reactions(&msg);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "👍");
        assert_eq!(reactions[0].count, 5);
        assert_eq!(
            reactions[0].reactors,
            vec![Reactor {
                id: "8:orgid:user-1".into(),
                name: "Alex".into()
            }]
        );
    }

    #[test]
    fn native_emotion_summary_accepts_stringified_arrays_and_user_count_fallback() {
        let msg: NativeMessage = serde_json::from_str(
            r#"{"id":"1","properties":{"emotions":"[{\"key\":\"heart\",\"users\":[{\"mri\":\"user-1\"},{\"mri\":\"user-2\"}]}]","emotionsSummary":"[{\"key\":\"heart\"}]"}}"#,
        )
        .unwrap();

        let reactions = message_reactions(&msg);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "❤️");
        assert_eq!(reactions[0].count, 2);
        assert_eq!(reactions[0].reactors.len(), 2);
    }

    #[test]
    fn graph_reactions_take_precedence_over_native_emotion_summary() {
        let msg: NativeMessage = serde_json::from_str(
            r#"{"id":"1","reactions":[{"reactionType":"like"}],"properties":{"emotions":[{"key":"heart","users":[{"mri":"user-1"}]}],"emotionsSummary":[{"key":"heart","count":9}]}}"#,
        )
        .unwrap();

        assert_eq!(
            message_reactions(&msg),
            vec![ReactionCount {
                emoji: "👍".to_owned(),
                count: 1,
                reactors: vec![]
            }]
        );
        let empty_graph: NativeMessage = serde_json::from_str(
            r#"{"id":"1","reactions":[],"properties":{"emotions":[{"key":"like","users":[{"mri":"8:orgid:user-1"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(message_reactions(&empty_graph)[0].emoji, "👍");
        let summary_only: NativeMessage = serde_json::from_str(
            r#"{"id":"1","annotationsSummary":{"emotions":{"heart":1}}}"#,
        )
        .unwrap();
        assert_eq!(message_reactions(&summary_only)[0].emoji, "❤️");
        assert_eq!(message_reactions(&summary_only)[0].count, 1);
    }

    #[test]
    fn native_tone_variants_roll_up_to_the_common_reaction() {
        let msg: NativeMessage = serde_json::from_str(
            r#"{"id":"1","properties":{"emotions":[{"key":"like-tone1","users":[{"mri":"user-1"}]},{"key":"like-tone4","users":[{"mri":"user-2"}]}],"emotionsSummary":[{"key":"like-tone1","count":2},{"key":"like-tone4","count":3}]}}"#,
        )
        .unwrap();

        let reactions = message_reactions(&msg);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "👍");
        assert_eq!(reactions[0].count, 5);
        assert_eq!(
            reactions[0]
                .reactors
                .iter()
                .map(|reactor| reactor.id.as_str())
                .collect::<Vec<_>>(),
            vec!["user-1", "user-2"]
        );
    }

    #[test]
    fn strip_html_block_boundaries_space_words() {
        // Tag-boundary glue: block boundaries separate words…
        assert_eq!(strip_html("<p>Hello</p><p>World</p>"), "Hello World");
        assert_eq!(strip_html("<div>a</div><div>b</div>"), "a b");
        assert_eq!(strip_html("a<br>b"), "a b");
        assert_eq!(strip_html("a<br/>b"), "a b");
        assert_eq!(strip_html("<ul><li>a</li><li>b</li></ul>"), "a b");
        // …but add no leading/trailing space…
        assert_eq!(strip_html("<p>hi</p>"), "hi");
        assert_eq!(strip_html(""), "");
        // …never double existing whitespace…
        assert_eq!(strip_html("<p>a</p> <p>b</p>"), "a b");
        assert_eq!(strip_html("a  <p>b"), "a  b");
        // …and leave inline tags glued.
        assert_eq!(strip_html("a<b>x</b>b"), "axb");
        assert_eq!(strip_html("<p>Hi <at id=\"8:x\">Bo</at>!</p>"), "Hi Bo!");
        // Attributes, case, and entities still handled.
        assert_eq!(strip_html("<P CLASS=\"x\">a</P><p>b</p>"), "a b");
        assert_eq!(strip_html("<p>a &amp; b</p>"), "a & b");
    }

    fn conv(json: &str) -> Conversation {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn bot_one_to_one_chats_list_like_teams() {
        // OstMac §83: live-shaped bot 1:1 rows (mychats view): the new
        // shape `19:{userOid}_{botAppId}@unq.gbl.spaces` and the legacy
        // bare bot MRI `28:{botAppId}`. Both list as 1:1s named after the
        // bot, never "[Direct message]" / a raw id.
        let c = conv(
            r#"{"id":"19:6b1f2c3d-aaaa-4bbb-8ccc-0d1e2f3a4b5c_7c8d9e0f-1111-4222-8333-444455556666@unq.gbl.spaces",
                "threadProperties":{"uniquerosterthread":"true","productThreadType":"OneToOneChat"},
                "lastMessage":{"imdisplayname":"Workflows","messagetype":"RichText/Html",
                    "from":"https://x/v1/users/ME/contacts/28:7c8d9e0f-1111-4222-8333-444455556666",
                    "content":"<div>Your approval is ready</div>"}}"#,
        );
        let id = c.id.as_deref().unwrap();
        assert!(is_onetoone_id(id));
        assert!(
            !(id.contains("thread") || id.contains("meeting")),
            "bot chat is not a group"
        );
        let mut chats=vec![conversation_info(&c).unwrap()];
        assert_eq!(chats[0].name,"[Direct message]");
        apply_peer_names(&mut chats,&HashMap::from([(id.to_owned(),"Workflows".to_owned())]));
        assert_eq!(chats[0].name,"Workflows");assert!(chats[0].peer_name_verified);
        let legacy = conv(
            r#"{"id":"28:7c8d9e0f-1111-4222-8333-444455556666",
                "lastMessage":{"imdisplayname":"Polly","messagetype":"Text","content":"Poll closed"}}"#,
        );
        let id = legacy.id.as_deref().unwrap();
        assert!(!(id.contains("thread") || id.contains("meeting")));
        assert_eq!(conversation_name(&legacy), "Polly");
    }

    #[test]
    fn conversation_name_never_raw_id() {
        // Group topics remain their explicit conversation title.
        let c = conv(
            r#"{"id":"19:t@thread.v2","threadProperties":{"topic":"Ship it"},
                "lastMessage":{"imdisplayname":"A"}}"#,
        );
        assert_eq!(conversation_name(&c), "Ship it");
        // Resolved mate beats last sender (1:1 named after self otherwise).
        let c = conv(r#"{"id":"19:a@unq.gbl.spaces","lastMessage":{"imdisplayname":"Self, Pat"}}"#);
        assert_eq!(conversation_name(&c), "[Direct message]");
        let mut chats=vec![conversation_info(&c).unwrap()];
        apply_peer_names(&mut chats,&HashMap::from([("19:a@unq.gbl.spaces".to_owned(),"Mate, Sam".to_owned())]));
        assert_eq!(chats[0].name,"Mate, Sam");
        // Blank topic/mate/sender fall through to the system label.
        let c = conv(
            r#"{"id":"19:a@unq.gbl.spaces","threadProperties":{"topic":"  "},
                "lastMessage":{"imdisplayname":""}}"#,
        );
        assert_eq!(conversation_name(&c), "[Direct message]");
        // Shape-based labels, never the id.
        for (id, want) in [
            ("19:a@unq.gbl.spaces", "[Direct message]"),
            ("19:t@thread.v2", "[Group chat]"),
            ("19:meeting_x@thread.v2", "[Meeting chat]"),
            ("48:notifications", "Notifications"),
            ("48:mentions", "Mentions"),
            ("48:notes", "Notes"),
            ("weird", "[Chat]"),
        ] {
            let c = conv(&format!(r#"{{"id":"{}"}}"#, id));
            let name = conversation_name(&c);
            assert_eq!(name, want);
            assert!(!name.contains(id), "raw id leaks: {}", name);
        }
        // Missing id entirely still labels.
        let c: Conversation = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(conversation_name(&c), "[Chat]");
    }

    #[test]
    fn mri_helpers_parse_and_match() {
        // Sender MRI = last `from` segment, percent-decoded.
        assert_eq!(
            mri_from_user_link(Some("https://h/v1/users/ME/contacts/8:orgid:abc-123")),
            "8:orgid:abc-123"
        );
        assert_eq!(
            mri_from_user_link(Some("https://h/v1/users/8%3Aorgid%3Aabc")),
            "8:orgid:abc"
        );
        assert_eq!(mri_from_user_link(None), "");
        assert_eq!(mri_from_user_link(Some("")), "");
        // Malformed % runs pass through.
        assert_eq!(percent_decode("a%2Fb%zzc%"), "a/b%zzc%");
        // Identity equality, case-insensitive; partial suffixes never match.
        assert!(mri_is_self("8:orgid:ABC-123", "abc-123"));
        assert!(mri_is_self("abc-123", "ABC-123"));
        assert!(!mri_is_self("8:orgid:abc-123", "other-oid"));
        assert!(!mri_is_self("8:orgid:abc-123", ""));
        assert!(!mri_is_self("", "abc-123"));
        assert!(!mri_is_self("8:orgid:prefix-abc-123", "abc-123"));
        // 1:1 id shapes.
        assert!(is_onetoone_id("19:a_b@unq.gbl.spaces"));
        assert!(!is_onetoone_id("19:t@thread.v2"));
        assert!(!is_onetoone_id("19:meeting_x@thread.v2"));
        assert!(!is_onetoone_id("48:notes"));
        // Roster payload parses (member MRI = `id`).
        let roster: ThreadMembersResponse = serde_json::from_str(
            r#"{"totalMemberCount":2,"members":[{"id":"8:orgid:self"},{"id":"8:orgid:mate"}],"isDeleted":false}"#,
        )
        .unwrap();
        let ids: Vec<_> = roster
            .members
            .unwrap()
            .into_iter()
            .filter_map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["8:orgid:self", "8:orgid:mate"]);
    }

    #[test]
    fn receipt_endpoint_shapes() {
        let base = "https://h";
        assert_eq!(
            consumptionhorizon_url(base, "19:t@thread.v2"),
            "https://h/v1/users/ME/conversations/19:t@thread.v2/properties?name=consumptionhorizon"
        );
        assert_eq!(
            consumptionhorizons_url(base, "19:t@thread.v2"),
            "https://h/v1/threads/19:t@thread.v2/consumptionhorizons"
        );
        assert_eq!(
            consumptionhorizon_value("m42", 1700000000000),
            "1700000000000;1700000000000;m42"
        );
        assert_eq!(
            consumptionhorizon_body("m42", 7),
            serde_json::json!({ "consumptionhorizon": "7;7;m42" })
        );
    }

    #[test]
    fn receipt_message_id_splits_last_segment() {
        assert_eq!(receipt_message_id("1;2;m42").as_deref(), Some("m42"));
        assert_eq!(receipt_message_id("m42").as_deref(), Some("m42"));
        assert_eq!(receipt_message_id(" 1;2; m42 ").as_deref(), Some("m42"));
        assert_eq!(receipt_message_id(""), None);
        assert_eq!(receipt_message_id("   "), None);
        assert_eq!(receipt_message_id("1;2;"), None);
    }

    #[test]
    fn receipt_parse_tolerates_shapes() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"id":"19:t@thread.v2","version":"1","consumptionhorizons":[
                {"mri":"8:orgid:a","consumptionhorizon":"1;2;m1"},
                {"user":"Bo","consumptionhorizon":"3;4;m2"},
                "5;6;m3",
                {"mri":"8:orgid:bad","consumptionhorizon":""},
                {"mri":"8:orgid:nohorizon"}
            ]}"#,
        )
        .unwrap();
        let out = parse_consumptionhorizons(&v);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0],
            ReadReceipt {
                user: "8:orgid:a".to_string(),
                message_id: "m1".to_string(),
                horizon: "1;2;m1".to_string(),
            }
        );
        assert_eq!(out[1].user, "Bo");
        assert_eq!(out[1].message_id, "m2");
        assert_eq!(out[2].user, "");
        assert_eq!(out[2].message_id, "m3");
        // Missing/empty lists yield empty, never panic.
        let missing: serde_json::Value = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert!(parse_consumptionhorizons(&missing).is_empty());
        let empty: serde_json::Value =
            serde_json::from_str(r#"{"consumptionhorizons":[]}"#).unwrap();
        assert!(parse_consumptionhorizons(&empty).is_empty());
    }

    fn native(json: &str) -> NativeMessage {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn channel_parent_top_level_root_message_id() {
        let m = native(
            r#"{"id":"r1","messagetype":"RichText/Html","content":"<p>reply</p>","rootMessageId":"m1"}"#,
        );
        assert_eq!(message_parent_id(&m).as_deref(), Some("m1"));
        // Graph casing variant.
        let m = native(
            r#"{"id":"r1","messagetype":"RichText/Html","content":"<p>reply</p>","replyToId":"m2"}"#,
        );
        assert_eq!(message_parent_id(&m).as_deref(), Some("m2"));
        // Lowercase wire variant.
        let m = native(
            r#"{"id":"r1","messagetype":"RichText/Html","content":"<p>reply</p>","rootmessageid":"m3"}"#,
        );
        assert_eq!(message_parent_id(&m).as_deref(), Some("m3"));
    }

    #[test]
    fn channel_parent_nested_properties_and_content() {
        // properties.replyToId nesting.
        let m = native(
            r#"{"id":"r1","messagetype":"RichText/Media_Card","content":"<p>reply</p>","properties":{"replyToId":"m9"}}"#,
        );
        assert_eq!(message_parent_id(&m).as_deref(), Some("m9"));
        // Content-embedded JSON form (Media_Card payloads).
        let m = native(
            r#"{"id":"r2","messagetype":"RichText/Media_Card","content":"{\"rootMessageId\":\"m7\",\"body\":\"hi\"}"}"#,
        );
        assert_eq!(message_parent_id(&m).as_deref(), Some("m7"));
        // Content-embedded attr form.
        assert_eq!(
            parent_id_from_content(r#"<msg rootMessageId="m5">hi</msg>"#).as_deref(),
            Some("m5")
        );
    }

    #[test]
    fn channel_parent_missing_is_none_never_crash() {
        let m = native(r#"{"id":"m1","content":"<p>plain</p>"}"#);
        assert_eq!(message_parent_id(&m), None);
        // Empty / null / numeric-adjacent shapes.
        let m = native(r#"{"id":"m1","content":"<p>x</p>","rootMessageId":"  "}"#);
        assert_eq!(message_parent_id(&m), None);
        let m = native(r#"{"id":"m1","content":"<p>x</p>","rootMessageId":null}"#);
        assert_eq!(message_parent_id(&m), None);
        assert_eq!(parent_id_from_content(""), None);
        assert_eq!(parent_id_from_content("<p>no keys here</p>"), None);
        assert_eq!(parent_id_from_content(r#"rootMessageId="m1"#), None);
    }

    // wire-pre lane: fenced blocks go out as <pre> (other clients keep
    // indents); prose keeps the legacy single-<p> shape bit-identical.

    #[test]
    fn wire_prose_unchanged_single_p() {
        assert_eq!(build_message_html("hi"), "<p>hi</p>");
        assert_eq!(
            build_message_html("a<b>&\"'\nline2  indented"),
            "<p>a&lt;b&gt;&amp;&quot;&#39;\nline2  indented</p>"
        );
        // Inline backticks and short runs are not fences.
        assert_eq!(build_message_html("use `x` here"), "<p>use `x` here</p>");
        assert_eq!(build_message_html("a\n``\nb"), "<p>a\n``\nb</p>");
        // Info string containing the fence char: not a fence (CommonMark).
        assert_eq!(
            build_message_html("``` `x` ```\nstill prose"),
            "<p>``` `x` ```\nstill prose</p>"
        );
    }

    #[test]
    fn wire_fenced_block_byte_exact_pre() {
        // Captured wire body: exact JSON `chat_post` receives.
        let body = send_message_body("```swift\nlet x  =  1\n\tindented\n```");
        assert_eq!(body["messagetype"], "RichText/Html");
        assert_eq!(body["contenttype"], "text");
        assert_eq!(body["content"], "<pre>let x  =  1\n\tindented</pre>");
        // Escaping still applies inside <pre>; fences + info consumed.
        let body = send_message_body("```\na<b>&\"'\n```");
        assert_eq!(body["content"], "<pre>a&lt;b&gt;&amp;&quot;&#39;</pre>");
        // ~~~ fences + indented fence lines work.
        let body = send_message_body("  ~~~py\nx = 1\n  ~~~");
        assert_eq!(body["content"], "<pre>x = 1</pre>");
    }

    #[test]
    fn rich_editor_body_keeps_semantic_html_and_native_message_headers() {
        let body = send_message_html_body(
            "<p><strong>Olá</strong> <a href=\"https://example.com\">equipe</a></p>",
        );
        assert_eq!(body["messagetype"], "RichText/Html");
        assert_eq!(body["contenttype"], "text");
        assert_eq!(
            body["content"],
            "<p><strong>Olá</strong> <a href=\"https://example.com\">equipe</a></p>"
        );
    }

    #[test]
    fn wire_mixed_prose_and_code_segments() {
        assert_eq!(
            build_message_html("hi\n```\ncode  x\n```\nbye"),
            "<p>hi</p><pre>code  x</pre><p>bye</p>"
        );
        // Longer runs need equally long closers; inner short runs stay code.
        assert_eq!(
            build_message_html("````\n```\ninner\n```\n````"),
            "<pre>```\ninner\n```</pre>"
        );
    }

    #[test]
    fn wire_unclosed_fence_runs_to_end() {
        // Swift CodeBlocks parity: mid-typing states stay code.
        assert_eq!(
            build_message_html("note\n```\nline1\nline2"),
            "<p>note</p><pre>line1\nline2</pre>"
        );
    }

    #[test]
    fn wire_fence_cap_leaves_extras_prose() {
        let mut msg = String::new();
        for i in 0..(WIRE_FENCE_MAX_BLOCKS + 1) {
            msg.push_str(&format!("```\nc{}\n```\n", i));
        }
        let html = build_message_html(&msg);
        assert_eq!(html.matches("<pre>").count(), WIRE_FENCE_MAX_BLOCKS);
        // 51st block never parsed: its fences stay literal prose, nothing lost.
        assert!(html.contains("```\nc50\n```"), "{}", html);
    }

    #[test]
    fn wire_reply_and_edit_carry_pre() {
        let html = build_reply_html("m1", "A", "parent", "```\ncode\n```");
        assert!(html.starts_with("<quote"), "{}", html);
        assert!(html.contains("<pre>code</pre>"), "{}", html);
        // Prose reply keeps the legacy quote+<p> shape.
        assert_eq!(
            build_reply_html("m1", "A", "p", "On it!"),
            "<quote author=\"A\" guid=\"m1\">p</quote><p>On it!</p>"
        );
        let b = edit_message_body("123", "```\ncode\n```");
        assert_eq!(b["content"], "<pre>code</pre>");
        assert_eq!(b["messagetype"], "RichText/Html");
        assert_eq!(b["skypeeditedid"], "123");
    }
}

#[cfg(test)]
mod chat_files_tests {
    use super::*;

    #[test]
    fn message_file_refs_single_and_page() {
        let files = r#"[{"id":"att-1","fileName":"Plan.docx","objectUrl":"https://c.sharepoint.com/Plan.docx","fileInfo":{"shareUrl":"https://c.sharepoint.com/:w:/s/x"}}]"#;
        let msg = serde_json::json!({"id":"1700000000500","imdisplayname":"Ava Hart",
            "originalarrivaltime":"2026-09-28T10:00:00Z","properties":{"files":files}});
        let refs = message_file_refs(&msg, "1700000000500").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].attachment_id.as_deref(), Some("att-1"));
        assert_eq!(
            refs[0].share_url.as_deref(),
            Some("https://c.sharepoint.com/:w:/s/x")
        );
        assert!(message_file_refs(&msg, "999").is_none());
        let page = serde_json::json!({"messages":[
            {"id":"1700000000900","properties":{}}, msg.clone(), {"id":"1700000000100"}]});
        assert_eq!(message_file_refs(&page, "1700000000500").unwrap().len(), 1);
        assert!(message_file_refs(&page, "1700000000400").is_none());
        // A message without files yields an empty list, not a miss.
        assert_eq!(message_file_refs(&page, "1700000000900").unwrap().len(), 0);
        // History walk stops once a page reaches the message's time.
        assert!(page_reaches(&page, "1700000000400"));
        let newer = serde_json::json!({"messages":[{"id":"1700000000900"}]});
        assert!(!page_reaches(&newer, "1700000000400"));
    }

    #[test]
    fn native_message_projection_reads_file_references_from_properties() {
        let files = r#"[{"id":"att-9","fileName":"Roadmap.pdf","fileType":"pdf","objectUrl":"https://files.example/Roadmap.pdf"}]"#;
        let message: NativeMessage = serde_json::from_value(serde_json::json!({
            "id": "1700000000777",
            "imdisplayname": "Sam Lee",
            "originalarrivaltime": "2026-09-28T10:00:00Z",
            "messagetype": "RichText/Html",
            "content": "<p>Roadmap.pdf</p>",
            "properties": { "files": files }
        }))
        .unwrap();

        let refs = native_message_file_refs(
            &message,
            "1700000000777",
            "Sam Lee",
            "2026-09-28T10:00:00Z",
        );
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "Roadmap.pdf");
        assert_eq!(refs[0].file_type.as_deref(), Some("pdf"));
        assert_eq!(refs[0].sender.as_deref(), Some("Sam Lee"));
        assert_eq!(refs[0].object_url, "https://files.example/Roadmap.pdf");

        let deleted: NativeMessage = serde_json::from_value(serde_json::json!({
            "id": "1700000000778",
            "properties": { "deletetime": "1", "files": files }
        }))
        .unwrap();
        assert!(native_message_file_refs(&deleted, "x", "", "").is_empty());
    }

    #[test]
    fn file_refs_from_chat_service_page() {
        let files = serde_json::json!([
            {"id": "att-1", "fileName": "Plan.docx", "fileType": "docx",
             "objectUrl": "https://contoso-my.sharepoint.com/personal/a/Documents/Microsoft Teams Chat Files/Plan.docx",
             "fileInfo": {"shareUrl": "https://contoso-my.sharepoint.com/:w:/g/personal/a/xyz"}},
            {"id": "att-2", "title": "Old.xlsx", "state": "deleted", "objectUrl": "https://x/Old.xlsx"}
        ])
        .to_string();
        let page = serde_json::json!({
            "messages": [
                {"imdisplayname": "Alex Carter", "originalarrivaltime": "2026-09-28T09:00:00Z",
                 "properties": {"files": files}},
                {"imdisplayname": "Jamie Brooks", "properties": {"deletetime": "1700000000000",
                 "files": [{"id": "gone", "objectUrl": "https://x/Gone.pdf"}]}},
                {"imdisplayname": "Jamie Brooks", "composetime": "2026-09-27T08:00:00Z",
                 "properties": {"files": [{"id": "att-3", "fileInfo": {"fileUrl": "https://x/Budget.pptx"}}]}},
                {"content": "no files here"}
            ],
            "_metadata": {"backwardLink": "https://svc/v1/users/ME/conversations/c/messages?startTime=1&pageSize=200"}
        });
        let (refs, back) = parse_chat_file_refs(&page);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "Plan.docx");
        assert_eq!(refs[0].attachment_id.as_deref(), Some("att-1"));
        assert_eq!(
            refs[0].share_url.as_deref(),
            Some("https://contoso-my.sharepoint.com/:w:/g/personal/a/xyz")
        );
        assert_eq!(refs[0].sender.as_deref(), Some("Alex Carter"));
        assert_eq!(refs[0].time.as_deref(), Some("2026-09-28T09:00:00Z"));
        assert_eq!(refs[1].name, "Budget.pptx");
        assert_eq!(refs[1].object_url, "https://x/Budget.pptx");
        assert_eq!(refs[1].time.as_deref(), Some("2026-09-27T08:00:00Z"));
        assert!(back.unwrap().contains("startTime=1"));
        let (none, no_back) = parse_chat_file_refs(&serde_json::json!({}));
        assert!(none.is_empty() && no_back.is_none());
    }
}

// ---------------------------------------------------------------------------
// Pinned messages (OstMac §84)
// ---------------------------------------------------------------------------

/// One server-side pinned chat message. `graph_pin_id` is set only for
/// Graph-sourced pins (the `pinnedChatMessageInfo` id Graph DELETE
/// takes); chat-service pins carry the message id alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PinnedRef {
    pub message_id: String,
    pub sender: Option<String>,
    pub preview: Option<String>,
    /// Message arrival time (ISO 8601).
    pub time: Option<String>,
    pub pinned_by: Option<String>,
    pub pinned_at: Option<String>,
    pub graph_pin_id: Option<String>,
}

/// Where a chat's pins came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    ChatService,
    Graph,
}

/// Chat-service thread GET (properties carry the thread's state).
pub fn thread_pins_url(base: &str, chat_id: &str) -> String {
    format!(
        "{}/v1/threads/{}?view=msnp24Equivalent",
        base,
        chat_id.trim()
    )
}

/// Graph `GET /chats/{id}/pinnedMessages?$expand=message` (needs
/// Chat.Read: expect 403 on the Teams web token).
pub fn graph_pins_path(chat_id: &str) -> String {
    format!("/chats/{}/pinnedMessages?$expand=message", chat_id.trim())
}

/// Graph `DELETE /chats/{id}/pinnedMessages/{pinId}`.
pub fn graph_unpin_path(chat_id: &str, pin_id: &str) -> String {
    format!("/chats/{}/pinnedMessages/{}", chat_id.trim(), pin_id.trim())
}

/// Scalar JSON (string or number) as a trimmed non-empty string.
fn pin_scalar(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// First present scalar among `keys` (case-insensitive).
fn pin_field(o: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| pin_scalar(obj_get_ci(o, k)))
}

/// Plain one-line preview from message HTML/text.
fn pin_preview(content: &str) -> Option<String> {
    let text = strip_html(content);
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(one).filter(|s| !s.is_empty())
}

/// A bare id from a string list: chat-service message ids are arrival
/// ms, so only all-digit tokens count (flags like "true" never pin).
fn bare_pin_id(s: &str) -> Option<String> {
    let t = s.trim().trim_matches('"').trim();
    (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.to_string())
}

fn collect_thread_pins(v: &serde_json::Value, depth: usize, out: &mut Vec<PinnedRef>) {
    if depth > 4 {
        return;
    }
    match v {
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.starts_with('[') || t.starts_with('{') {
                if let Ok(inner) = serde_json::from_str::<serde_json::Value>(t) {
                    collect_thread_pins(&inner, depth + 1, out);
                    return;
                }
            }
            out.extend(
                t.split(',')
                    .filter_map(bare_pin_id)
                    .map(|message_id| PinnedRef {
                        message_id,
                        ..Default::default()
                    }),
            );
        }
        serde_json::Value::Number(n) => out.push(PinnedRef {
            message_id: n.to_string(),
            ..Default::default()
        }),
        serde_json::Value::Array(a) => {
            for e in a {
                collect_thread_pins(e, depth + 1, out);
            }
        }
        serde_json::Value::Object(o) => {
            let id = pin_field(o, &["messageId", "messageid", "id"])
                .filter(|s| !s.contains(['/', '?', '#', ' ']));
            match id {
                Some(message_id) => out.push(PinnedRef {
                    message_id,
                    sender: pin_field(o, &["sender", "imdisplayname", "senderDisplayName"]),
                    preview: pin_field(o, &["content", "preview", "messagePreview"])
                        .and_then(|c| pin_preview(&c)),
                    time: pin_field(o, &["originalarrivaltime", "composetime", "messageTime"]),
                    pinned_by: pin_field(o, &["pinnedBy", "pinnedby"]),
                    pinned_at: pin_field(o, &["pinnedTime", "pinnedAt", "pinnedDateTime"]),
                    graph_pin_id: None,
                }),
                None => {
                    // Id-less wrapper (e.g. `{pins:[…]}`): descend into
                    // containers only — its scalars (times, flags) are
                    // never ids.
                    for inner in o.values().filter(|v| v.is_array() || v.is_object()) {
                        collect_thread_pins(inner, depth + 1, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Pins from a chat-service thread body (`GET /v1/threads/{id}`). The
/// shape is undocumented, so this is tolerant: every `properties` key
/// whose lowercase name contains "pinned" is read, its value a JSON
/// string, an array, or comma-separated ids; object entries take
/// `messageId|id` (+ optional pinnedBy/pinnedTime/sender/content).
/// Deduplicated by message id, first wins. Pure (no network).
pub fn parse_thread_pins(value: &serde_json::Value) -> Vec<PinnedRef> {
    let props = value
        .get("properties")
        .and_then(|p| p.as_object())
        .or_else(|| value.as_object());
    let mut out = Vec::new();
    if let Some(props) = props {
        for (k, v) in props {
            if k.to_lowercase().contains("pinned") {
                collect_thread_pins(v, 0, &mut out);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.message_id.clone()));
    out
}

/// Pins from a Graph `pinnedMessages?$expand=message` body. Entries
/// without a message id are skipped. Pure (no network).
pub fn parse_graph_pins(value: &serde_json::Value) -> Result<Vec<PinnedRef>> {
    let list = value
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("pinned messages response has no value[]"))?;
    let mut out = Vec::new();
    for e in list {
        let msg = &e["message"];
        let Some(message_id) = pin_scalar(msg.get("id")) else {
            continue;
        };
        if msg["deletedDateTime"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            continue;
        }
        let sender = pin_scalar(msg["from"]["user"].get("displayName"))
            .or_else(|| pin_scalar(msg["from"]["application"].get("displayName")));
        out.push(PinnedRef {
            message_id,
            sender,
            preview: msg["body"]["content"].as_str().and_then(pin_preview),
            time: pin_scalar(msg.get("createdDateTime")),
            pinned_by: None,
            pinned_at: None,
            graph_pin_id: pin_scalar(e.get("id")),
        });
    }
    Ok(out)
}

/// Fill a thread pin's missing sender/preview/time from its chat-service
/// message (`GET …/messages/{id}`). Present fields are kept. Pure.
pub fn fill_pin_from_message(pin: &mut PinnedRef, msg: &serde_json::Value) {
    if pin.sender.is_none() {
        pin.sender = pin_scalar(msg.get("imdisplayname"));
    }
    if pin.preview.is_none() {
        pin.preview = msg["content"].as_str().and_then(pin_preview);
    }
    if pin.time.is_none() {
        pin.time = pin_scalar(msg.get("originalarrivaltime"))
            .or_else(|| pin_scalar(msg.get("composetime")));
    }
}

/// Source precedence: non-empty chat-service pins, else Graph pins,
/// else an empty chat-service read (authoritative "no pins"), else the
/// chat-service error (Graph's 403 is the expected, uninformative one).
/// `graph` is `None` when it was never tried. Pure.
pub fn pick_pins(
    thread: Result<Vec<PinnedRef>>,
    graph: Option<Result<Vec<PinnedRef>>>,
) -> Result<(PinSource, Vec<PinnedRef>)> {
    match (thread, graph) {
        (Ok(t), _) if !t.is_empty() => Ok((PinSource::ChatService, t)),
        (_, Some(Ok(g))) => Ok((PinSource::Graph, g)),
        (Ok(t), _) => Ok((PinSource::ChatService, t)),
        (Err(e), Some(Err(g))) => Err(e.context(format!(
            "graph pinnedMessages fallback also failed: {:#}",
            g
        ))),
        (Err(e), None) => Err(e),
    }
}

/// OstMac §84: a chat's server-side pinned messages from the chat service
/// thread properties (skypetoken; shape undocumented, parsed tolerantly).
/// §GRAPHSWEEP: the Graph `pinnedMessages` fallback is gone (needs
/// Chat.Read — 403 on the Teams web token). Thread pins missing a preview are filled from their
/// chat-service message (failure leaves them `None`). GETs only; the
/// consumption horizon never moves.
pub async fn chat_pinned_messages_data(
    client: &TeamsClient,
    chat_id: &str,
) -> Result<(PinSource, Vec<PinnedRef>)> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() || chat_id.contains(['/', '?', '#', ' ']) {
        bail!("bad chat id");
    }
    let base = client.chat_service_url();
    let thread = async {
        let v: serde_json::Value = client
            .chat_get(&thread_pins_url(&base, chat_id))
            .await?
            .json()
            .await
            .context("Failed to parse thread response")?;
        Ok::<_, anyhow::Error>(parse_thread_pins(&v))
    }
    .await;
    // §GRAPHSWEEP: no Graph `pinnedMessages` fallback (Chat.Read is not on
    // the Teams token: a guaranteed 403 per open).
    let (source, mut pins) = pick_pins(thread, None)?;
    if source == PinSource::ChatService {
        for pin in pins
            .iter_mut()
            .filter(|p| p.preview.is_none() || p.sender.is_none())
        {
            if pin.message_id.contains(['/', '?', '#', ' ']) {
                continue;
            }
            let url = message_url(&base, chat_id, &pin.message_id);
            if let Ok(resp) = client.chat_get(&url).await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    fill_pin_from_message(pin, &v);
                }
            }
        }
    }
    Ok((source, pins))
}

/// OstMac §84: unpin one Graph-sourced pin. §GRAPHSWEEP: Graph pins can
/// no longer load (the read needs Chat.Read, 403 on the Teams token) and
/// Graph `DELETE /chats/{id}/pinnedMessages/{pinId}` needs
/// ChatMessage.ReadWrite/Chat.ReadWrite (not granted either), so this
/// fails without any network. Blank/unsafe ids are rejected first.
pub async fn chat_unpin_message_with_client(
    _client: &TeamsClient,
    chat_id: &str,
    pin_id: &str,
) -> Result<()> {
    let bad = |s: &str| s.trim().is_empty() || s.trim().contains(['/', '?', '#', ' ']);
    if bad(chat_id) || bad(pin_id) {
        bail!("bad chat or pin id");
    }
    bail!("Server unpin is not available with the Teams sign-in (Graph Chat.ReadWrite not granted)")
}

#[cfg(test)]
mod roster_name_tests {
    use super::*;

    fn member(mri: &str, name: &str) -> ChatMemberInfo {
        ChatMemberInfo {
            mri: mri.to_string(),
            user_id: oid_from_orgid_mri(mri),
            display_name: name.to_string(),
            email: None,
            roles: vec![],
            is_owner: false,
        }
    }

    /// §GRAPHSWEEP: blank chat-service names fill from Graph /users/{oid};
    /// present names win; a missing answer leaves the member as it was.
    #[test]
    fn roster_blank_names_fill_from_users() {
        assert_eq!(
            roster_user_path(" 11111111-aaaa-4aaa-8aaa-111111111111 "),
            "/users/11111111-aaaa-4aaa-8aaa-111111111111?$select=displayName,mail,userPrincipalName"
        );
        let mut m = member("8:orgid:11111111-aaaa-4aaa-8aaa-111111111111", "");
        apply_roster_user(
            &mut m,
            &serde_json::json!({"displayName": "Ava Stone", "mail": null, "userPrincipalName": "ava@example.com"}),
        );
        assert_eq!(m.display_name, "Ava Stone");
        assert_eq!(m.email.as_deref(), Some("ava@example.com"));
        let mut named = member("8:orgid:11111111-aaaa-4aaa-8aaa-111111111111", "Kept Name");
        apply_roster_user(
            &mut named,
            &serde_json::json!({"displayName": "Other", "mail": "k@example.com"}),
        );
        assert_eq!(named.display_name, "Kept Name");
        assert_eq!(named.email.as_deref(), Some("k@example.com"));
        let mut blank = member("8:orgid:11111111-aaaa-4aaa-8aaa-111111111111", "");
        apply_roster_user(&mut blank, &serde_json::json!({"displayName": "  "}));
        assert_eq!(blank.display_name, "");
        assert_eq!(blank.email, None);
    }
}

#[cfg(test)]
mod pinned_tests {
    use super::*;

    #[test]
    fn thread_pins_from_json_string_property() {
        let pins = serde_json::json!([
            {"messageId": "1727000000100", "pinnedBy": "8:orgid:aaa", "pinnedTime": "1727000000900"},
            {"id": 1727000000200u64, "content": "<p>Ship <b>Friday</b></p>", "imdisplayname": "Ava Hart"},
            {"messageId": "1727000000100"}
        ])
        .to_string();
        let thread = serde_json::json!({
            "id": "19:a@thread.v2", "type": "Thread",
            "properties": {"topic": "Launch", "pinnedMessages": pins, "ispinned": "true"}
        });
        let got = parse_thread_pins(&thread);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].message_id, "1727000000100");
        assert_eq!(got[0].pinned_by.as_deref(), Some("8:orgid:aaa"));
        assert_eq!(got[0].pinned_at.as_deref(), Some("1727000000900"));
        assert!(got[0].preview.is_none() && got[0].graph_pin_id.is_none());
        assert_eq!(got[1].message_id, "1727000000200");
        assert_eq!(got[1].preview.as_deref(), Some("Ship Friday"));
        assert_eq!(got[1].sender.as_deref(), Some("Ava Hart"));
    }

    #[test]
    fn thread_pins_from_array_and_csv_and_absent() {
        let arr = serde_json::json!({"properties": {"PinnedMessages": [
            {"messageid": "1727000000300", "sender": "Jamie Brooks"}, "1727000000400"]}});
        let ids: Vec<_> = parse_thread_pins(&arr)
            .into_iter()
            .map(|p| p.message_id)
            .collect();
        assert_eq!(ids, ["1727000000300", "1727000000400"]);
        let csv =
            serde_json::json!({"properties": {"pinnedmessages": "1727000000500, 1727000000600,"}});
        assert_eq!(parse_thread_pins(&csv).len(), 2);
        let none = serde_json::json!({"properties": {"topic": "x", "alerts": "true"}});
        assert!(parse_thread_pins(&none).is_empty());
        assert!(parse_thread_pins(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn graph_pins_parse_and_fill_from_message() {
        let v = serde_json::json!({"value": [
            {"id": "pin-1", "message": {"id": "1727000000700", "createdDateTime": "2026-09-28T10:00:00Z",
             "from": {"user": {"displayName": "Alex Carter"}},
             "body": {"contentType": "html", "content": "<p>Budget  due</p>"}}},
            {"id": "pin-2", "message": {"id": "1727000000800", "deletedDateTime": "2026-09-28T11:00:00Z"}},
            {"id": "pin-3"}
        ]});
        let got = parse_graph_pins(&v).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].graph_pin_id.as_deref(), Some("pin-1"));
        assert_eq!(got[0].sender.as_deref(), Some("Alex Carter"));
        assert_eq!(got[0].preview.as_deref(), Some("Budget due"));
        assert!(parse_graph_pins(&serde_json::json!({"error": {"code": "Forbidden"}})).is_err());

        let mut p = PinnedRef {
            message_id: "1".into(),
            sender: Some("Kept".into()),
            ..Default::default()
        };
        fill_pin_from_message(
            &mut p,
            &serde_json::json!({"imdisplayname": "Other",
            "content": "<div>hi there</div>", "originalarrivaltime": "2026-09-28T09:00:00Z"}),
        );
        assert_eq!(p.sender.as_deref(), Some("Kept"));
        assert_eq!(p.preview.as_deref(), Some("hi there"));
        assert_eq!(p.time.as_deref(), Some("2026-09-28T09:00:00Z"));
    }

    #[test]
    fn source_precedence_and_graph_403() {
        let pin = |id: &str| PinnedRef {
            message_id: id.into(),
            ..Default::default()
        };
        let forbidden = || {
            Err(anyhow::anyhow!(
                "HTTP 403 Forbidden: Missing scope permissions"
            ))
        };
        // Thread pins win; Graph untried.
        let (s, p) = pick_pins(Ok(vec![pin("1")]), None).unwrap();
        assert_eq!((s, p.len()), (PinSource::ChatService, 1));
        // Empty thread + Graph 403 = no pins, not an error.
        let (s, p) = pick_pins(Ok(vec![]), Some(forbidden())).unwrap();
        assert_eq!((s, p.len()), (PinSource::ChatService, 0));
        // Thread failure falls back to Graph.
        let (s, p) = pick_pins(
            Err(anyhow::anyhow!("chatsvc 500")),
            Some(Ok(vec![pin("2")])),
        )
        .unwrap();
        assert_eq!((s, p[0].message_id.as_str()), (PinSource::Graph, "2"));
        // Both fail: the chat-service error leads.
        let e = pick_pins(Err(anyhow::anyhow!("chatsvc 500")), Some(forbidden())).unwrap_err();
        let msg = format!("{:#}", e);
        assert!(
            msg.contains("chatsvc 500") && msg.contains("403"),
            "{}",
            msg
        );
        assert_eq!(e.root_cause().to_string(), "chatsvc 500");
    }

    #[test]
    fn pin_paths() {
        assert_eq!(
            thread_pins_url("https://h", " 19:a@thread.v2 "),
            "https://h/v1/threads/19:a@thread.v2?view=msnp24Equivalent"
        );
        assert_eq!(
            graph_pins_path("19:a@thread.v2"),
            "/chats/19:a@thread.v2/pinnedMessages?$expand=message"
        );
        assert_eq!(
            graph_unpin_path("19:a@thread.v2", " pin-1 "),
            "/chats/19:a@thread.v2/pinnedMessages/pin-1"
        );
    }
}

#[cfg(test)]
mod sendfix_tests {
    use super::*;

    #[test]
    fn sendfix_one_to_one_thread_id_is_ordered_pair() {
        let a = "527A0000-0000-0000-0000-000000000001";
        let b = "01d90000-0000-0000-0000-000000000002";
        let want = "19:01d90000-0000-0000-0000-000000000002_527a0000-0000-0000-0000-000000000001@unq.gbl.spaces";
        assert_eq!(one_to_one_thread_id(a, b), want);
        assert_eq!(one_to_one_thread_id(b, a), want);
        assert!(looks_like_guid(b));
        assert!(!looks_like_guid("someone@example.org"));
    }

    #[test]
    fn sendfix_client_message_id_is_19_digits_and_fresh() {
        let a = new_client_message_id();
        let b = new_client_message_id();
        assert_eq!(a.len(), 19);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(a, b);
    }

    #[test]
    fn sendfix_sent_id_prefers_location_then_arrival_time() {
        let loc = "https://h/v1/users/ME/conversations/19:a@thread.v2/messages/1727540406676";
        assert_eq!(
            sent_id_from_response(Some(loc), "").as_deref(),
            Some("1727540406676")
        );
        assert_eq!(
            sent_id_from_response(None, r#"{"OriginalArrivalTime":1727540406677}"#).as_deref(),
            Some("1727540406677")
        );
        assert_eq!(
            sent_id_from_response(
                Some("https://h/x/messages"),
                r#"{"originalarrivaltime":"42"}"#
            )
            .as_deref(),
            Some("42")
        );
        assert_eq!(sent_id_from_response(None, "{}"), None);
        assert_eq!(sent_id_from_response(None, "not json"), None);
    }

    #[test]
    fn sendfix_history_rows_carry_client_message_id() {
        let with: NativeMessage =
            serde_json::from_str(r#"{"id":"2","ClientMessageId":"555","content":"<p>b</p>"}"#)
                .unwrap();
        let num: NativeMessage =
            serde_json::from_str(r#"{"id":"3","clientmessageid":556}"#).unwrap();
        let without: NativeMessage = serde_json::from_str(r#"{"id":"1"}"#).unwrap();
        assert_eq!(client_message_id_of(&with.extra).as_deref(), Some("555"));
        assert_eq!(client_message_id_of(&num.extra).as_deref(), Some("556"));
        assert_eq!(client_message_id_of(&without.extra), None);
        let row = |id: &str, c: Option<&str>| MessageInfo {
            reaction_properties: None,
            mention_properties: None,
            card_wire_shape: None,
            id: id.into(),
            sender_mri: String::new(),
            application_id: None,
            sender: "A".into(),
            timestamp: String::new(),
            content: "x".into(),
            raw: String::new(),
            reactions: vec![],
            reply_to: None,
            client_message_id: c.map(String::from),
            forwarded: None,
            files: vec![],
            importance: MessageImportance::Normal,
            edited: false,
        };
        let rows = vec![row("1", None), row("2", Some("555"))];
        assert_eq!(
            find_by_client_id(&rows, "555").map(|m| m.id.as_str()),
            Some("2")
        );
        assert!(find_by_client_id(&rows, "556").is_none());
        assert!(find_by_client_id(&rows, " ").is_none());
    }

    #[test]
    fn message_importance_keeps_high_and_urgent_and_the_edit_marker() {
        let high: NativeMessage = serde_json::from_str(
            r#"{"id":"1","properties":{"importance":"high","skypeeditedid":"1"}}"#,
        )
        .unwrap();
        assert_eq!(message_importance(&high), MessageImportance::High);
        assert!(message_was_edited(&high));
        let urgent: NativeMessage =
            serde_json::from_str(r#"{"id":"2","importance":"Urgent"}"#).unwrap();
        assert_eq!(message_importance(&urgent), MessageImportance::Urgent);
        assert!(!message_was_edited(&urgent));
        let normal: NativeMessage =
            serde_json::from_str(r#"{"id":"3","properties":{"importance":"normal"}}"#).unwrap();
        assert_eq!(message_importance(&normal), MessageImportance::Normal);
        let edited: NativeMessage =
            serde_json::from_str(r#"{"id":"4","properties":{"edittime":1730000000000}}"#).unwrap();
        assert!(message_was_edited(&edited));
        assert_eq!(message_importance(&edited), MessageImportance::Normal);
    }
}

/// FIXPACK F8: batched + cached 1:1 mate names (fake transport, no tenant).
#[cfg(test)]
mod mate_batch_tests {
    use super::*;
    use crate::api::fake_transport::Fake;
    use serde_json::json;
    use std::sync::Mutex;

    const ME: &str = "aaaaaaaa-0000-0000-0000-000000000001";

    fn routes(ids: &[(&'static str, &str, &str)]) -> Vec<crate::api::fake_transport::Route> {
        let mut r = Vec::new();
        for (id, mate_oid, name) in ids {
            r.push((
                "GET",
                Box::leak(format!("/chat/v1/threads/{id}/members").into_boxed_str()) as &'static str,
                200,
                json!({"members": [{"id": format!("8:orgid:{ME}")}, {"id": format!("8:orgid:{mate_oid}")}]}).to_string(),
            ));
            r.push((
                "GET",
                Box::leak(format!("/graph/users/{mate_oid}").into_boxed_str()) as &'static str,
                200,
                json!({"id":mate_oid, "displayName":name}).to_string(),
            ));
        }
        r
    }

    fn ids() -> Vec<String> {
        vec![
            "19:one@unq.gbl.spaces".into(),
            "19:two@unq.gbl.spaces".into(),
            "19:three@unq.gbl.spaces".into(),
        ]
    }

    fn fake_routes() -> Vec<crate::api::fake_transport::Route> {
        routes(&[
            (
                "19:one@unq.gbl.spaces",
                "bbbbbbbb-0000-0000-0000-000000000001",
                "Riley Stone",
            ),
            (
                "19:two@unq.gbl.spaces",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "Casey Morgan",
            ),
            (
                "19:three@unq.gbl.spaces",
                "bbbbbbbb-0000-0000-0000-000000000003",
                "Drew Parker",
            ),
        ])
    }

    #[tokio::test]
    async fn a_page_of_untitled_one_to_ones_resolves_together_then_never_again() {
        let fake = Fake::start(fake_routes()).await;
        let cache = Mutex::new(MateNames::default());
        let got =
            resolve_mates_cached(&fake.client(), &ids(), ME, &cache, 1_000, MATE_CONCURRENCY).await;
        assert_eq!(got.len(), 3);
        assert_eq!(got["19:one@unq.gbl.spaces"], "Riley Stone");
        assert_eq!(got["19:three@unq.gbl.spaces"], "Drew Parker");
        assert_eq!(
            fake.requests().len(),
            6,
            "one roster + one identity lookup per chat"
        );
        // Second page view / refresh: all from the cache, zero requests.
        let again =
            resolve_mates_cached(&fake.client(), &ids(), ME, &cache, 2_000, MATE_CONCURRENCY).await;
        assert_eq!(again, got);
        assert_eq!(fake.requests().len(), 6, "cache hits make no request");
    }

    #[tokio::test]
    async fn direct_thread_uses_verified_peer_profile_even_if_last_message_is_own() {
        let peer = "bbbbbbbb-0000-0000-0000-000000000001";
        let id = format!("19:{ME}_{peer}@unq.gbl.spaces");
        let fake = Fake::start(vec![("GET", "/graph/users/bbbbbbbb-0000-0000-0000-000000000001", 200,
            json!({"id":peer,"displayName":"Blair Costa"}).to_string())]).await;
        assert_eq!(resolve_mate_name(&fake.client(), &id, ME).await.as_deref(), Some("Blair Costa"));
        assert_eq!(fake.requests().len(), 1);
        let mismatch = Fake::start(vec![("GET", "/graph/users/bbbbbbbb-0000-0000-0000-000000000001", 200,
            json!({"id":ME,"displayName":"Alex Silva"}).to_string())]).await;
        assert_eq!(resolve_mate_name(&mismatch.client(), &id, ME).await, None);
        assert_eq!(resolve_mate_name(&fake.client(), &id, "cccccccc-0000-0000-0000-000000000001").await, None);
    }

    #[tokio::test]
    async fn directory_keeps_the_peer_name_after_an_outgoing_message_and_an_account_switch() {
        let peer="bbbbbbbb-0000-0000-0000-000000000001";
        let id=format!("19:{ME}_{peer}@unq.gbl.spaces");
        let mut r=vec![
            ("GET", "/graph/users/bbbbbbbb-0000-0000-0000-000000000001", 200, json!({"id":peer,"displayName":"Blair Costa"}).to_string()),
            ("GET", "/graph/users/aaaaaaaa-0000-0000-0000-000000000001", 200, json!({"id":ME,"displayName":"Alex Silva"}).to_string()),
        ];
        let own=json!({"id":"2","messagetype":"Text","imdisplayname":"Alex Silva","content":"Approved",
            "from":format!("https://chat.example.com/contacts/8:orgid:{ME}"),"originalarrivaltime":"2026-10-30T19:29:00Z"});
        r.push(("GET","/csa/teams/users/ME/conversations",200,json!({"conversations":[{
            "id":id,"threadProperties":{"topic":"Alex Silva"},"lastMessage":own
        }]}).to_string()));
        let fake=Fake::start(r).await;
        let mut old=MateNames::default();old.put(&id,"Alex Silva",1_000);
        let cache=Mutex::new(old);
        let first=list_chats_with_cache(&fake.client(),10,ME,&cache,1_000).await.unwrap();
        assert_eq!(first[0].name,"Blair Costa");assert!(first[0].peer_name_verified);
        assert_eq!(fake.requests().len(),2,"one directory and one identity lookup");
        let repeat=list_chats_with_cache(&fake.client(),10,ME,&cache,1_001).await.unwrap();
        assert_eq!(repeat[0].name,"Blair Costa");assert_eq!(fake.requests().len(),3,"verified cache is reused");
        let switched=list_chats_with_cache(&fake.client(),10,peer,&cache,1_002).await.unwrap();
        assert_eq!(switched[0].name,"Alex Silva","the other account must resolve its own peer");
        assert_eq!(cache.lock().unwrap().get(&mate_cache_key(&id,ME).unwrap(),1_002).as_deref(),Some("Blair Costa"));
    }

    #[tokio::test]
    async fn missing_roster_never_uses_the_last_sender_as_a_direct_chat_title() {
        let fake=Fake::start(vec![("GET","/csa/teams/users/ME/conversations",200,
            json!({"conversations":[{"id":"19:no-roster@unq.gbl.spaces","lastMessage":{"imdisplayname":"Alex Silva","content":"Approved"}}]}).to_string())]).await;
        let chats=list_chats_with_cache(&fake.client(),10,ME,&Mutex::new(MateNames::default()),1_000).await.unwrap();
        assert_eq!(chats[0].name,"[Direct message]");assert!(!chats[0].peer_name_verified);
    }

    #[tokio::test]
    async fn a_failed_lookup_leaves_that_chat_out_and_uncached() {
        let mut r = fake_routes();
        r.retain(|(_, p, _, _)| !p.contains("two@"));
        let fake = Fake::start(r).await;
        let cache = Mutex::new(MateNames::default());
        let got = resolve_mates_cached(&fake.client(), &ids(), ME, &cache, 1_000, 2).await;
        assert_eq!(got.len(), 2);
        assert!(!got.contains_key("19:two@unq.gbl.spaces"));
        assert!(
            cache
                .lock()
                .unwrap()
                .get(&mate_cache_key("19:two@unq.gbl.spaces",ME).unwrap(), 1_000)
                .is_none(),
            "failures are retried later"
        );
    }

    #[test]
    fn names_persist_across_a_reload_and_expire() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mate-tests-{}", std::process::id()));
        let path = dir.join("mates-config.json");
        let mut c = MateNames::load(Some(path.clone()));
        assert!(c.get("19:one@unq.gbl.spaces", 10).is_none());
        c.put("19:one@unq.gbl.spaces", "Riley Stone", 10);
        c.save();
        let back = MateNames::load(Some(path.clone()));
        assert_eq!(
            back.get("19:one@unq.gbl.spaces", 20).as_deref(),
            Some("Riley Stone")
        );
        assert!(
            back.get("19:one@unq.gbl.spaces", 10 + MATE_TTL_SECS + 1)
                .is_none(),
            "stale names are looked up again"
        );
        // Corrupt file: empty cache, no panic.
        std::fs::write(&path, "not json").unwrap();
        assert!(
            MateNames::load(Some(path))
                .get("19:one@unq.gbl.spaces", 20)
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
