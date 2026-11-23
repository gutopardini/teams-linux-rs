use iced::mouse::Interaction;
use iced::widget::{button, column, container, mouse_area, row, space, stack, text};
use iced::window::{Direction, Id};
use iced::{Alignment, Border, Element, Fill, Padding, Theme};

use crate::appearance::{Tone, metrics::TOP_BAR, tone, ui_button, ui_surface, ui_text};
use crate::{AppMessage, Icon, NAV_RAIL_WIDTH, hint, icon_tone, tr};

const CONTROL_WIDTH: f32 = 40.0;
const CONTROL_STRIP: f32 = CONTROL_WIDTH * 3.0;
const RESIZE_EDGE: f32 = 5.0;
pub const PROFILE_TRAILING: f32 = CONTROL_STRIP + 12.0;
pub const PROFILE_WIDTH: f32 = 44.0;
pub const MENU_TRAILING: f32 = PROFILE_TRAILING + PROFILE_WIDTH + 4.0;
pub const MENU_WIDTH: f32 = 36.0;
pub const ARROW_WIDTH: f32 = 40.0;

#[derive(Clone, Copy)]
pub enum Action {
    Drag,
    Resize(Direction),
    Minimize,
    ToggleMaximize,
    Close,
}

pub struct BarLayout {
    pub brand_width: f32,
    pub navigation: bool,
    pub before_navigation: f32,
    pub search_left: f32,
    pub search_width: f32,
    pub after_search: f32,
}

impl BarLayout {
    pub fn new(width: f32) -> Self {
        let navigation = width >= 720.0;
        let brand_width = if navigation { NAV_RAIL_WIDTH } else { 54.0 };
        let navigation_width = if navigation { ARROW_WIDTH * 2.0 } else { 0.0 };
        let minimum_left = brand_width + navigation_width + 5.0;
        let available = width - minimum_left - MENU_TRAILING - MENU_WIDTH;
        let search_width = (available - 12.0).clamp(1.0, 800.0);
        let extra_space = (available - search_width - 12.0).max(0.0);
        let before_navigation = extra_space * 0.435;

        Self {
            brand_width,
            navigation,
            before_navigation,
            search_left: minimum_left + before_navigation,
            search_width,
            after_search: (available - search_width - before_navigation).max(0.0),
        }
    }
}

pub fn title_bar<'a>(
    content: impl Into<Element<'a, AppMessage>>,
    window: Option<Id>,
) -> Element<'a, AppMessage> {
    let bar = container(content)
        .width(Fill)
        .center_y(TOP_BAR)
        .style(ui_surface(Tone::TopBar, false, 0.0));
    if let Some(id) = window {
        mouse_area(bar)
            .on_press(AppMessage::MainWindowAction(id, Action::Drag))
            .on_double_click(AppMessage::MainWindowAction(id, Action::ToggleMaximize))
            .into()
    } else {
        bar.into()
    }
}

pub fn signed_out<'a>(content: Element<'a, AppMessage>, window: Option<Id>) -> Element<'a, AppMessage> {
    let title = container(text("Teams Linux").size(16).style(ui_text(Tone::Secondary))).padding([0, 18]);
    column![title_bar(title, window), content].height(Fill).into()
}

fn window_square(theme: &Theme, fill: bool) -> container::Style {
    container::Style {
        background: fill.then(|| tone(theme, Tone::TopBar).into()),
        border: Border::default().width(1.0).color(tone(theme, Tone::Secondary)),
        ..Default::default()
    }
}

fn control_icon(action: Action, maximized: bool) -> Element<'static, AppMessage> {
    match action {
        Action::Minimize => container(space::vertical())
            .width(12)
            .height(1)
            .style(ui_surface(Tone::Secondary, false, 0.0))
            .into(),
        Action::ToggleMaximize if maximized => {
            let back = container(container(space::vertical()).width(9).height(9)
                .style(|theme| window_square(theme, false)))
                .padding(Padding { left: 3.0, ..Padding::ZERO });
            let front = container(container(space::vertical()).width(9).height(9)
                .style(|theme| window_square(theme, true)))
                .padding(Padding { top: 3.0, ..Padding::ZERO });
            stack![back, front].width(12).height(12).into()
        }
        Action::ToggleMaximize => container(space::vertical())
            .width(11)
            .height(11)
            .style(|theme| window_square(theme, false))
            .into(),
        _ => icon_tone(Icon::Dismiss, Tone::Secondary, 15.0),
    }
}

fn control(
    id: Id,
    action: Action,
    maximized: bool,
    label: &'static str,
) -> Element<'static, AppMessage> {
    hint(
        button(container(control_icon(action, maximized)).center(Fill))
            .width(CONTROL_WIDTH)
            .height(TOP_BAR - RESIZE_EDGE * 2.0)
            .padding(0)
            .on_press(AppMessage::MainWindowAction(id, action))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        label,
    )
}

fn resize_handle(
    id: Id,
    direction: Direction,
    interaction: Interaction,
    width: iced::Length,
    height: iced::Length,
) -> Element<'static, AppMessage> {
    mouse_area(container(space::vertical()).width(width).height(height))
        .interaction(interaction)
        .on_press(AppMessage::MainWindowAction(id, Action::Resize(direction)))
        .into()
}

