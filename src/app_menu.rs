//! Top-bar Settings and more: local menu state, no network work in rendering.
use crate::i18n::tr;
use crate::{AppMessage, Icon, TeamsApp, appearance::{Tone, ui_button, ui_surface, ui_text}, icon_tone};
use iced::widget::{button, column, container, mouse_area, opaque, row, scrollable, space, stack, text};
use iced::{Alignment, Color, Element, Fill, Theme};

const SUPPORT: &str = "https://support.microsoft.com/pt-br/teams/";
const PREMIUM: &str = "https://www.microsoft.com/pt-br/microsoft-teams/premium";
const MOBILE: &str = "https://www.microsoft.com/pt-br/microsoft-teams/download-app";
const FEEDBACK: &str = "https://feedbackportal.microsoft.com/feedback/";
const FEEDBACK_GUIDE: &str = "https://support.microsoft.com/en-gb/teams/apps-service/give-feedback-in-microsoft-teams";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface { Menu, Feedback, Shortcuts }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action { Toggle, Close, Back, Settings, Help, Feedback, Shortcuts, Premium, Mobile, FeedbackPortal, FeedbackGuide, HoverRoot(usize), Hover(usize) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key { Up, Down, Left, Right, Home, End, PageUp, PageDown, Activate, Settings, Help, Shortcuts }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect { None, Settings, OpenUrl(&'static str) }
#[derive(Default, Debug)]
pub struct State { pub surface: Option<Surface>, pub selected: Option<usize> }

const ACTIONS: [Action;6] = [Action::Premium,Action::Settings,Action::Help,Action::Feedback,Action::Shortcuts,Action::Mobile];
impl State {
    pub fn apply(&mut self, action: Action) -> Effect {
        let mut effect=Effect::None;
        match action {
            Action::Toggle => { self.surface=if self.surface.is_some(){None}else{Some(Surface::Menu)};self.selected=None; }
            Action::Close => {self.surface=None;self.selected=None;}
            Action::Back => {self.surface=Some(Surface::Menu);self.selected=Some(3);}
            Action::Feedback => {self.surface=Some(Surface::Feedback);self.selected=None;}
            Action::Shortcuts => {self.surface=Some(Surface::Shortcuts);self.selected=None;}
            Action::HoverRoot(index) if index<6 => {self.surface=Some(Surface::Menu);self.selected=Some(index);}
            Action::Hover(index) => {self.selected=Some(index);}
            Action::Settings => {self.surface=None;self.selected=None;effect=Effect::Settings;}
            Action::Help|Action::Premium|Action::Mobile|Action::FeedbackPortal|Action::FeedbackGuide => {
                self.surface=None;self.selected=None;
                effect=Effect::OpenUrl(match action {Action::Help=>SUPPORT,Action::Premium=>PREMIUM,Action::Mobile=>MOBILE,Action::FeedbackPortal=>FEEDBACK,_=>FEEDBACK_GUIDE});
            }
            _=>{}
        }
        effect
    }
    pub fn key(&mut self, key: Key) -> Option<Action> {
        let surface=self.surface?;
        match key {
            Key::Settings=>return Some(Action::Settings),Key::Help=>return Some(Action::Help),Key::Shortcuts=>return Some(Action::Shortcuts),
            _=>{}
        }
        if surface==Surface::Shortcuts { return None; }
        let count=if surface==Surface::Feedback {2}else{6};
        match key {
            Key::Down=>self.selected=Some(self.selected.map_or(0,|i|(i+1)%count)),
            Key::Up=>self.selected=Some(self.selected.map_or(count-1,|i|(i+count-1)%count)),
            Key::Home=>self.selected=Some(0),Key::End=>self.selected=Some(count-1),
            Key::Left if surface==Surface::Feedback=>return Some(Action::Back),
            Key::Right if surface==Surface::Menu && self.selected==Some(3)=>return Some(Action::Feedback),
            Key::Activate=>return self.selected.and_then(|i|if surface==Surface::Feedback {[Action::FeedbackPortal,Action::FeedbackGuide].get(i).copied()}else{ACTIONS.get(i).copied()}),
            _=>{}
        }
        None
    }
}

pub fn navigation(event: &iced::Event) -> Option<Key> {
    use iced::keyboard::{Event, key::{Code,Physical}};
    let iced::Event::Keyboard(Event::KeyPressed{physical_key:Physical::Code(code),modifiers,repeat,..})=event else{return None;};
    if modifiers.control() && !modifiers.alt() && !*repeat {
        return match code {
            Code::Comma if modifiers.shift()=>Some(Key::Settings),
            Code::Period if !modifiers.shift()=>Some(Key::Shortcuts),
            Code::F1 if !modifiers.shift()=>Some(Key::Help),_=>None,
        };
    }
    if modifiers.control()||modifiers.alt()||modifiers.shift(){return None;}
    match code {Code::ArrowDown=>Some(Key::Down),Code::ArrowUp=>Some(Key::Up),Code::ArrowLeft=>Some(Key::Left),Code::ArrowRight=>Some(Key::Right),Code::Home=>Some(Key::Home),Code::End=>Some(Key::End),Code::PageUp=>Some(Key::PageUp),Code::PageDown=>Some(Key::PageDown),Code::Enter|Code::Space if !*repeat=>Some(Key::Activate),_=>None}
}

fn panel_style(theme: &Theme)->container::Style {
    let mut style=ui_surface(Tone::Surface,false,5.0)(theme);
    style.shadow=iced::Shadow{color:Color::from_rgba(0.0,0.0,0.0,0.32),offset:iced::Vector::new(0.0,4.0),blur_radius:16.0};
    style
}
fn separator()->Element<'static,AppMessage>{container(space::horizontal()).height(1).width(Fill).style(ui_surface(Tone::MenuBorder,false,0.0)).into()}
fn entry(icon: Icon,label: &'static str,shortcut: Option<&'static str>,action: Action,height: f32,selected: bool,arrow: bool)->Element<'static,AppMessage>{
    let mut content=row![icon_tone(icon,Tone::MenuText,20.0),text(label).size(18).width(Fill).wrapping(iced::widget::text::Wrapping::None)].spacing(10).height(Fill).align_y(Alignment::Center);
    if matches!(action,Action::Premium|Action::Settings) {
        content=content.padding(iced::Padding{bottom:4.0,..Default::default()});
    }
    if let Some(shortcut)=shortcut {content=content.push(text(shortcut).size(14).style(ui_text(Tone::Secondary)));}
    if arrow {content=content.push(icon_tone(Icon::ChevronRight,Tone::MenuText,18.0));}
    let background=if selected{Tone::PickerSelected}else{Tone::Transparent};
    button(content).width(Fill).height(height).padding([0,14])
        .on_press(AppMessage::AppMenuAction(action))
        .style(ui_button(background,background,Tone::MenuText,0.0)).into()
}
fn root_menu(state: &State,width: f32)->Element<'static,AppMessage>{
    let labels=[tr("Unlock Teams Premium"),tr("Settings"),tr("Help"),tr("Feedback"),tr("Keyboard shortcuts"),tr("Get the mobile app")];
    let icons=[Icon::Premium,Icon::Settings,Icon::Help,Icon::Feedback,Icon::Keyboard,Icon::Phone];
    let shortcuts=[None,Some("Ctrl+Shift+,"),Some("Ctrl+F1"),None,Some("Ctrl+."),None];
    let mut rows=column![];
    for i in 0..6 {
        let selected=if state.surface==Some(Surface::Feedback){i==3}else{state.selected==Some(i)};
        let control=entry(icons[i],labels[i],shortcuts[i],ACTIONS[i],if i==0{50.0}else if i==1{53.0}else{41.0},selected,i==3);
        rows=rows.push(mouse_area(control).on_enter(AppMessage::AppMenuAction(if i==3{Action::Feedback}else{Action::HoverRoot(i)})));
        if i<2 {rows=rows.push(separator());}
        if i==1 {rows=rows.push(space::vertical().height(4));}
    }
    container(rows).width(width).padding(iced::Padding{bottom:5.0,..Default::default()}).style(panel_style).into()
}
fn feedback_menu(state: &State,width: f32,back: bool)->Element<'static,AppMessage>{
    let mut rows=column![];
    if back {rows=rows.push(button(row![icon_tone(Icon::ChevronLeft,Tone::MenuText,18.0),text(tr("Feedback")).size(16)].spacing(8)).width(Fill).padding(12).on_press(AppMessage::AppMenuAction(Action::Back)).style(ui_button(Tone::Transparent,Tone::Hover,Tone::MenuText,0.0)));}
    for (i,(label,action)) in [(tr("Feedback portal"),Action::FeedbackPortal),(tr("How to send feedback"),Action::FeedbackGuide)].into_iter().enumerate(){
        rows=rows.push(mouse_area(entry(Icon::Feedback,label,None,action,44.0,state.selected==Some(i),false)).on_enter(AppMessage::AppMenuAction(Action::Hover(i))));
    }
    container(rows).width(width).padding([4,0]).style(panel_style).into()
}
fn shortcuts(size: iced::Size)->Element<'static,AppMessage>{
    let mut list=column![].spacing(12);
    for (heading,items) in [
        (tr("General"),vec![(tr("Open Settings"),"Ctrl+Shift+,"),(tr("Open Help"),"Ctrl+F1"),(tr("Keyboard shortcuts"),"Ctrl+."),(tr("Go to a chat or channel"),"Ctrl+Alt+E"),(tr("Close the menu or dialog"),"Esc")]),
        (tr("History and reading · outside text fields"),vec![(tr("Search the conversation"),"Ctrl+F"),(tr("Select the previous or next message"),"Ctrl+Alt+↑ / ↓"),(tr("Open the selected message menu"),"Shift+F10"),(tr("Zoom in or out"),"Ctrl++ / Ctrl+−"),(tr("Reset zoom"),"Ctrl+0")]),
        (tr("Settings"),vec![(tr("Find a setting"),"Ctrl+Alt+F")]),
    ] {
        list=list.push(text(heading).size(15).font(crate::semibold()).style(ui_text(Tone::MenuText)));
        for (label,key) in items {list=list.push(row![text(label).size(15).width(Fill),text(key).size(13).style(ui_text(Tone::Secondary))].spacing(20).align_y(Alignment::Center));}
    }
    let header=row![icon_tone(Icon::Keyboard,Tone::MenuText,24.0),text(tr("Keyboard shortcuts")).size(22).font(crate::semibold()).width(Fill),
        crate::hint(button(icon_tone(Icon::Dismiss,Tone::Secondary,20.0)).padding(7).on_press(AppMessage::AppMenuAction(Action::Close)).style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),tr("Close"))].spacing(12).align_y(Alignment::Center);
    let body=scrollable(container(list).padding(iced::Padding{right:16.0,..Default::default()}).width(Fill))
        .id("app-keyboard-shortcuts").direction(crate::scroll_axis()).style(crate::appearance::ui_scroll)
        .height((size.height-210.0).clamp(100.0,420.0));
    let panel=container(column![header,separator(),body].spacing(18)).padding(24).width((size.width-32.0).min(660.0).max(1.0)).style(panel_style);
    container(opaque(panel)).width(Fill).height(Fill).center_x(Fill).center_y(Fill).into()
}
pub fn view(state: &TeamsApp,size: iced::Size)->Element<'_,AppMessage>{
    let outside=mouse_area(container(space::vertical()).width(Fill).height(Fill)).on_press(AppMessage::AppMenuAction(Action::Close));
    let panel:Element<'_,AppMessage>=if state.app_menu.surface==Some(Surface::Shortcuts){shortcuts(size)}else{
        let right = crate::window_chrome::MENU_TRAILING;
        let available = size.width - right - 12.0;
        let width = available.clamp(1.0, 308.0);
        let content:Element<'_,AppMessage>=if state.app_menu.surface==Some(Surface::Feedback){
            if available >= width + 264.0 {
                row![container(feedback_menu(&state.app_menu,260.0,false)).padding(iced::Padding{top:150.0,..Default::default()}),root_menu(&state.app_menu,width)].spacing(4).into()
            }else{feedback_menu(&state.app_menu,width,true)}
        }else{root_menu(&state.app_menu,width)};
        container(opaque(content)).width(Fill).height(Fill).align_right(Fill).align_top(Fill)
            .padding(iced::Padding{top:crate::appearance::metrics::TOP_BAR,right,..Default::default()}).into()
    };
    stack![outside,panel].width(Fill).height(Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_navigation_reaches_settings_and_feedback_without_external_work(){
        let mut state=State::default();assert_eq!(state.apply(Action::Toggle),Effect::None);
        assert_eq!(state.key(Key::Down),None);assert_eq!(state.selected,Some(0));
        state.key(Key::Down);assert_eq!(state.key(Key::Activate),Some(Action::Settings));
        state.apply(Action::HoverRoot(3));assert_eq!(state.key(Key::Right),Some(Action::Feedback));
        state.apply(Action::Feedback);state.key(Key::Down);assert_eq!(state.key(Key::Activate),Some(Action::FeedbackPortal));
        assert_eq!(state.key(Key::Left),Some(Action::Back));
        state.apply(Action::Shortcuts);assert_eq!(state.key(Key::Activate),None);
        state.apply(Action::Close);assert_eq!(state.surface,None);
    }
    #[test]
    fn external_destinations_are_explicit_and_closing_never_changes_settings(){
        let mut state=State::default();
        for action in [Action::Premium,Action::Mobile,Action::Help,Action::FeedbackPortal,Action::FeedbackGuide]{
            state.apply(Action::Toggle);let Effect::OpenUrl(url)=state.apply(action)else{panic!("external destination missing")};
            assert!(url.starts_with("https://"));assert_eq!(state.surface,None);
        }
        assert_eq!(state.apply(Action::Settings),Effect::Settings);
    }
}
