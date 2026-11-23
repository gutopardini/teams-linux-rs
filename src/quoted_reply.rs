//! Typed Teams quote cards. Static protocol evidence: original module663717.
//! The body HTML comes only from our safe native document serializer.
use crate::i18n::tr;
use crate::{graph::GraphMessage, message_body::Document};
use serde_json::{Value, json};
use std::collections::HashMap;

pub const TYPE: &str = "http://schema.skype.com/Reply";

fn author_label<'a>(
    context: &'a Context,
    origin: Option<&'a GraphMessage>,
    names: &'a HashMap<String, String>,
) -> &'a str {
    fn known_author(name: &str) -> Option<&str> {
        crate::sender_names::known(name).filter(|name| !name.eq_ignore_ascii_case("Display Name"))
    }

    let sender = crate::reactions::identity(&context.sender);
    let app_id = sender.strip_prefix("28:");
    let author = origin.filter(|message| message.id == context.message_id)
        .and_then(|message| known_author(&message.author))
        .or_else(|| known_author(&context.author))
        .or_else(|| {
            if let Some(id) = app_id {
                names.get(&format!("bot:{id}")).and_then(|name| known_author(name))
                    .or_else(|| names.get(&format!("app:{id}")).and_then(|name| known_author(name)))
            } else {
                names.get(&sender).and_then(|name| known_author(name))
            }
        })
        .unwrap_or_else(|| tr("Quoted message"));
    if app_id.is_some() && matches!(author, "Workflows" | "Fluxos de trabalho") {
        tr("Workflows")
    } else {
        author
    }
}

pub fn has_card(origin: Option<&GraphMessage>) -> bool {
    origin.and_then(|message| message.rich.as_deref())
        .is_some_and(|document| document.blocks.iter().any(|block| block.card.is_some()))
}

fn uses_notebook(context: &Context, origin: Option<&GraphMessage>) -> bool {
    let preview = context.preview.trim().trim_end_matches('\u{fe0f}');
    has_card(origin) || (context.sender.starts_with("28:")
        && (matches!(preview, "📄" | "📓" | "[Anexo]" | "[Attachment]")
            || preview.contains("https://go.skype.com/cards.unsupported")
            || (preview.is_empty() && origin.is_none())))
}

pub fn notebook_icon() -> crate::Element<'static, crate::AppMessage> {
    crate::icon_tone(crate::Icon::Notebook, crate::Tone::Secondary, 20.0)
}

pub fn sent_card(
    state: &crate::TeamsApp,
    message: &GraphMessage,
    context: &Context,
    origin: Option<&GraphMessage>,
    names: &HashMap<String, String>,
) -> crate::Element<'static, crate::AppMessage> {
    use crate::*;
    let size = f32::from(state.preferences.message_size);
    let date = context
        .timestamp_ms
        .and_then(quote_stamp)
        .unwrap_or_default();
    let author = author_label(context, origin, names);
    let preview: Element<'static, AppMessage> = if uses_notebook(context, origin) {
        notebook_icon()
    } else {
        let preview = preview_line(&context.preview, 160);
        if state.message_search_open && !state.message_search_query.trim().is_empty() {
            message_body::view_plain_with_search_highlights(
                &preview,
                &state.message_search_query,
                size * 0.94,
                &state.preferences,
                &teams_theme(state.preferences.appearance),
                message.is_mine,
            )
        } else {
            text(preview)
                .size(size * 0.94)
                .style(ui_text(Tone::Text))
                .into()
        }
    };
    let content = iced::widget::column![
        text(format!("{author} {date}").trim_end().to_owned())
            .size(size * 0.82)
            .style(ui_text(Tone::Muted)),
        preview,
    ].spacing(3);
    let card = container(stack![
        container(content).padding(iced::Padding {
            left: 13.0,
            ..Default::default()
        }),
        container(space::vertical())
            .width(3)
            .height(Fill)
            .style(quote_mark(1.0)),
    ])
    .padding([6, 8])
    .max_width(600)
    .style(quiet_frame(Tone::Canvas, 4.0));
    let can_navigate = origin.is_some() || (state.history_before.is_some() && state.session.is_some());
    let searching = state
        .pending_reply_origin_navigation
        .as_ref()
        .is_some_and(|pending| {
            pending.epoch == state.session_epoch
                && pending.message_id == context.message_id
                && state.selected_chat.as_deref() == Some(pending.chat_id.as_str())
        });
    let label = if searching {
        tr("Finding the original message…")
    } else if can_navigate {
        tr("View original message")
    } else {
        tr("Original message outside the available history")
    };
    hint(
        button(card)
            .padding(0)
            .on_press_maybe(
                (can_navigate && !searching)
                    .then(|| AppMessage::NavigateToMessage(context.message_id.clone())),
            )
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        label,
    )
}

