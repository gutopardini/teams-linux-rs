//! Opt-in native-window benchmark. Records action handling to end of CPU draw
//! submission, not monitor presentation or completed GPU execution.
use crate::*;
use iced::advanced::{Widget, widget::{Tree, Operation, tree}, layout, Layout, mouse, overlay, renderer, Clipboard, Shell};
use iced::{Event, Length, Rectangle, Size, Vector};
use std::sync::{Mutex, OnceLock, atomic::{AtomicBool, Ordering::Relaxed}};

static ENABLED: AtomicBool = AtomicBool::new(false);
static PROBE: OnceLock<Mutex<Probe>> = OnceLock::new();

#[derive(Default)]
struct Probe {
    index: usize,
    pending: Option<(&'static str, Instant)>,
    samples: Vec<(&'static str, f64)>,
}

pub fn enable() { ENABLED.store(true, Relaxed); }
pub fn enabled() -> bool { ENABLED.load(Relaxed) }

fn later() -> Task<AppMessage> {
    Task::perform(async { futures_timer::Delay::new(Duration::from_millis(50)).await; }, |_| AppMessage::PerformancePulse)
}

pub fn pulse(state: &mut TeamsApp) -> Task<AppMessage> {
    if !enabled() || !state.demo_mode { return Task::none(); }
    let mut probe = PROBE.get_or_init(|| Mutex::new(Probe::default())).lock().unwrap();
    if let Some((kind, started)) = probe.pending {
        if started.elapsed() < Duration::from_secs(3) { return later(); }
        eprintln!("Synthetic UI benchmark timed out waiting for draw: kind={kind} index={}",probe.index);
        return iced::exit();
    }
    let index = probe.index;
    probe.index += 1;
    if index == 0 {
        state.chats = (0..100).map(|index| ChatSummary {
            id: format!("19:perf-{index}@thread.v2"),
            title: format!("Pessoa sintética {index} da equipe de testes"),
            preview: "Prévia fictícia de uma conversa recente para medir a lista.".into(),
            is_group: index % 4 == 0, ..Default::default()
        }).collect();
        state.selected_chat = Some(state.chats[0].id.clone());
        state.messages = performance::messages(1_000);
        state.chat_messages_loaded_for = state.selected_chat.clone();
        drop(update(state, AppMessage::CloseMessageMenu));
    }
    if index == 50 {
        drop(update(state, AppMessage::DraftChanged("Texto de teste com acentuação. ".repeat(160))));
        if !state.formatting_open { drop(update(state, AppMessage::ToggleFormatting)); }
    }
    let search_demo = std::env::args().any(|arg| arg == "--reaction-search-perf-demo");
    if index == 92 && search_demo {
        // Match the production first-page limit. Native popup layout, filtering
        // and painting are included; no session or account exists in this demo.
        state.messages = performance::messages(50);
        state.chat_messages_loaded_for = state.selected_chat.clone();
        let id = state.messages.last().unwrap().id.clone();
        drop(update(state,AppMessage::CloseMessageMenu));
        drop(update(state,AppMessage::OpenReactionPicker(id)));
        return iced::widget::operation::snap_to("message-history",iced::widget::scrollable::RelativeOffset::END).chain(later());
    }
    if index >= if search_demo {114} else {92} {
        let mut output = serde_json::Map::new();
        for kind in ["type", "delete", "rich_edit", "switch", "reaction_search_50"] {
            let mut samples: Vec<_> = probe.samples.iter().filter(|(name,_)| *name==kind).map(|(_,ms)| *ms).collect();
            samples.sort_by(f64::total_cmp);
            if !samples.is_empty() {
                output.insert(kind.into(), serde_json::json!({"samples":samples.len(),"median_ms":samples[samples.len()/2],"p95_ms":samples[samples.len()*95/100],"max_ms":samples.last()}));
            }
        }
        println!("{}", serde_json::json!({"kind":"synthetic-native-ui","metric":"action handling to end of CPU draw submission; GPU completion and monitor presentation excluded","workloads":output}));
        return iced::exit();
    }
    let kind = if index < 30 { "type" } else if index < 50 { "delete" } else if index < 72 { "rich_edit" } else if index < 92 { "switch" } else { "reaction_search_50" };
    if index >= 10 && index != 50 && index != 72 && index != 73 && index != 93 {
        probe.pending = Some((kind, Instant::now()));
    }
    drop(probe);
    let action = match kind {
        "reaction_search_50" => AppMessage::ReactionSearchChanged(if index % 2 == 0 {"foguete"} else {"palmas"}.into()),
        "type" if state.composer_uses_editor() => AppMessage::DraftAction(text_editor::Action::Edit(text_editor::Edit::Insert('x'))),
        "delete" if state.composer_uses_editor() => AppMessage::DraftAction(text_editor::Action::Edit(text_editor::Edit::Backspace)),
        "type" => AppMessage::DraftChanged(format!("{}x", state.draft)),
        "delete" => { let mut text=state.draft.clone();text.pop();AppMessage::DraftChanged(text) },
        "rich_edit" => AppMessage::DraftAction(text_editor::Action::Edit(if index%2==0 { text_editor::Edit::Insert('x') } else { text_editor::Edit::Backspace })),
        _ => {
            let id = state.chats[if index%2==0 { 0 } else { 1 }].id.clone();
            // Exercise the production selector with no session/account. Restore
            // demo mode before view/subscription; no task can reach a service.
            state.demo_mode = false;
            drop(update(state, AppMessage::SelectConversation(id)));
            state.demo_mode = true;
            if state.messages.is_empty() {
                state.messages = performance::messages(1_000);
                state.chat_messages_loaded_for = state.selected_chat.clone();
                drop(update(state, AppMessage::CloseMessageMenu));
            }
            return later();
        }
    };
    drop(update(state, action));
    later()
}

pub fn wrap<'a>(inner: Element<'a, AppMessage>) -> Element<'a, AppMessage> {
    if enabled() || live_trace::enabled() { Element::new(DrawProbe { inner }) } else { inner }
}