fn resize_handles(id: Id) -> Element<'static, AppMessage> {
    use Direction::*;
    use Interaction::{ResizingDiagonallyDown, ResizingDiagonallyUp, ResizingHorizontally, ResizingVertically};
    let edge = iced::Length::Fixed(RESIZE_EDGE);
    column![
        row![
            resize_handle(id, NorthWest, ResizingDiagonallyDown, edge, edge),
            resize_handle(id, North, ResizingVertically, Fill, edge),
            resize_handle(id, NorthEast, ResizingDiagonallyUp, edge, edge),
        ],
        row![
            resize_handle(id, West, ResizingHorizontally, edge, Fill),
            space::horizontal(),
            resize_handle(id, East, ResizingHorizontally, edge, Fill),
        ].height(Fill),
        row![
            resize_handle(id, SouthWest, ResizingDiagonallyUp, edge, edge),
            resize_handle(id, South, ResizingVertically, Fill, edge),
            resize_handle(id, SouthEast, ResizingDiagonallyDown, edge, edge),
        ],
    ].height(Fill).into()
}

pub fn frame(content: Element<'_, AppMessage>, window: Option<Id>, maximized: bool) -> Element<'_, AppMessage> {
    let Some(id) = window else { return content; };
    let controls = row![
        control(id, Action::Minimize, maximized, tr("Minimize window")),
        control(id, Action::ToggleMaximize, maximized, tr(if maximized { "Restore window" } else { "Maximize window" })),
        control(id, Action::Close, maximized, tr("Close window")),
    ].align_y(Alignment::Center);
    let controls = container(controls).align_right(Fill).padding(Padding { top: RESIZE_EDGE, ..Padding::ZERO });
    let mut frame = stack![content, controls].width(Fill).height(Fill);
    if !maximized {
        frame = frame.push(resize_handles(id));
    }
    frame.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_bar_aligns_navigation_and_search_and_reserves_the_controls() {
        let width = 2048.0;
        let layout = BarLayout::new(width);
        assert!(layout.navigation);
        assert!((535.0..=543.0).contains(&layout.search_left));
        assert_eq!(layout.search_width, 800.0);
        assert_eq!(layout.search_left + layout.search_width + layout.after_search + MENU_WIDTH + MENU_TRAILING, width);
    }

    #[test]
    fn compact_bar_keeps_search_and_window_controls_in_one_row() {
        let width = 440.0;
        let layout = BarLayout::new(width);
        assert!(!layout.navigation);
        assert!(layout.search_width >= 150.0);
        assert_eq!(layout.search_left + layout.search_width + layout.after_search + MENU_WIDTH + MENU_TRAILING, width);
    }

    #[test]
    fn window_controls_and_edges_keep_title_bar_and_search_clicks_separate() {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
        use iced::{Event, Point, Rectangle, Size, mouse};

        let mut state = crate::TeamsApp::from_client_id(String::new());
        let id = Id::unique();
        state.main_window = Some(id);
        let size = Size::new(704.0, 450.0);
        let body = iced::widget::column![state.top_bar(size.width), space::vertical()].width(Fill);
        let mut frame = frame(body.into(), Some(id), false);
        let renderer = crate::chat_render_tests::headless_renderer();
        let mut tree = Tree::new(frame.as_widget());
        let node = frame.as_widget_mut().layout(
            &mut tree, &renderer, &layout::Limits::new(Size::ZERO, size),
        );
        let mut click = |point: Point| {
            let mut messages = Vec::new();
            for event in [mouse::Event::ButtonPressed(mouse::Button::Left), mouse::Event::ButtonReleased(mouse::Button::Left)] {
                frame.as_widget_mut().update(
                    &mut tree, &Event::Mouse(event), Layout::new(&node),
                    mouse::Cursor::Available(point), &renderer, &mut clipboard::Null,
                    &mut Shell::new(&mut messages), &Rectangle::with_size(size),
                );
            }
            messages
        };

        assert!(click(Point::new(24.0, 30.0)).iter().any(|message| matches!(message, AppMessage::MainWindowAction(target, Action::Drag) if *target == id)));
        assert!(click(Point::new(128.0, 30.0)).is_empty());
        assert!(matches!(click(Point::new(73.0, 30.0)).as_slice(), [AppMessage::FocusSearch]));
        assert!(matches!(click(Point::new(604.0, 30.0)).as_slice(), [AppMessage::MainWindowAction(_, Action::Minimize)]));
        assert!(matches!(click(Point::new(644.0, 30.0)).as_slice(), [AppMessage::MainWindowAction(_, Action::ToggleMaximize)]));
        assert!(matches!(click(Point::new(684.0, 30.0)).as_slice(), [AppMessage::MainWindowAction(_, Action::Close)]));
        assert!(matches!(click(Point::new(702.0, 448.0)).as_slice(), [AppMessage::MainWindowAction(_, Action::Resize(Direction::SouthEast))]));
        assert!(matches!(click(Point::new(2.0, 128.0)).as_slice(), [AppMessage::MainWindowAction(_, Action::Resize(Direction::West))]));
    }
}
