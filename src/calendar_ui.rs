//! Native calendar surfaces; all date/overlap calculations live in calendar.rs.
use crate::i18n::tr;
use crate::{
    AppMessage, Icon, TeamsApp, Tone, calendar, hint, icon_tone, semibold, ui_button, ui_scroll,
    ui_surface, ui_text,
};
use calendar::{CalendarEvent, CalendarView};
use chrono::{Datelike, Local, Timelike, TimeZone};
use iced::widget::{
    button, checkbox, column, container, mouse_area, pick_list, responsive, row, scrollable, space,
    stack, text, text_input,
};
use iced::{Alignment, Element, Fill};

#[cfg(test)]
thread_local! { static BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn reset_test_builds() {
    BUILDS.with(|count| count.set(0));
}
#[cfg(test)]
pub(crate) fn test_builds() -> u64 {
    BUILDS.with(std::cell::Cell::get)
}

pub const HOUR: f32 = 96.0;
pub const SCHEDULE_HALF: f32 = 22.0;
const DAY: f32 = 24.0 * HOUR;
// Rusty Teams' calendar proportions: compact chrome, a 236px list pane and
// a 50px time gutter leave the grid as the visual focus.
const CALENDAR_PANE_WIDTH: f32 = 236.0;
const CALENDAR_TOOLBAR_HEIGHT: f32 = 52.0;
const CALENDAR_DAY_HEADER_HEIGHT: f32 = 56.0;
const CALENDAR_TIME_GUTTER: f32 = 50.0;

// Owned snapshot lets Iced retain this surface while local video frames update.
struct Model {
    calendar_anchor: chrono::NaiveDate,
    mini_month: chrono::NaiveDate,
    calendar_visible: bool,
    calendar_mode: CalendarView,
    calendar_error: Option<String>,
    calendar_loading: bool,
    calendar_events: std::sync::Arc<Vec<CalendarEvent>>,
    selected_calendar_event: Option<String>,
    calendar_next_link: Option<String>,
    calendar_new_menu_open: bool,
    meet_now: crate::MeetNowState,
    known_chats: Vec<String>,
    now: chrono::DateTime<Local>,
    schedule_open: bool,
    schedule_subject: String,
    schedule_date: String,
    schedule_start: String,
    schedule_end: String,
    schedule_online: bool,
    schedule_busy: bool,
    schedule_error: Option<String>,
    schedule_attendees: Vec<calendar::ScheduleAttendee>,
    schedule_people_query: String,
    schedule_people_results: Vec<calendar::ScheduleAttendee>,
    schedule_people_loading: bool,
    schedule_location: String,
    schedule_body: String,
    schedule_show_as: calendar::ScheduleShowAs,
    schedule_repeat: calendar::ScheduleRepeat,
    schedule_repeat_until: String,
}
pub fn view(state: &TeamsApp) -> Element<'static, AppMessage> {
    #[cfg(test)]
    BUILDS.with(|count| count.set(count.get() + 1));
    let state = Model {
        calendar_anchor: state.calendar_anchor,
        mini_month: state.calendar_mini_month,
        calendar_visible: state.calendar_visible,
        calendar_mode: state.calendar_mode,
        calendar_error: state.calendar_error.clone(),
        calendar_loading: state.calendar_loading,
        calendar_events: std::sync::Arc::new(state.calendar_events.clone()),
        selected_calendar_event: state.selected_calendar_event.clone(),
        calendar_next_link: state.calendar_next_link.clone(),
        calendar_new_menu_open: state.calendar_new_menu_open,
        meet_now: state.meet_now.clone(),
        known_chats:if state.native_call.is_none()&&state.prejoin.is_none(){state.chats.iter().filter(|c|c.hidden!=Some(true)).map(|c|c.id.clone()).collect()}else{Vec::new()},
        now:if state.demo_mode&&std::env::args().any(|a|a=="--calendar-reference-demo"){Local::now().date_naive().and_hms_opt(16,2,0).and_then(|v|Local.from_local_datetime(&v).earliest()).unwrap_or_else(Local::now)}else{Local::now()},
        schedule_open: state.schedule_open,
        schedule_subject: state.schedule_subject.clone(),
        schedule_date: state.schedule_date.clone(),
        schedule_start: state.schedule_start.clone(),
        schedule_end: state.schedule_end.clone(),
        schedule_online: state.schedule_online,
        schedule_busy: state.schedule_busy,
        schedule_error: state.schedule_error.clone(),
        schedule_attendees: state.schedule_attendees.clone(),
        schedule_people_query: state.schedule_people_query.clone(),
        schedule_people_results: state.schedule_people_results.clone(),
        schedule_people_loading: state.schedule_people_loading,
        schedule_location: state.schedule_location.clone(),
        schedule_body: state.schedule_body.clone(),
        schedule_show_as: state.schedule_show_as,
        schedule_repeat: state.schedule_repeat,
        schedule_repeat_until: state.schedule_repeat_until.clone(),
    };
    responsive(move |size| {
        let show_sidebar = size.width >= 1000.0;
        let content_width = size.width - if show_sidebar { CALENDAR_PANE_WIDTH } else { 0.0 };
        let first = state.calendar_mode.first_date(state.calendar_anchor);
        let last = first + chrono::Duration::days(state.calendar_mode.days() - 1);
        let label = period_label(first, last);
        let leading = row![
            hint(
                button(
                    row![
                        icon_tone(Icon::Calendar, Tone::Secondary, 17.0),
                        text(tr("Today")).size(14)
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center)
                )
                .padding([6, 8])
                .on_press(AppMessage::CalendarToday)
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                tr("Go to today")
            ),
            hint(
                button(icon_tone(Icon::ChevronLeft, Tone::Secondary, 17.0))
                    .padding(7)
                    .on_press(AppMessage::CalendarNavigate(-1))
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                tr("Previous period")
            ),
            hint(
                button(icon_tone(Icon::ChevronRight, Tone::Secondary, 17.0))
                    .padding(7)
                    .on_press(AppMessage::CalendarNavigate(1))
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                tr("Next period")
            ),
            text(label).size(14).style(ui_text(Tone::Text)),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        let compact = content_width < 600.0;
        let meet_label = if compact { tr("Meet") } else { tr("Meet now") };
        let meet_button = hint(
            button(row![icon_tone(Icon::Camera,Tone::Secondary,18.0),text(meet_label).size(14)].spacing(8).align_y(Alignment::Center))
                .padding([7, 9]).on_press(AppMessage::OpenMeetNow)
                .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),
            tr("Meet now"),
        );
        let meet_popup = state.meet_now.open.then(|| meet_now_panel(&state.meet_now));
        let meet_control = crate::composer_popover::view_with_close(meet_button, meet_popup, true, AppMessage::CloseMeetNow);
        let new_button = row![
            button(row![icon_tone(Icon::CalendarAdd,Tone::OnPrimary,20.0),text(tr("New")).size(14).font(semibold())].spacing(8).align_y(Alignment::Center))
                .padding([7, 12]).on_press(AppMessage::OpenSchedule)
                .style(ui_button(Tone::Primary,Tone::SelectedHover,Tone::OnPrimary,4.0)),
            hint(button(icon_tone(Icon::ChevronDown,Tone::OnPrimary,15.0))
                .padding([10, 8]).on_press(AppMessage::ToggleCalendarNewMenu)
                .style(ui_button(Tone::Primary,Tone::SelectedHover,Tone::OnPrimary,4.0)), tr("More creation options")),
        ].spacing(1).align_y(Alignment::Center);
        let new_popup = state.calendar_new_menu_open.then(calendar_new_menu);
        let new_control = crate::composer_popover::view_with_close(new_button, new_popup, true, AppMessage::CloseCalendarNewMenu);
        let join: Element<'static,AppMessage> = if compact {
            hint(button(icon_tone(Icon::Link,Tone::Secondary,18.0)).padding(7)
                .on_press(AppMessage::OpenMeetingEntry)
                .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),tr("Join with a link")).into()
        } else {
            button(text(tr("Join with a link")).size(13)).padding([7, 10])
                .on_press(AppMessage::OpenMeetingEntry).style(crate::appearance::ui_filter(false)).into()
        };
        let controls = row![
            pick_list(
                CalendarView::ALL,
                Some(state.calendar_mode),
                AppMessage::CalendarModeChanged
            )
            .text_size(13)
            .padding([6, 8])
            .style(crate::appearance::ui_pick_list()),
            hint(
                button(icon_tone(Icon::Refresh, Tone::Secondary, 17.0))
                    .padding(7)
                    .on_press_maybe(
                        (!state.calendar_loading).then_some(AppMessage::RefreshCalendar)
                    )
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                tr("Refresh calendar")
            ),
            join,
            meet_control,
            new_control,
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let toolbar: Element<'static, AppMessage> = if content_width < 1050.0 {
            column![leading, controls].spacing(8).into()
        } else {
            row![leading, space::horizontal(), controls]
                .align_y(Alignment::Center)
                .into()
        };
        let mut body = column![
            container(toolbar)
                .padding(if content_width < 1050.0 { [10, 16] } else { [8, 16] })
                .height(if content_width<1050.0{96.0}else{CALENDAR_TOOLBAR_HEIGHT})
                .center_y(if content_width<1050.0{96.0}else{CALENDAR_TOOLBAR_HEIGHT})
                .width(Fill)
                .style(ui_surface(Tone::CalendarToolbar, false, 0.0)),
            crate::divider()
        ]
        .spacing(0)
        .height(Fill);
        // Keep this slot in the tree even when empty: inserting/removing a
        // sibling used to recreate the grid and reset its scroll position.
        let mut status = column![].spacing(0);
        if let Some(error) = &state.calendar_error {
            status = status.push(
                container(
                    row![
                        text(error.clone())
                            .size(13)
                            .style(ui_text(Tone::Danger))
                            .width(Fill),
                        button(text(tr("Try again")).size(13))
                            .on_press(AppMessage::RefreshCalendar)
                            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                    ]
                    .spacing(12),
                )
                .padding(12)
                .style(ui_surface(Tone::Surface, true, 6.0)),
            );
        }
        if state.calendar_loading {
            status = status.push(
                text(tr("Refreshing calendar…"))
                    .size(12)
                    .style(ui_text(Tone::Muted)),
            );
        }
        body = body.push(container(status).width(Fill));
        let calendar = if state.calendar_mode == CalendarView::Agenda || content_width < 760.0 {
            agenda(&state)
        } else {
            week(&state)
        };
        let selected_event = state
            .selected_calendar_event
            .as_deref()
            .and_then(|id| state.calendar_events.iter().find(|event| event.id == id));
        body = body.push(calendar);
        if state.calendar_next_link.is_some() {
            body = body.push(
                button(text(tr("Load more events")).size(13))
                    .on_press_maybe(
                        (!state.calendar_loading).then_some(AppMessage::LoadMoreCalendar),
                    )
                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
            );
        }
        let main = container(body).width(Fill).height(Fill).style(ui_surface(
            Tone::CalendarCanvas,
            false,
            0.0,
        ));
        let base: Element<'static, AppMessage> = if show_sidebar {
            row![
                month_sidebar(&state),
                container(space::vertical())
                    .width(1)
                    .height(Fill)
                    .style(ui_surface(Tone::Border, false, 0.0)),
                main
            ]
            .height(Fill)
            .into()
        } else {
            main.into()
        };
        let overlay: Element<'static, AppMessage> = if state.schedule_open {
            container(iced::widget::opaque(schedule_dialog(&state)))
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill)
                .padding(24)
                .into()
        } else {
            selected_event.map(|event|{
                let card_width=470.0_f32.min((size.width-32.0).max(1.0));
                let card_height=(size.height-112.0).clamp(160.0,580.0);
                let day=event.date_key().and_then(|key|chrono::NaiveDate::parse_from_str(&key,"%Y-%m-%d").ok()).map(|date|(date-first).num_days());
                let at=details_position(size,if show_sidebar{CALENDAR_PANE_WIDTH+1.0}else{0.0},if content_width>=760.0{day}else{None},state.calendar_mode.days(),iced::Size::new(card_width,card_height));
                let can_chat=event.join_url.as_deref().and_then(calendar::join_thread_id).is_some_and(|thread|state.known_chats.contains(&thread));
                stack![
                    mouse_area(container(space::horizontal()).width(Fill).height(Fill)).on_press(AppMessage::CloseCalendarEvent),
                    container(iced::widget::opaque(details(event,state.now,can_chat,card_width,card_height)))
                        .padding(iced::Padding{top:at.y,left:at.x,..Default::default()}).width(Fill).height(Fill),
                ].width(Fill).height(Fill).into()
            }).unwrap_or_else(||space::vertical().into())
        };
        stack![base, overlay].width(Fill).height(Fill).into()
    })
    .into()
}

