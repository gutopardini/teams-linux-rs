//! Reproducible CPU interaction workload. Synthetic data only; no account/network.
//! Measures update + widget-tree construction/diff, not GPU or input-to-photon.
use crate::*;
use iced::advanced::widget::Tree;
use serde_json::{Value, json};
use std::hint::black_box;
use std::cell::Cell;

thread_local! {
    static HISTORY_BUILDS: Cell<Option<u64>> = const { Cell::new(None) };
    static CHAT_LIST_BUILDS: Cell<Option<u64>> = const { Cell::new(None) };
}

pub fn history_built() {
    HISTORY_BUILDS.with(|count| {
        if let Some(value) = count.get() { count.set(Some(value + 1)); }
    });
}

pub(crate) fn start_counting() {
    HISTORY_BUILDS.with(|count| count.set(Some(0)));
    CHAT_LIST_BUILDS.with(|count| count.set(Some(0)));
}
pub(crate) fn history_builds() -> u64 { HISTORY_BUILDS.with(|count| count.get().unwrap_or(0)) }
pub fn chat_list_built() {
    CHAT_LIST_BUILDS.with(|count| {
        if let Some(value) = count.get() { count.set(Some(value + 1)); }
    });
}
#[cfg(test)]
pub(crate) fn chat_list_builds() -> u64 { CHAT_LIST_BUILDS.with(|count| count.get().unwrap_or(0)) }

pub(crate) fn messages(count: usize) -> Vec<GraphMessage> {
    let html = "<p>Mensagem sintética com <strong>formatação</strong>, um <a href='https://example.test'>link</a> e texto para medir a interface.</p>";
    let rich = std::sync::Arc::new(message_body::Document::from_html(html).unwrap());
    (0..count).map(|index| GraphMessage {
        id: format!("perf-{index}"), author: "Pessoa de teste".into(),
        body: format!("Mensagem sintética {index} com texto para medir a interface."),
        rich: Some(rich.clone()), is_mine: index % 3 == 0,
        timestamp: "2026-10-27T12:00:00Z".into(), time: "12:00".into(), date: "27/10/2026".into(),
        ..Default::default()
    }).collect()
}

fn stats(values: &mut [f64]) -> Value {
    values.sort_by(f64::total_cmp);
    json!({"median_ms":values[values.len()/2], "p95_ms": values[(values.len()*95/100).min(values.len()-1)], "max_ms": values.last()})
}

fn edit_workload(count: usize, rich: bool) -> Value {
    let mut state = TeamsApp::from_client_id(String::new());
    state.messages = messages(count);
    state.chat_messages_loaded_for = state.selected_chat.clone();
    state.commit_visible_history();
    let base = if rich { "Texto de teste com acentuação. ".repeat(160) } else { "Texto de teste".into() };
    state.draft = base.clone();
    state.formatting_open = rich;
    state.composer_content = crate::composer_decorations::Content::with_text(&base);
    if rich {
        let length = unicode_segmentation::UnicodeSegmentation::graphemes(base.as_str(), true).count();
        state.composer_content.move_to(message_body::editor_cursor_at(&base, length));
        state.draft_formatting.entry(state.draft_key().unwrap()).or_default().toggle_mark(0, 16, message_body::InlineMark::Bold);
    }
    let panel = state.message_panel("Teste", "Dados fictícios", &state.messages, false, iced::Size::new(1050.0, 840.0), false);
    let mut tree = Tree::new(panel.as_widget());
    drop(panel);
    let mut updates = Vec::new(); let mut views = Vec::new();
    let count_before = history_builds();
    for index in 0..160 {
        let event = if rich {
            AppMessage::DraftAction(text_editor::Action::Edit(if index % 2 == 0 { text_editor::Edit::Insert('x') } else { text_editor::Edit::Backspace }))
        } else { AppMessage::DraftChanged(if index % 2 == 0 { format!("{base}x") } else { base.clone() }) };
        let start = Instant::now(); drop(black_box(update(&mut state, event)));
        let update_ms = start.elapsed().as_secs_f64()*1000.0;
        let start = Instant::now();
        let panel = state.message_panel("Teste", "Dados fictícios", &state.messages, false, iced::Size::new(1050.0, 840.0), false);
        tree.diff(panel.as_widget()); black_box(&panel); drop(panel);
        let view_ms = start.elapsed().as_secs_f64()*1000.0;
        if index >= 10 { updates.push(update_ms); views.push(view_ms); }
    }
    json!({"messages":count,"rich_editor":rich,"draft_bytes":base.len(),"samples":updates.len(),"update":stats(&mut updates),"widget_build_diff":stats(&mut views),"history_rebuilds_for_160_edits":history_builds()-count_before})
}

fn switch_workload() -> Value {
    let mut state = TeamsApp::from_client_id(String::new());
    state.demo_mode = false; // No session: no task can access a real service.
    state.selected_chat = Some("19:a@thread.v2".into());
    state.messages = messages(200);
    state.chat_messages_loaded_for = state.selected_chat.clone();
    drop(update(&mut state, AppMessage::SelectConversation("19:b@thread.v2".into())));
    state.messages = messages(200);
    state.chat_messages_loaded_for = state.selected_chat.clone();
    let start = Instant::now();
    drop(update(&mut state, AppMessage::SelectConversation("19:a@thread.v2".into())));
    json!({"return_update_ms": start.elapsed().as_secs_f64()*1000.0,"messages_visible_without_network":state.messages.len(),"account_requests":0})
}

pub fn run() {
    start_counting();
    let result = json!({"schema":2,"kind":"synthetic-cpu","limitations":"No GPU layout/raster, real network, official Teams comparison or input-to-photon measurement.","workloads":[edit_workload(80,false),edit_workload(1000,false),edit_workload(1000,true)],"switch":switch_workload()});
    println!("{}",serde_json::to_string_pretty(&result).unwrap());
}