pub fn composer_key(
    key: iced::widget::text_editor::KeyPress,
) -> Option<iced::widget::text_editor::Binding<crate::AppMessage>> {
    use iced::{
        keyboard::{Key, key::Named},
        widget::text_editor::{Binding, Status},
    };
    if matches!(key.status, Status::Focused { .. })
        && !key.modifiers.control()
        && !key.modifiers.alt()
        && key.key == Key::Named(Named::Enter)
    {
        return Some(if key.modifiers.shift() {
            Binding::Enter
        } else {
            Binding::Custom(crate::AppMessage::SendMessage)
        });
    }
    if matches!(key.status, Status::Focused { .. })
        && !key.modifiers.control()
        && !key.modifiers.alt()
        && !key.modifiers.shift()
        && key.key == Key::Named(Named::Escape)
    {
        return Some(Binding::Custom(crate::AppMessage::ClearReplyTarget));
    }
    Binding::from_key_press(key)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub message_id: String,
    pub sender: String,
    pub author: String,
    pub timestamp_ms: Option<i64>,
    pub preview: String,
}
impl Context {
    pub fn from_message(message: &GraphMessage, own_id: &str) -> Option<Self> {
        let sender = message
            .author_id
            .as_deref()
            .or(message.is_mine.then_some(own_id))?;
        let sender = if let Some(id) = sender
            .strip_prefix("bot:")
            .or_else(|| sender.strip_prefix("app:"))
        {
            format!("28:{id}")
        } else {
            format!("8:orgid:{}", crate::avatars::person_id(sender)?)
        };
        Some(Self {
            message_id: message.id.clone(),
            sender,
            author: message.author.clone(),
            timestamp_ms: chrono::DateTime::parse_from_rfc3339(&message.timestamp)
                .ok()
                .map(|time| time.timestamp_millis())
                .or_else(|| message.id.parse::<i64>().ok().filter(|time| *time > 0)),
            preview: ost::api::reply_snippet(
                &message
                    .rich
                    .as_ref()
                    .map(|doc| doc.clipboard_payload().text)
                    .unwrap_or_else(|| message.body.clone()),
            ),
        })
    }
    pub fn html(&self) -> Result<String, String> {
        if self.message_id.is_empty()
            || self.message_id.len() > 1024
            || self.message_id.chars().any(char::is_control)
            || self.sender.is_empty()
            || self.sender.len() > 256
            || self.author.trim().is_empty()
            || self.author.len() > 4096
        {
            return Err(tr("Could not identify the quoted message. Reopen the chat and try again.").into());
        }
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;")
        };
        let time = self
            .timestamp_ms
            .filter(|time| *time > 0)
            .map(|time| format!("<span itemprop=\"time\" itemid=\"{time}\"></span>"))
            .unwrap_or_default();
        Ok(format!(
            "<blockquote itemscope=\"\" itemtype=\"{TYPE}\" itemid=\"{}\"><strong itemprop=\"mri\" itemid=\"{}\">{}</strong>{time}<p itemprop=\"preview\">{}</p></blockquote>",
            escape(&self.message_id),
            escape(&self.sender),
            escape(&self.author),
            escape(&ost::api::reply_snippet(&self.preview))
        ))
    }
    pub fn body(&self, html: &str) -> Result<Value, String> {
        let mut metadata = json!({"messageId":self.message_id,"sender":self.sender});
        if let Some(time) = self.timestamp_ms.filter(|time| *time > 0) {
            metadata["time"] = json!(time);
        }
        Ok(
            json!({"content":format!("{}{html}",self.html()?),"messagetype":"RichText/Html","contenttype":"text",
            "properties":{"qtdMsgs":serde_json::to_string(&vec![metadata]).map_err(|_|tr("Could not prepare the quote."))?}}),
        )
    }
}

/// Preserve the body and quoted identity separately. Legacy <quote> remains
/// handled by the old parser, so existing local and historic messages survive.
pub fn split(raw: &str) -> Option<(Context, String)> {
    let (mut quotes, html) = split_all(raw)?;
    Some((quotes.drain(..).next()?, html))
}