fn calendar_new_menu() -> Element<'static, AppMessage> {
    container(
        button(row![icon_tone(Icon::Calendar,Tone::Secondary,18.0),text(tr("Event")).size(14)]
            .spacing(12).align_y(Alignment::Center))
            .on_press(AppMessage::OpenSchedule).padding([10, 14]).width(Fill)
            .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),
    ).width(220).padding(4).style(ui_surface(Tone::Surface,true,6.0)).into()
}

fn meet_now_panel(state: &crate::MeetNowState) -> Element<'static, AppMessage> {
    let mut content = column![
        row![text(tr("Meet now")).size(18).font(semibold()).style(ui_text(Tone::Text)).width(Fill),
            hint(button(icon_tone(Icon::Dismiss,Tone::Secondary,18.0)).padding(4)
                .on_press(AppMessage::CloseMeetNow)
                .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),tr("Close"))]
            .align_y(Alignment::Center),
        text(tr("Meeting name")).size(13).style(ui_text(Tone::Secondary)),
    ].spacing(12);
    if state.meeting.is_none() {
        let mut input = text_input(tr("Meeting name"), &state.subject).id("meet-now-subject")
            .size(14).padding([9,10]).width(Fill);
        if !state.busy { input = input.on_input(AppMessage::MeetNowSubjectChanged); }
        content = content.push(input);
    } else {
        content = content.push(text(state.subject.clone()).size(14).style(ui_text(Tone::Text)));
    }
    if state.busy {
        content = content.push(text(tr("Creating meeting…")).size(13).style(ui_text(Tone::Secondary)));
    }
    if state.meeting.is_some() {
        content = content.push(text(tr("Link ready to share")).size(13).style(ui_text(Tone::Secondary)))
            .push(row![
                button(row![icon_tone(Icon::Copy,Tone::Text,16.0),text(if state.copied {tr("Link copied")} else {tr("Copy link")}).size(13)].spacing(7).align_y(Alignment::Center))
                    .padding([8,10]).on_press(AppMessage::MeetNowCopy).style(ui_button(Tone::Hover,Tone::SelectedHover,Tone::Text,4.0)),
                button(text(tr("Start meeting")).size(13).font(semibold())).padding([8,12])
                    .on_press(AppMessage::MeetNowCreate(true)).style(ui_button(Tone::Primary,Tone::SelectedHover,Tone::OnPrimary,4.0)),
            ].spacing(8))
            .push(button(text(tr("Create another meeting")).size(13)).padding([5,0]).on_press(AppMessage::MeetNowNew)
                .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)));
    } else {
        let can_create = !state.busy && !state.subject.trim().is_empty();
        content = content.push(row![
            button(text(tr("Get link")).size(13)).padding([8,12])
                .on_press_maybe(can_create.then_some(AppMessage::MeetNowCreate(false)))
                .style(ui_button(Tone::Hover,Tone::SelectedHover,Tone::Text,4.0)),
            button(text(tr("Start meeting")).size(13).font(semibold())).padding([8,12])
                .on_press_maybe(can_create.then_some(AppMessage::MeetNowCreate(true)))
                .style(ui_button(Tone::Primary,Tone::SelectedHover,Tone::OnPrimary,4.0)),
        ].spacing(8));
    }
    if let Some(error) = &state.error {
        content = content.push(text(error.clone()).size(13).style(ui_text(Tone::Danger)));
    }
    container(content).padding(18).width(360).style(ui_surface(Tone::Surface,true,6.0)).into()
}

