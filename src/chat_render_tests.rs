use super::*;
use iced::advanced::{
    Layout, layout, mouse,
    renderer::{self, Headless, Renderer as _},
    widget::{self, Operation, Tree},
};
use iced::{Pixels, Rectangle, Size, Vector};
use std::{borrow::Cow, path::PathBuf};

pub(crate) fn headless_renderer() -> iced::Renderer {
    typography::initialize(preferences::FontFamily::Inter);
    static FONTS: std::sync::Once = std::sync::Once::new();
    FONTS.call_once(|| {
        let mut fonts = iced::advanced::graphics::text::font_system().write().unwrap();
        for bytes in [
            typography::REGULAR_BYTES,
            typography::SEMIBOLD_BYTES,
            typography::INTER_REGULAR_BYTES,
            typography::INTER_SEMIBOLD_BYTES,
            typography::EMOJI_BYTES,
        ] {
            fonts.load_font(Cow::Borrowed(bytes));
        }
    });
    iced::futures::executor::block_on(<iced::Renderer as Headless>::new(
        typography::for_family(preferences::FontFamily::Inter),
        Pixels(16.0),
        Some("wgpu"),
    ))
    .expect("local wgpu adapter for chat regression")
}

pub(crate) fn capture(
    renderer: &mut iced::Renderer,
    element: &Element<'_, AppMessage>,
    tree: &Tree,
    node: &layout::Node,
    theme: &Theme,
    size: Size<u32>,
    zoom: f32,
) -> Vec<u8> {
    let viewport = Rectangle::with_size(Size::new(size.width as f32 / zoom, size.height as f32 / zoom));
    renderer.reset(viewport);
    element.as_widget().draw(
        tree,
        renderer,
        theme,
        &renderer::Style { text_color: tone(theme, Tone::Text) },
        Layout::new(node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    Headless::screenshot(renderer, size, zoom, tone(theme, Tone::Canvas))
}

pub(crate) fn save_snapshot(name: &str, size: Size<u32>, rgba: &[u8]) {
    let Some(dir) = std::env::var_os("TEAMS_LINUX_TEST_SNAPSHOTS").map(PathBuf::from) else { return; };
    let file = std::fs::File::create(dir.join(name)).expect("create test snapshot");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(rgba).unwrap();
}

#[derive(Default)]
struct HistoryBounds {
    viewport: Rectangle,
    content: Rectangle,
    offset: Vector,
    rows: HashMap<widget::Id, Rectangle>,
}

impl Operation for HistoryBounds {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
        if let Some(id) = id {
            self.rows.insert(id.clone(), bounds);
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn widget::operation::Scrollable,
    ) {
        if id == Some(&widget::Id::new("message-history")) {
            self.viewport = bounds;
            self.content = content;
            self.offset = translation;
        }
    }
}

#[test]
#[ignore = "requires a local GPU adapter; run explicitly for chat rendering"]
fn alert_history_renders_previous_cards_when_opened_from_cache() {
    let mut renderer = headless_renderer();
    let mut state = TeamsApp::from_client_id(String::new());
    state.preferences = Preferences {
        font_family: preferences::FontFamily::Inter,
        interface_zoom: 125,
        chat_sidebar_width: 464,
        roomy_lines: true,
        ..Default::default()
    };
    let alerts = state.selected_chat.clone().unwrap();
    let other = state.chats.iter().find(|chat| chat.id != alerts).unwrap().id.clone();
    let card = std::sync::Arc::new(message_body::Document::from_html(include_str!(
        "../tests/fixtures/workflow-adaptive-card.html"
    )).unwrap());
    for chat in &mut state.chats {
        if chat.id == alerts {
            chat.is_group = true;
            chat.title = "Alertas de demonstração".into();
        }
    }
    let messages: Vec<_> = (0..48).map(|index| {
        let is_card = (19..47).contains(&index);
        GraphMessage {
            id: format!("fixture-{index}"),
            author: if is_card { "Workflows" } else { "Marina Costa" }.into(),
            time: format!("13:{index:02}"),
            date: "21/11/2026".into(),
            body: format!("Mensagem de teste {index}"),
            rich: is_card.then(|| card.clone()),
            reply_to: (index == 47).then(|| "fixture-46".into()),
            quoted_context: (index == 47).then(|| quoted_reply::Context {
                message_id: "fixture-46".into(),
                sender: "28:12345678-1234-1234-1234-123456789abc".into(),
                author: "Workflows".into(),
                timestamp_ms: None,
                preview: "".into(),
            }),
            ..Default::default()
        }
    }).collect();
    state.messages = messages.clone();
    state.commit_visible_history();
    let size = Size::new(2560, 1373);
    let zoom = 1.25;
    let logical = Size::new(size.width as f32 / zoom, size.height as f32 / zoom);
    let limits = layout::Limits::new(Size::ZERO, logical);
    let mut tree = Tree::new(view_impl(&state).as_widget());
    for chat in [&other, &alerts] {
        // Use the real conversation cache with a synthetic, disconnected account.
        state.demo_mode = false;
        drop(update(&mut state, AppMessage::SelectConversation(chat.clone())));
        state.demo_mode = true;
        let mut element = view_impl(&state);
        tree.diff(element.as_widget());
        element.as_widget_mut().layout(&mut tree, &renderer, &limits);
    }
    assert!(state.messages == messages, "the cache restored all the same messages");

    let mut element = view_impl(&state);
    tree.diff(element.as_widget());
    let node = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut measured = HistoryBounds::default();
    element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut measured);
    let last = measured.rows[&history_scroll::message_anchor("fixture-47")] - measured.offset;
    let prior = measured.rows[&history_scroll::message_anchor("fixture-46")] - measured.offset;
    assert!(last.intersects(&measured.viewport), "latest row {last:?}, viewport {:?}", measured.viewport);
    assert!(prior.intersects(&measured.viewport), "previous card {prior:?}, viewport {:?}, content {:?}", measured.viewport, measured.content);

    let theme = teams_theme(state.preferences.appearance);
    let rgba = capture(&mut renderer, &element, &tree, &node, &theme, size, zoom);
    save_snapshot("alertas-125.png", size, &rgba);
    let card_background = tone(&theme, Tone::Card).into_rgba8();
    let card_area = prior.intersection(&measured.viewport).unwrap();
    let left = (card_area.x * zoom) as usize;
    let right = ((card_area.x + card_area.width) * zoom) as usize;
    let top = (card_area.y * zoom) as usize;
    let bottom = ((card_area.y + card_area.height) * zoom) as usize;
    let stride = size.width as usize * 4;
    let painted_card_rows = (top..bottom).filter(|&y| {
        let row = &rgba[y * stride + left * 4..y * stride + right * 4];
        row.chunks_exact(4).filter(|pixel| *pixel == card_background).count() >= 100
    }).count();
    assert!(painted_card_rows >= 20, "earlier workflow card reserves a readable area: {painted_card_rows} drawn rows, previous {prior:?}, latest {last:?}");
    eprintln!("Alert history: {} messages, previous message {:.1} logical px tall, {painted_card_rows} visible card scanlines", state.messages.len(), prior.height);
}