/// Every native reply card in the message, outer and nested. The returned HTML
/// no longer contains those cards, so the reply text is not painted again as a quote.
pub fn split_all(raw: &str) -> Option<(Vec<Context>, String)> {
    if raw.len() > 2 * 1024 * 1024 || !raw.contains(TYPE) {
        return None;
    }
    let mut html = scraper::Html::parse_fragment(raw);
    let selector =
        scraper::Selector::parse("blockquote[itemtype='http://schema.skype.com/Reply']").ok()?;
    let ids: Vec<_> = html.select(&selector).map(|quote| quote.id()).collect();
    if ids.is_empty() {
        return None;
    }
    let mut contexts = Vec::new();
    let mut detach = Vec::new();
    for id in ids {
        let Some(node) = html.tree.get(id) else {
            continue;
        };
        let Some(quote) = scraper::ElementRef::wrap(node) else {
            continue;
        };
        let Some(mut context) = context_from_quote(quote) else {
            continue;
        };
        let external_preview = context
            .preview
            .trim()
            .is_empty()
            .then(|| following_preview(quote))
            .flatten();
        if let Some(preview) = external_preview {
            context.preview = ost::api::reply_snippet(&text_outside_reply_cards(preview));
        }
        if context.html().is_err() {
            continue;
        }
        if let Some(preview) = external_preview {
            detach.push(preview.id());
        }
        contexts.push(context);
        detach.push(id);
    }
    if contexts.is_empty() {
        return None;
    }
    detach.sort_by_key(|id| {
        let mut depth = 0usize;
        let mut current = html.tree.get(*id);
        while let Some(node) = current {
            depth += 1;
            current = node.parent();
        }
        std::cmp::Reverse(depth)
    });
    for id in detach {
        if let Some(mut node) = html.tree.get_mut(id) {
            node.detach();
        }
    }
    Some((contexts, html.root_element().inner_html()))
}

fn context_from_quote(quote: scraper::ElementRef<'_>) -> Option<Context> {
    let id = quote.value().attr("itemid")?.to_owned();
    let mri_selector = scraper::Selector::parse("[itemprop='mri']").ok()?;
    let author = quote
        .select(&mri_selector)
        .find(|node| belongs_to_quote(*node, quote))?;
    let sender = author.value().attr("itemid")?.to_owned();
    let author = text_outside_reply_cards(author);
    let preview = quote
        .select(&scraper::Selector::parse("[itemprop='preview']").ok()?)
        .find(|node| belongs_to_quote(*node, quote))
        .map(text_outside_reply_cards)
        .unwrap_or_default();
    let timestamp_ms = quote
        .select(&scraper::Selector::parse("[itemprop='time']").ok()?)
        .find(|node| belongs_to_quote(*node, quote))
        .and_then(|node| node.value().attr("itemid"))
        .and_then(|time| time.parse().ok());
    Some(Context {
        message_id: id,
        sender,
        author,
        timestamp_ms,
        preview: ost::api::reply_snippet(&preview),
    })
}