fn event_button(
    event: &CalendarEvent,
    height: f32,
    selected: bool,
) -> Element<'static, AppMessage> {
    let cancelled = event.cancelled;
    let subject = if event.cancelled {
        format!("Canceled · {}", event.subject)
    } else {
        event.subject.clone()
    };
    let mut label = column![
        text(subject)
            .size(12)
            .line_height(1.1)
            .style(ui_text(Tone::Text))
    ]
    .spacing(3);
    if height >= 48.0 {
        label = label.push(text(event.time_range().unwrap_or_default()).size(10).style(ui_text(Tone::Secondary)));
    }
    if height >= 78.0 {
        if let Some(organizer) = &event.organizer {
            label = label.push(text(organizer.clone()).size(10).style(ui_text(Tone::Muted)));
        }
    }
    let content = container(label).clip(true).height(Fill).width(Fill);
    hint(
        button(
            row![
                container(space::vertical())
                    .width(4)
                    .height(Fill)
                    .style(ui_surface(Tone::CalendarAccent, false, 2.0)),
                container(content).width(Fill).height(Fill).padding([2, 6])
            ]
            .spacing(0),
        )
        .width(Fill)
        .height(height.max(18.0))
        .padding(0)
        .on_press(AppMessage::SelectCalendarEvent(event.id.clone()))
        .style(move |theme, status| {
            let mut style = ui_button(
                if cancelled {
                    Tone::Transparent
                } else if selected {
                    Tone::CalendarSelectedEvent
                } else {
                    Tone::CalendarEvent
                },
                Tone::SelectedHover,
                Tone::Text,
                6.0,
            )(theme, status);
            style.border = iced::Border::default()
                .rounded(6)
                .width(if selected || cancelled { 1.0 } else { 0.0 })
                .color(crate::appearance::tone(theme, if selected{Tone::Text}else if cancelled{Tone::Border}else{Tone::CalendarAccent}));
            style
        }),
        format!(
            "{}\n{}",
            event.subject,
            event.time_range().unwrap_or_default()
        ),
    )
}