struct DrawProbe<'a> { inner: Element<'a, AppMessage> }
impl Widget<AppMessage, Theme, iced::Renderer> for DrawProbe<'_> {
    fn tag(&self) -> tree::Tag { self.inner.as_widget().tag() }
    fn state(&self) -> tree::State { self.inner.as_widget().state() }
    fn children(&self) -> Vec<Tree> { self.inner.as_widget().children() }
    fn diff(&self, tree: &mut Tree) { live_trace::span("widget_diff", || self.inner.as_widget().diff(tree)); }
    fn size(&self) -> Size<Length> { self.inner.as_widget().size() }
    fn size_hint(&self) -> Size<Length> { self.inner.as_widget().size_hint() }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        live_trace::span("layout", || self.inner.as_widget_mut().layout(tree, renderer, limits))
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.inner.as_widget_mut().operate(tree, layout, renderer, operation);
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &iced::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, AppMessage>, viewport: &Rectangle) {
        live_trace::input(event);
        live_trace::span("widget_event", || self.inner.as_widget_mut().update(tree,event,layout,cursor,renderer,clipboard,shell,viewport));
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &Theme, style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        live_trace::span("cpu_draw", || self.inner.as_widget().draw(tree,renderer,theme,style,layout,cursor,viewport));
        live_trace::drawn();
        if let Some(probe) = PROBE.get() {
            let mut probe=probe.lock().unwrap();
            if let Some((kind,start))=probe.pending.take() { probe.samples.push((kind,start.elapsed().as_secs_f64()*1000.0)); }
        }
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &iced::Renderer) -> mouse::Interaction {
        self.inner.as_widget().mouse_interaction(tree,layout,cursor,viewport,renderer)
    }
    fn overlay<'a>(&'a mut self, tree: &'a mut Tree, layout: Layout<'a>, renderer: &iced::Renderer, viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'a,AppMessage,Theme,iced::Renderer>> {
        self.inner.as_widget_mut().overlay(tree,layout,renderer,viewport,translation)
    }
}