fn following_preview(quote: scraper::ElementRef<'_>) -> Option<scraper::ElementRef<'_>> {
    let mut sibling = quote.next_sibling();
    while let Some(node) = sibling {
        if let Some(element) = scraper::ElementRef::wrap(node) {
            return (element.value().attr("itemprop") == Some("preview")).then_some(element);
        }
        let blank = node
            .value()
            .as_text()
            .is_some_and(|text| text.trim().is_empty());
        if !blank {
            return None;
        }
        sibling = node.next_sibling();
    }
    None
}

fn belongs_to_quote(node: scraper::ElementRef<'_>, quote: scraper::ElementRef<'_>) -> bool {
    let mut current = Some(node);
    while let Some(element) = current {
        if element.id() == quote.id() {
            return true;
        }
        let nested_reply = element.value().name() == "blockquote"
            && element
                .value()
                .attr("itemtype")
                .is_some_and(|kind| kind == TYPE);
        if nested_reply {
            return false;
        }
        current = element.parent().and_then(scraper::ElementRef::wrap);
    }
    false
}

fn text_outside_reply_cards(root: scraper::ElementRef<'_>) -> String {
    let mut text = String::new();
    append_visible_text(root, &mut text);
    text
}

fn append_visible_text(root: scraper::ElementRef<'_>, text: &mut String) {
    for child in root.children() {
        if let Some(element) = scraper::ElementRef::wrap(child) {
            let nested = element.value().name() == "blockquote"
                && element
                    .value()
                    .attr("itemtype")
                    .is_some_and(|kind| kind == TYPE);
            if !nested {
                append_visible_text(element, text);
            }
        } else if let Some(value) = child.value().as_text() {
            text.push_str(value);
        }
    }
}

pub fn quote_stamp(millis: i64) -> Option<String> {
    let now = chrono::Local::now().fixed_offset();
    let time = chrono::DateTime::from_timestamp_millis(millis)?.with_timezone(now.offset());
    Some(time.format("%d/%m/%Y %H:%M").to_string())
}

pub fn editor_source(message: &GraphMessage) -> (String, crate::message_body::ComposerFormatting) {
    message
        .rich
        .as_ref()
        .map(|document| {
            let paste = document.to_composer_paste();
            (paste.text, paste.formatting)
        })
        .unwrap_or_else(|| (message.body.clone(), Default::default()))
}
pub fn edited_body(
    id: &str,
    text: &str,
    formatting: &crate::message_body::ComposerFormatting,
    context: Option<&Context>,
) -> Result<Value, String> {
    let html = Document::from_editor(text, formatting).to_html();
    let mut body = if let Some(context) = context {
        context.body(&html)?
    } else {
        ost::api::send_message_html_body(&html)
    };
    body["skypeeditedid"] = json!(id);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_quote_author_uses_the_origin_or_the_exact_app_identity() {
        let (mut quotes, _) = split_all(include_str!("../tests/fixtures/workflow-reply.html")).unwrap();
        let context = quotes.pop().unwrap();
        assert_eq!(context.author, "Display Name");
        let mut origin = GraphMessage {
            id: context.message_id.clone(),
            author: "Workflows".into(),
            ..Default::default()
        };
        let names = HashMap::from([("bot:11111111-1111-4111-8111-111111111111".into(), "Workflows".into())]);
        assert_eq!(author_label(&context, Some(&origin), &HashMap::new()), tr("Workflows"));
        assert_eq!(author_label(&context, None, &names), tr("Workflows"));
        origin.id = "different-card".into();
        assert_eq!(author_label(&context, Some(&origin), &HashMap::new()), tr("Quoted message"));
    }

    #[test]
    fn workflow_card_quote_uses_a_notebook_and_keeps_normal_quotes_as_text() {
        let (mut quotes, _) = split_all(include_str!("../tests/fixtures/workflow-reply.html")).unwrap();
        let mut context = quotes.pop().unwrap();
        let origin = GraphMessage {
            rich: Document::from_html(include_str!("../tests/fixtures/workflow-adaptive-card.html"))
                .map(std::sync::Arc::new),
            ..Default::default()
        };
        assert!(uses_notebook(&context, None));
        context.preview = "📄".into();
        assert!(uses_notebook(&context, None));
        context.preview = "Workflow details".into();
        assert!(uses_notebook(&context, Some(&origin)));
        assert!(!uses_notebook(&context, None));
        context.sender = "8:orgid:11111111-1111-4111-8111-111111111111".into();
        context.preview.clear();
        assert!(!uses_notebook(&context, None));
    }

    #[test]
    fn shift_enter_breaks_the_line_and_enter_sends() {
        use iced::keyboard::{
            Key, Modifiers,
            key::{Code, Named, Physical},
        };
        let press = |modifiers| iced::widget::text_editor::KeyPress {
            key: Key::Named(Named::Enter),
            modified_key: Key::Named(Named::Enter),
            physical_key: Physical::Code(Code::Enter),
            modifiers,
            text: None,
            status: iced::widget::text_editor::Status::Focused { is_hovered: false },
        };
        assert!(matches!(
            composer_key(press(Modifiers::empty())),
            Some(iced::widget::text_editor::Binding::Custom(
                crate::AppMessage::SendMessage
            ))
        ));
        assert!(matches!(
            composer_key(press(Modifiers::SHIFT)),
            Some(iced::widget::text_editor::Binding::Enter)
        ));
    }

    #[test]
    fn quoting_own_message_uses_the_account_identity_when_echo_omits_author_id() {
        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let message = GraphMessage {
            id: "1793400000000".into(),
            author: "Ana".into(),
            body: "Original".into(),
            is_mine: true,
            ..Default::default()
        };
        let target = crate::reply_target_for_message("chat", &message, owner);
        assert_eq!(target.context.unwrap().sender, format!("8:orgid:{owner}"));
        let message = GraphMessage {
            is_mine: false,
            ..message
        };
        assert!(
            crate::reply_target_for_message("chat", &message, owner)
                .context
                .is_none(),
            "never attribute an unidentified incoming message to the account owner"
        );
    }
    #[test]
    fn native_quote_roundtrips_without_losing_identity_or_duplicating_preview() {
        let context = Context {
            message_id: "1793400000000".into(),
            sender: "8:orgid:aaaaaaaa-1111-2222-3333-444444444444".into(),
            author: "Ana <QA> & Co".into(),
            timestamp_ms: Some(1793400000000),
            preview: "Ship <it> & verify".into(),
        };
        let body = context.body("<p><b>Teste</b></p>").unwrap();
        let properties: Value =
            serde_json::from_str(body["properties"]["qtdMsgs"].as_str().unwrap()).unwrap();
        assert_eq!(properties[0]["messageId"], context.message_id);
        assert_eq!(properties[0]["sender"], context.sender);
        assert_eq!(properties[0]["time"], 1793400000000i64);
        let (parsed, html) = split(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(parsed, context);
        assert!(!html.contains("blockquote"));
        assert_eq!(
            Document::from_html(&html)
                .unwrap()
                .clipboard_payload()
                .text
                .trim(),
            "Teste"
        );
        let edited = edited_body("reply", "Changed", &Default::default(), Some(&parsed)).unwrap();
        assert_eq!(edited["skypeeditedid"], "reply");
        assert!(edited["content"].as_str().unwrap().contains(TYPE));
    }
    #[test]
    fn quote_composer_keeps_the_draft_and_does_not_force_formatting_controls() {
        use crate::*;
        let mut state = TeamsApp::from_client_id(String::new());
        state.demo_mode = true;
        state.draft = "Keep draft".into();
        state.formatting_open = false;
        let id = state
            .messages
            .iter()
            .find(|message| !message.is_mine && !message.id.starts_with("local:"))
            .unwrap()
            .id
            .clone();
        drop(update(&mut state, AppMessage::ReplyToMessage(id)));
        assert!(state.active_reply_target().is_some());
        assert!(!state.formatting_open);
        drop(update(
            &mut state,
            AppMessage::DraftAction(iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Insert('X'),
            )),
        ));
        assert!(state.draft.contains('X'));
        state
            .composer_content
            .move_to(message_body::editor_cursor_at(&state.draft, 1));
        drop(update(&mut state, AppMessage::PasteClipboard(None)));
        let target = state.clipboard_paste_target.clone().unwrap();
        assert_eq!(target.selection, (1, 1));
        let epoch = state.session_epoch;
        drop(update(
            &mut state,
            AppMessage::ClipboardRead(epoch, target.draft_key, Ok(images::Paste::Text("&".into()))),
        ));
        assert!(
            !state.formatting_open,
            "plain paste must not expose formatting controls"
        );
        assert!(state.draft.contains('&'));
        let draft = state.draft.clone();
        drop(update(&mut state, AppMessage::ClearReplyTarget));
        assert_eq!(state.draft, draft);
        assert!(state.reply_target.is_none());
    }

    #[test]
    fn two_reply_cards_keep_their_own_preview_and_leave_only_the_answer() {
        let william = Context {
            message_id: "1762867140000".into(),
            sender: "8:orgid:aaaaaaaa-1111-2222-3333-444444444444".into(),
            author: "William dos Santos".into(),
            timestamp_ms: Some(1_762_867_140_000),
            preview: "bom dia a Todos, vamos seguir com os testes".into(),
        };
        let leonardo = Context {
            message_id: "1762886580000".into(),
            sender: "8:orgid:bbbbbbbb-1111-2222-3333-444444444444".into(),
            author: "Leonardo Emilly Dias Santos Bido".into(),
            timestamp_ms: Some(1_762_886_580_000),
            preview: "por hora, aconselho seguir".into(),
        };
        let raw = format!(
            "{}{}<p>Amanda, seria possível essa validação?</p>",
            william.html().unwrap(),
            leonardo.html().unwrap()
        );
        let (quotes, html) = split_all(&raw).unwrap();
        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0].author, "William dos Santos");
        assert!(!quotes[0].preview.contains("Leonardo"));
        assert_eq!(quotes[1].preview, "por hora, aconselho seguir");
        assert!(!html.contains("blockquote"));
        assert!(html.contains("Amanda, seria possível"));

        let nested = william.html().unwrap().replacen(
            "</p></blockquote>",
            &format!("{}</p></blockquote><p>Pergunta</p>", leonardo.html().unwrap()),
            1,
        );
        let (quotes, html) = split_all(&nested).unwrap();
        assert_eq!(quotes.len(), 2, "{html}");
        assert!(
            !quotes[0].preview.contains("Leonardo"),
            "{}",
            quotes[0].preview
        );
        assert_eq!(quotes[1].author, "Leonardo Emilly Dias Santos Bido");
        assert!(quotes[1].preview.contains("por hora"));
        assert!(!html.contains("blockquote"));
        assert!(html.contains("Pergunta"));
        assert!(!html.contains("por hora"));
    }
}