fn week(state: &Model) -> Element<'static, AppMessage> {
    let first = state.calendar_mode.first_date(state.calendar_anchor);
    let days = state.calendar_mode.days();
    let events = if state.calendar_visible {
        std::sync::Arc::clone(&state.calendar_events)
    } else {
        std::sync::Arc::new(Vec::new())
    };
    let mut headings = row![
        container(space::vertical())
            .width(CALENDAR_TIME_GUTTER)
            .height(CALENDAR_DAY_HEADER_HEIGHT)
            .style(ui_surface(Tone::CalendarToolbar, false, 0.0))
    ]
    .spacing(1);
    let mut has_all_day=false;
    let mut all_day = row![container(space::vertical()).width(CALENDAR_TIME_GUTTER)].spacing(1);
    let hours = column((0..24).map(|hour| {
        container(
            text(format!("{hour:02}"))
                .size(12)
                .style(ui_text(Tone::Muted)),
        )
        .padding([6, 16])
        .height(HOUR)
        .width(CALENDAR_TIME_GUTTER)
        .into()
    }))
    .height(DAY);
    let mut grid =
        row![container(hours).style(ui_surface(Tone::CalendarToolbar, false, 0.0))].spacing(1);
    for offset in 0..days {
        let date = first + chrono::Duration::days(offset);
        let weekdays = [
            tr("Monday"),
            tr("Tuesday"),
            tr("Wednesday"),
            tr("Thursday"),
            tr("Friday"),
            tr("Saturday"),
            tr("Sunday"),
        ];
        let today = date == state.now.date_naive();
        headings = headings.push(
            container(column![
                container(space::horizontal())
                    .height(3)
                    .width(Fill)
                    .style(ui_surface(
                        if today {
                            Tone::CalendarAccent
                        } else {
                            Tone::Transparent
                        },
                        false,
                        0.0
                    )),
                container(
                    column![
                        text(format!("{:02}",date.day()))
                            .size(20)
                            .style(ui_text(Tone::CalendarAccent)),
                        text(weekdays[date.weekday().num_days_from_monday() as usize])
                            .size(12)
                            .style(ui_text(if today {
                                Tone::CalendarAccent
                            } else {
                                Tone::Muted
                            }))
                    ]
                    .spacing(0)
                )
                .padding([6, 12])
            ])
            .width(Fill)
            .height(CALENDAR_DAY_HEADER_HEIGHT)
            .style(ui_surface(if today{Tone::CalendarTodayCanvas}else{Tone::CalendarCanvas}, false, 0.0)),
        );
        let mut day_events = column![space::vertical().height(10)].spacing(3);
        let placed = calendar::placements(&events, date);
        for (index, event) in events.iter().enumerate().filter(|(index, event)| {
            event.all_day && calendar::on_date(event, date)
                || !placed.iter().any(|item| item.event == *index)
                    && event.date_key().as_deref() == Some(date.to_string().as_str())
        }) {
            let _ = index;has_all_day=true;
            day_events = day_events.push(event_button(
                event,
                26.0,
                state.selected_calendar_event.as_deref() == Some(event.id.as_str()),
            ));
        }
        all_day = all_day.push(container(day_events).width(Fill).padding([2, 2]));
        let events = std::sync::Arc::clone(&events);
        let selected = state.selected_calendar_event.clone();
        let draft = draft_block(state, date);
        let now=state.now;
        let now = state.now;
        grid = grid.push(
            responsive(move |size| {
                let lines: Vec<Element<'_, AppMessage>> = (0..48)
                    .map(|half| {
                        mouse_area(
                            column![
                                container(space::horizontal())
                                    .height(1)
                                    .width(Fill)
                                    .style(ui_surface(
                                        if half % 2 == 0 {
                                            Tone::CalendarGrid
                                        } else {
                                            Tone::CalendarMinor
                                        },
                                        false,
                                        0.0
                                    )),
                                container(space::vertical())
                                    .height(HOUR / 2.0 - 1.0)
                                    .width(Fill)
                                    .style(ui_surface(if today{Tone::CalendarTodayCanvas}else{Tone::CalendarCanvas}, false, 0.0)),
                            ]
                            .height(HOUR / 2.0)
                            .width(Fill),
                        )
                        .on_double_click(AppMessage::ScheduleAt(date, (half as u16) * 30))
                        .interaction(iced::mouse::Interaction::Pointer)
                        .into()
                    })
                    .collect();
                let mut layers = vec![column(lines).width(Fill).height(DAY).into()];
                for placement in calendar::placements(&events, date) {
                    let cell = (size.width - 4.0).max(1.0) / placement.lanes as f32;
                    let event = &events[placement.event];
                    let top = f32::from(placement.start) * HOUR / 60.0;
                    let height = f32::from(placement.end - placement.start) * HOUR / 60.0;
                    let item = event_button(
                        event,
                        (height - 2.0).max(18.0),
                        selected.as_deref() == Some(event.id.as_str()),
                    );
                    layers.push(
                        container(item)
                            .width(cell.max(1.0))
                            .padding(iced::Padding::ZERO)
                            .into(),
                    );
                    // Position the native button without an estimated history spacer.
                    let last = layers.pop().unwrap();
                    layers.push(
                        container(last)
                            .padding(iced::Padding {
                                top,
                                left: placement.lane as f32 * cell + 2.0,
                                ..Default::default()
                            })
                            .into(),
                    );
                }
                if let Some((start, end, label)) = draft.clone() {
                    let top = f32::from(start) * HOUR / 60.0;
                    let height = f32::from(end.saturating_sub(start)) * HOUR / 60.0;
                    layers.push(
                        container(
                            container(text(label).size(12).style(ui_text(Tone::Text)))
                                .width(Fill)
                                .height(height.max(18.0))
                                .padding([4, 8])
                                .style(|theme| {
                                    let mut fill =
                                        crate::appearance::tone(theme, Tone::CalendarAccent);
                                    fill.a = 0.45;
                                    iced::widget::container::Style {
                                        background: Some(fill.into()),
                                        border: iced::Border::default()
                                            .rounded(4)
                                            .width(1)
                                            .color(crate::appearance::tone(
                                                theme,
                                                Tone::CalendarAccent,
                                            )),
                                        ..Default::default()
                                    }
                                }),
                        )
                        .width(Fill)
                        .padding(iced::Padding {
                            top,
                            left: 2.0,
                            right: 2.0,
                            ..Default::default()
                        })
                        .into(),
                    );
                }
                if today {
                    let top = (now.hour() * 60 + now.minute()) as f32 * HOUR / 60.0;
                    layers.push(
                        container(
                            row![
                                container(space::vertical())
                                    .width(8)
                                    .height(8)
                                    .style(ui_surface(Tone::CalendarAccent, false, 4.0)),
                                container(space::horizontal())
                                    .height(2)
                                    .width(Fill)
                                    .style(ui_surface(Tone::CalendarAccent, false, 0.0))
                            ]
                            .spacing(0)
                            .align_y(Alignment::Center),
                        )
                        .padding(iced::Padding {
                            top,
                            ..Default::default()
                        })
                        .into(),
                    );
                }
                stack(layers).width(Fill).height(DAY).into()
            })
            .height(DAY)
            .width(Fill),
        );
    }
    column![
        headings,
        container(scrollable(all_day).height(iced::Shrink).style(ui_scroll))
            .max_height(if has_all_day{112}else{0})
            .width(Fill),
        scrollable(container(grid.height(DAY)).style(ui_surface(Tone::CalendarGrid, false, 0.0)))
            .id("calendar-grid")
            .height(Fill)
            .style(ui_scroll),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill)
    .into()
}

fn agenda(state: &Model) -> Element<'static, AppMessage> {
    let first = state.calendar_mode.first_date(state.calendar_anchor);
    let mut list = column![].spacing(8);
    let mut count = 0;
    for offset in 0..state.calendar_mode.days() {
        let date = first + chrono::Duration::days(offset);
        let mut events: Vec<_> = state
            .calendar_events
            .iter()
            .filter(|event| {
                state.calendar_visible
                    && (calendar::on_date(event, date)
                        || event.date_key().as_deref() == Some(date.to_string().as_str()))
            })
            .collect();
        if events.is_empty() {
            continue;
        }
        events.sort_by_key(|event| {
            (
                !event.all_day,
                event.start.as_ref().map(|start| start.value.clone()),
            )
        });
        list = list.push(
            text(calendar::date_label(date))
                .size(15)
                .font(semibold())
                .style(ui_text(Tone::Secondary)),
        );
        for event in events {
            count += 1;
            list = list.push(event_button(
                event,
                64.0,
                state.selected_calendar_event.as_deref() == Some(event.id.as_str()),
            ));
        }
    }
    let undated: Vec<_> = state
        .calendar_events
        .iter()
        .filter(|event| state.calendar_visible && event.date_key().is_none())
        .collect();
    if !undated.is_empty() {
        list = list.push(
            text(tr("No time available"))
                .size(15)
                .font(semibold())
                .style(ui_text(Tone::Secondary)),
        );
        for event in undated {
            count += 1;
            list = list.push(event_button(
                event,
                64.0,
                state.selected_calendar_event.as_deref() == Some(event.id.as_str()),
            ));
        }
    }
    if count == 0 && !state.calendar_loading && state.calendar_error.is_none() {
        list = list.push(
            container(
                text(tr("No events in this period"))
                    .size(16)
                    .style(ui_text(Tone::Muted)),
            )
            .padding(30)
            .center_x(Fill),
        );
    }
    scrollable(list.padding(16))
        .height(Fill)
        .width(Fill)
        .style(ui_scroll)
        .into()
}

fn details_position(viewport:iced::Size,sidebar:f32,day:Option<i64>,days:i64,card:iced::Size)->iced::Point {
    let max_x=(viewport.width-card.width-16.).max(16.);
    let x=day.filter(|day|*day>=0&&*day<days).map(|day|{
        let width=(viewport.width-sidebar-60.).max(1.)/days.max(1) as f32;
        let left=sidebar+60.+day as f32*width;
        let right=left+width+4.;
        if right<=max_x {right}else{left-card.width-4.}
    }).unwrap_or((viewport.width-card.width)/2.).clamp(16.,max_x);
    iced::Point::new(x,92.0_f32.min((viewport.height-card.height-16.).max(16.)))
}
fn detail_when(event:&CalendarEvent)->String {
    let date=event.date_key().and_then(|key|chrono::NaiveDate::parse_from_str(&key,"%Y-%m-%d").ok()).map(|date|{
        let weekday=[tr("Mon"),tr("Tue"),tr("Wed"),tr("Thu"),tr("Fri"),tr("Sat"),tr("Sun")][date.weekday().num_days_from_monday() as usize];
        format!("{weekday}, {}",date.format("%d/%m/%Y"))
    });
    [date,event.time_range()].into_iter().flatten().collect::<Vec<_>>().join("  ")
}
fn details(event:&CalendarEvent,now:chrono::DateTime<Local>,can_chat:bool,width:f32,height:f32)->iced::widget::Container<'static,AppMessage> {
    let when=detail_when(event);
    let mut heading=column![row![text(event.subject.clone()).size(24).font(semibold()).width(Fill),
        hint(button(icon_tone(Icon::Dismiss,Tone::Secondary,18.0)).padding(3).on_press(AppMessage::CloseCalendarEvent)
            .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),tr("Close details (Esc)"))].spacing(12).align_y(Alignment::Start)].spacing(12);
    if let Some(label)=event.temporal_label(now){heading=heading.push(row![icon_tone(Icon::Clock,if event.cancelled{Tone::Danger}else{Tone::CalendarAccent},12.),text(label).size(14).style(ui_text(if event.cancelled{Tone::Danger}else{Tone::CalendarAccent}))].spacing(5).align_y(Alignment::Center));}
    let mut actions=row![].spacing(8).align_y(Alignment::Center);
    if let Some(link)=event.join_url.as_ref().filter(|link|!event.cancelled&&calendar::valid_teams_join_url(link)) {
        actions=actions.push(button(row![icon_tone(Icon::Camera,Tone::CalendarButtonText,18.),text(tr("Join meeting")).size(16).font(semibold())].spacing(7).align_y(Alignment::Center))
            .padding([8,14]).on_press(AppMessage::OpenCalendarMeeting(link.clone())).style(ui_button(Tone::CalendarAccent,Tone::CalendarAccent,Tone::CalendarButtonText,4.0)));
    }
    if can_chat && !event.cancelled {actions=actions.push(button(row![icon_tone(Icon::Chat,Tone::Text,18.),text("Chat").size(16).font(semibold())].spacing(7).align_y(Alignment::Center)).padding([8,16]).on_press(AppMessage::OpenCalendarEventChat(event.id.clone())).style(|theme,status|{let mut style=ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)(theme,status);style.border=iced::Border::default().rounded(4).width(1).color(crate::appearance::tone(theme,Tone::MenuBorder));style}));}
    heading=heading.push(actions);
    let section=|icon,value:String|row![container(icon_tone(icon,Tone::Secondary,20.)).width(24),text(value).size(16).style(ui_text(Tone::Text)).width(Fill)].spacing(10).align_y(Alignment::Start);
    let mut body=column![heading,crate::divider()].spacing(20);
    if !when.is_empty(){body=body.push(section(Icon::Clock,when));}
    if let Some(location)=event.location.as_ref().filter(|v|!v.trim().is_empty()) {body=body.push(section(Icon::Location,location.clone()));}
    else if event.is_online{body=body.push(section(Icon::Location,tr("Microsoft Teams meeting").into()));}
    if let Some(organizer)=event.organizer.as_ref().filter(|v|!v.trim().is_empty()) {
        body=body.push(crate::divider()).push(section(Icon::GroupMembers,format!("Organizer · {organizer}")));
    }
    if let Some(preview)=event.preview.as_ref().filter(|v|!v.trim().is_empty()) {
        body=body.push(crate::divider()).push(section(Icon::Description,crate::preview_line(preview,480)));
    }
    container(column![container(space::horizontal()).height(4).width(Fill).style(ui_surface(Tone::CalendarAccent,false,2.0)),
        container(scrollable(container(body).padding(iced::Padding{right:10.,..Default::default()})).height(iced::Length::Shrink).style(ui_scroll).direction(crate::scroll_axis())).padding(20).width(Fill).max_height((height-4.).max(1.)),
    ]).width(width).max_height(height).style(|theme|{let mut style=ui_surface(Tone::Surface,true,4.0)(theme);style.shadow=iced::Shadow{color:iced::Color::BLACK.scale_alpha(0.3),offset:iced::Vector::new(0.,4.),blur_radius:16.};style})
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];
pub fn month_cells(month: chrono::NaiveDate) -> Vec<chrono::NaiveDate> {
    let first = month.with_day(1).unwrap_or(month);
    let start = first
        .checked_sub_signed(chrono::Duration::days(i64::from(
            first.weekday().num_days_from_sunday(),
        )))
        .unwrap_or(first);
    let days = first
        .checked_add_months(chrono::Months::new(1))
        .map(|next| (next - first).num_days())
        .unwrap_or(31);
    let count = ((i64::from(first.weekday().num_days_from_sunday()) + days + 6) / 7) * 7;
    (0..count)
        .filter_map(|i| start.checked_add_signed(chrono::Duration::days(i)))
        .collect()
}
fn period_label(first: chrono::NaiveDate, last: chrono::NaiveDate) -> String {
    if first == last {
        calendar::date_label(first)
    } else if first.month() == last.month() && first.year() == last.year() {
        crate::i18n::format("{month} {first}–{last}, {year}", &[
            ("month", &tr(MONTHS[first.month0() as usize])),
            ("first", &first.day()), ("last", &last.day()), ("year", &first.year()),
        ])
    } else {
        format!(
            "{} – {}",
            calendar::date_label(first),
            calendar::date_label(last)
        )
    }
}
fn draft_block(state: &Model, date: chrono::NaiveDate) -> Option<(u16, u16, String)> {
    if !state.schedule_open {
        return None;
    }
    let parsed = calendar::parse_schedule_date(&state.schedule_date).ok()?;
    if parsed != date {
        return None;
    }
    let start = calendar::clock_minutes(&state.schedule_start)?;
    let mut end = calendar::clock_minutes(&state.schedule_end).unwrap_or(start.saturating_add(30));
    if end <= start {
        end = start.saturating_add(30).min(24 * 60);
    }
    let label = if state.schedule_subject.trim().is_empty() {
        format!("{} – {}", state.schedule_start, state.schedule_end)
    } else {
        state.schedule_subject.clone()
    };
    Some((start, end, label))
}

fn schedule_line<'a>(content: impl Into<Element<'a, AppMessage>>) -> Element<'a, AppMessage> {
    column![
        content.into(),
        container(space::horizontal())
            .height(1)
            .width(Fill)
            .style(|theme: &iced::Theme| {
                let mut style = container::Style::default();
                style.background =
                    Some(theme.extended_palette().background.weak.color.into());
                style
            }),
    ]
    .width(Fill)
    .into()
}

fn schedule_dialog(state: &Model) -> Element<'static, AppMessage> {
    let when = format!(
        "{}    {} – {}",
        calendar::schedule_day_label(&state.schedule_date),
        state.schedule_start,
        state.schedule_end
    );
    let mut people = column![].spacing(4);
    for (index, person) in state.schedule_attendees.iter().enumerate() {
        people = people.push(
            row![
                text(person.name.clone()).size(13).width(Fill),
                button(text(if person.required { tr("Required") } else { tr("Optional") }).size(12))
                    .padding([2, 8])
                    .on_press(AppMessage::ScheduleAttendeeRequired(index, !person.required))
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Secondary, 4.0)),
                button(text("×").size(14))
                    .padding([2, 6])
                    .on_press(AppMessage::ScheduleRemoveAttendee(index))
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
            ]
            .align_y(Alignment::Center),
        );
    }
    if state.schedule_people_loading {
        people = people.push(text(tr("Finding people…")).size(12).style(ui_text(Tone::Muted)));
    }
    for person in &state.schedule_people_results {
        people = people.push(
            button(
                column![
                    text(person.name.clone()).size(13),
                    text(person.email.clone()).size(11).style(ui_text(Tone::Muted)),
                ]
                .spacing(1),
            )
            .width(Fill)
            .padding([4, 2])
            .on_press(AppMessage::SchedulePickPerson(
                person.name.clone(),
                person.email.clone(),
            ))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        );
    }
    let mut tools = row![
        container(text(tr("Event")).size(13).style(ui_text(Tone::Text)))
            .padding([6, 10])
            .style(ui_surface(Tone::Hover, false, 4.0)),
        pick_list(
            calendar::ScheduleRepeat::ALL,
            Some(state.schedule_repeat),
            AppMessage::ScheduleRepeatChanged,
        )
        .text_size(13)
        .padding([4, 8])
        .width(130)
        .style(crate::appearance::ui_pick_list()),
        pick_list(
            calendar::ScheduleShowAs::ALL,
            Some(state.schedule_show_as),
            AppMessage::ScheduleShowAsChanged,
        )
        .text_size(13)
        .padding([4, 8])
        .width(150)
        .style(crate::appearance::ui_pick_list()),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if state.schedule_repeat != calendar::ScheduleRepeat::Once {
        tools = tools.push(
            text_input(tr("Until"), &state.schedule_repeat_until)
                .on_input(AppMessage::ScheduleRepeatUntilChanged)
                .padding([4, 8])
                .size(13)
                .width(110),
        );
    }
    let description = container(
        column![
            text_input(tr("Add a description"), &state.schedule_body)
                .on_input(AppMessage::ScheduleBodyChanged)
                .padding([8, 10])
                .size(14)
                .width(Fill)
                .style(|theme, status| {
                    let mut style = iced::widget::text_input::default(theme, status);
                    style.background = iced::Color::TRANSPARENT.into();
                    style.border.width = 0.0;
                    style
                }),
            space::vertical(),
            container(
                text("B   I   U   S   •   ≡")
                    .size(12)
                    .style(ui_text(Tone::Muted)),
            )
            .width(Fill)
            .padding([6, 10])
            .style(ui_surface(Tone::Canvas, false, 0.0)),
        ]
        .height(168),
    )
    .width(Fill)
    .height(168)
    .style(ui_surface(Tone::Canvas, true, 4.0));
    let mut form = column![
        tools,
        schedule_line(
            text_input(tr("Add a title"), &state.schedule_subject)
                .id("schedule-subject")
                .on_input(AppMessage::ScheduleSubjectChanged)
                .padding([10, 2])
                .size(16)
                .width(Fill)
                .style(plain_input),
        ),
        schedule_line(
            text_input(
                tr("Invite required attendees"),
                &state.schedule_people_query,
            )
            .on_input(AppMessage::SchedulePeopleQuery)
            .on_submit(AppMessage::ScheduleCommitPerson)
            .padding([8, 2])
            .size(14)
            .width(Fill)
            .style(plain_input),
        ),
        people,
        schedule_line(
            row![
                text(when).size(14).style(ui_text(Tone::Text)).width(Fill),
                text(tr("Scheduling assistant")).size(13).style(ui_text(Tone::Muted)),
            ]
            .align_y(Alignment::Center)
            .padding([8, 2]),
        ),
        schedule_line(
            text_input(tr("Add a room or location"), &state.schedule_location)
                .on_input(AppMessage::ScheduleLocationChanged)
                .padding([8, 2])
                .size(14)
                .width(Fill)
                .style(plain_input),
        ),
        row![
            checkbox(state.schedule_online)
                .size(16)
                .on_toggle(AppMessage::ScheduleOnlineChanged),
            text(tr("Microsoft Teams meeting"))
                .size(14)
                .style(ui_text(Tone::Text)),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding([6, 0]),
        description,
        button(
            row![text(tr("Add an agenda")).size(14)]
                .width(Fill)
                .padding([8, 4]),
        )
        .width(Fill)
        .on_press(AppMessage::ScheduleAddAgenda)
        .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Secondary, 4.0)),
    ]
    .spacing(10)
    .width(Fill);
    if let Some(error) = &state.schedule_error {
        form = form.push(text(error.clone()).size(13).style(ui_text(Tone::Danger)));
    }
    let card = column![
        row![
            text(tr("New event")).size(18).font(semibold()).width(Fill),
            button(text("×").size(18))
                .padding(4)
                .on_press_maybe((!state.schedule_busy).then_some(AppMessage::CloseSchedule))
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        ]
        .align_y(Alignment::Center),
        row![
            scrollable(form).height(540).width(Fill),
            schedule_day_preview(state),
        ]
        .spacing(18)
        .align_y(Alignment::Start),
    ]
    .spacing(14)
    .width(920);
    container(card)
        .padding(18)
        .width(960)
        .style(ui_surface(Tone::Surface, true, 8.0))
        .into()
}

fn plain_input(theme: &iced::Theme, status: iced::widget::text_input::Status) -> iced::widget::text_input::Style {
    let mut style = iced::widget::text_input::default(theme, status);
    style.background = iced::Color::TRANSPARENT.into();
    style.border.width = 0.0;
    style
}

fn schedule_day_preview(state: &Model) -> Element<'static, AppMessage> {
    let date = calendar::parse_schedule_date(&state.schedule_date).ok();
    let start = calendar::clock_minutes(&state.schedule_start);
    let end = calendar::clock_minutes(&state.schedule_end);
    let range = match (start, end) {
        (Some(start), Some(end)) => {
            let end = if end <= start {
                start.saturating_add(30)
            } else {
                end
            };
            Some((start, end))
        }
        _ => None,
    };
    let date_key = date.map(|date| date.format("%Y-%m-%d").to_string());
    let events: Vec<(u16, u16, String)> = state
        .calendar_events
        .iter()
        .filter(|event| date_key.as_deref() == event.date_key().as_deref())
        .filter_map(|event| {
            let (start, end) = event.local_span()?;
            Some((start, end, event.subject.clone()))
        })
        .collect();
    let mut hours = column![].spacing(0);
    for hour in 0..24u16 {
        for half in [0u16, 30] {
            let minute = hour * 60 + half;
            let selected = range
                .map(|(start, end)| minute >= start && minute < end)
                .unwrap_or(false);
            let first_selected = selected && range.map(|(start, _)| minute == start).unwrap_or(false);
            let event = events.iter().any(|(start, end, _)| minute >= *start && minute < *end);
            let subject = events.iter().find_map(|(start, end, subject)| {
                (minute >= *start && minute < *end).then(|| subject.clone())
            });
            let label = if first_selected {
                range
                    .map(|(start, end)| {
                        format!("{:02}:{:02} – {:02}:{:02}", start / 60, start % 60, end / 60, end % 60)
                    })
                    .unwrap_or_default()
            } else if let Some(subject) = subject.filter(|_| !selected) {
                subject
            } else if half == 0 {
                format!("{hour}")
            } else {
                String::new()
            };
            hours = hours.push(
                button(text(label).size(11))
                    .width(Fill)
                    .height(SCHEDULE_HALF)
                    .padding([2, 8])
                    .on_press_maybe(date.map(|date| AppMessage::ScheduleAt(date, minute)))
                    .style(move |theme, status| {
                        let mut style = ui_button(
                            if selected {
                                Tone::CalendarAccent
                            } else if event {
                                Tone::CalendarEvent
                            } else {
                                Tone::CalendarCanvas
                            },
                            Tone::Hover,
                            if selected { Tone::OnPrimary } else { Tone::Muted },
                            0.0,
                        )(theme, status);
                        if selected {
                            style.background = Some(
                                iced::Color {
                                    r: 0.20,
                                    g: 0.72,
                                    b: 0.38,
                                    a: 1.0,
                                }
                                .into(),
                            );
                            style.text_color = iced::Color::BLACK;
                        }
                        style
                    }),
            );
        }
    }
    column![
        row![
            text(calendar::schedule_preview_heading(&state.schedule_date))
                .size(13)
                .style(ui_text(Tone::Text))
                .width(Fill),
            button(text(if state.schedule_busy { tr("Saving…") } else { tr("Save") }).size(13))
                .padding([6, 14])
                .on_press_maybe((!state.schedule_busy).then_some(AppMessage::SubmitSchedule))
                .style(ui_button(Tone::Primary, Tone::Primary, Tone::OnPrimary, 4.0)),
        ]
        .align_y(Alignment::Center),
        scrollable(hours)
            .id("schedule-hours")
            .height(500)
            .width(220)
            .style(ui_scroll),
    ]
    .spacing(8)
    .width(230)
    .into()
}

fn month_sidebar(state: &Model) -> Element<'static, AppMessage> {
    let month = state.mini_month;
    let mut month_grid = column![].spacing(2);
    let mut week_labels = row![].spacing(0);
    for label in ["D", "S", "T", "Q", "Q", "S", "S"] {
        week_labels = week_labels.push(
            container(text(label).size(11).style(ui_text(Tone::Muted)))
                .width(Fill)
                .center_x(Fill),
        );
    }
    month_grid = month_grid.push(week_labels);
    let first = if state.calendar_mode == CalendarView::Day {
        state.calendar_anchor
    } else {
        state
            .calendar_anchor
            .checked_sub_signed(chrono::Duration::days(i64::from(
                state.calendar_anchor.weekday().num_days_from_sunday(),
            )))
            .unwrap_or(state.calendar_anchor)
    };
    let last = first
        + chrono::Duration::days(if state.calendar_mode == CalendarView::Day {
            0
        } else {
            6
        });
    for week in month_cells(month).chunks(7) {
        let mut days = row![].spacing(0);
        for &date in week {
            let today = date == Local::now().date_naive();
            let active = date >= first && date <= last;
            let selected = date == state.calendar_anchor;
            days = days.push(
                button(
                    container(
                        container(text(date.day().to_string()).size(11))
                            .width(26)
                            .height(26)
                            .center_x(26)
                            .center_y(26)
                            .style(ui_surface(
                                if today {
                                    Tone::Primary
                                } else {
                                    Tone::Transparent
                                },
                                false,
                                10.0,
                            )),
                    )
                    .width(Fill)
                    .height(30)
                    .center_x(Fill)
                    .center_y(30),
                )
                .padding(0)
                .height(30)
                .width(Fill)
                .on_press(AppMessage::CalendarDateSelected(date))
                .style(move |theme, status| {
                    let mut style = ui_button(
                        if active {
                            Tone::CalendarSelection
                        } else {
                            Tone::Transparent
                        },
                        Tone::Hover,
                        if today {
                            Tone::OnPrimary
                        } else if date.month() == month.month() {
                            Tone::Text
                        } else {
                            Tone::Muted
                        },
                        0.0,
                    )(theme, status);
                    if selected && !today {
                        style.border = iced::Border::default()
                            .rounded(3)
                            .width(1)
                            .color(crate::appearance::tone(theme, Tone::CalendarAccent));
                    }
                    style
                }),
            );
        }
        month_grid = month_grid.push(days);
    }
    let header = row![
        text(format!(
            "{} {}",
            tr(MONTHS[month.month0() as usize]),
            month.year()
        ))
        .size(13)
        .font(semibold())
        .width(Fill),
        hint(
            button(icon_tone(Icon::ChevronLeft, Tone::Secondary, 15.0))
                .padding(5)
                .on_press(AppMessage::CalendarMonthNavigate(-1))
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
            tr("Previous month")
        ),
        hint(
            button(icon_tone(Icon::ChevronRight, Tone::Secondary, 15.0))
                .padding(5)
                .on_press(AppMessage::CalendarMonthNavigate(1))
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
            tr("Next month")
        )
    ]
    .spacing(2)
    .align_y(Alignment::Center);
    container(
        column![
            container(
                text(tr("Calendar"))
                    .size(16)
                    .font(semibold())
                    .style(ui_text(Tone::Text))
            )
            .height(52)
            .center_y(52)
            .padding([0, 16]),
            container(column![header, month_grid].spacing(8)).padding([8, 12]),
            button(
                row![
                    icon_tone(Icon::CalendarAdd, Tone::CalendarAccent, 15.0),
                    text(tr("Add calendar")).size(13).style(ui_text(Tone::CalendarAccent)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([6, 16])
            .on_press(AppMessage::OpenSettings)
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::CalendarAccent, 6.0)),
            crate::divider(),
            container(
                column![
                    text(tr("My calendars"))
                        .size(13)
                        .font(semibold())
                        .style(ui_text(Tone::Secondary)),
                    checkbox(state.calendar_visible)
                        .label(tr("Calendar"))
                        .text_size(13)
                        .size(14)
                        .spacing(10)
                        .on_toggle(AppMessage::CalendarVisibilityChanged)
                ]
                .spacing(20)
            )
            .padding([24, 20]),
        ]
        .height(Fill),
    )
    .width(CALENDAR_PANE_WIDTH)
    .height(Fill)
    .style(ui_surface(Tone::CalendarSidebar, false, 0.0))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mini_month_covers_leap_day_and_cross_year_with_sunday_columns() {
        for (year, month) in [(2015, 2), (2024, 2), (2026, 10), (2026, 12), (2027, 1)] {
            let date = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
            let cells = month_cells(date);
            assert!(matches!(cells.len(), 28 | 35 | 42));
            assert_eq!(cells[0].weekday(), chrono::Weekday::Sun);
            for pair in cells.windows(2) {
                assert_eq!(pair[1] - pair[0], chrono::Duration::days(1));
            }
            let next = date.checked_add_months(chrono::Months::new(1)).unwrap();
            let expected = (next - date).num_days() as usize;
            assert_eq!(
                cells
                    .iter()
                    .filter(|d| d.month() == month && d.year() == year)
                    .count(),
                expected
            );
        }
        assert!(
            month_cells(chrono::NaiveDate::from_ymd_opt(2024, 2, 1).unwrap())
                .contains(&chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap())
        );
    }
    #[test]
    fn date_range_heading_is_compact_without_duplicating_month_and_year() {
        let date = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
        assert_eq!(
            period_label(date(2026, 10, 26), date(2026, 10, 30)),
            "October 26–30, 2026"
        );
        assert!(period_label(date(2026, 12, 28), date(2027, 1, 3)).contains("2027"));
        assert_eq!(
            period_label(date(2026, 10, 29), date(2026, 10, 29)),
            calendar::date_label(date(2026, 10, 29))
        );
    }
    #[test]
    fn event_popup_stays_inside_viewport_and_prefers_beside_the_selected_day(){
        for (viewport,sidebar) in [(iced::Size::new(1456.,800.),225.),(iced::Size::new(800.,500.),0.)] {
            for day in 0..5 {
                let card=iced::Size::new(470.0_f32,(viewport.height-112.0_f32).min(580.0_f32));
                let at=details_position(viewport,sidebar,Some(day),5,card);
                assert!(at.x>=16.&&at.y>=16.);assert!(at.x+card.width<=viewport.width-16.);assert!(at.y+card.height<=viewport.height-16.);
            }
        }
        assert!(details_position(iced::Size::new(1456.,800.),225.,Some(1),5,iced::Size::new(470.,580.)).x>500.);
    }

    #[test]
    fn popup_shows_full_date_instead_of_losing_year_and_month_in_today_label(){
        let event=CalendarEvent{start:Some(calendar::CalendarDateTime{value:"2026-11-11T16:00:00".into(),time_zone:None}),end:Some(calendar::CalendarDateTime{value:"2026-11-11T16:30:00".into(),time_zone:None}),..Default::default()};
        assert_eq!(detail_when(&event),"Wed, 11/11/2026  16:00 – 16:30");
    }

}
