//! Expanded message reaction picker. It never owns or edits composer content.
use crate::i18n::tr;
use crate::*;
use iced::widget::column;

pub fn view(state: &TeamsApp, message: &GraphMessage) -> Element<'static, AppMessage> {
    let recent = state.reaction_category == emoji::Category::Recent && state.reaction_query.trim().is_empty();
    let choices: Vec<_> = if recent {
        state.preferences.recent_reactions.iter().filter_map(|token| {
            reaction_catalog::resolve_key(reactions::key(token)?).map(|(entry,tone)| (entry,tone))
        }).collect()
    } else {
        reaction_catalog::visible(state.reaction_category, &state.reaction_query, &[]).into_iter()
            .map(|entry| (entry, if entry.diverse { reaction_catalog::tone_index(state.preferences.skin_tone) } else { 0 })).collect()
    };
    let pending = state.selected_chat.as_deref().is_some_and(|chat| reaction_is_pending(&state.pending_reactions, state.session_epoch, chat, &message.id));
    let enabled = !pending && state.pending_reactions.len() < MAX_PENDING_REACTIONS;
    let mut grid = column![].spacing(3);
    for choices in choices.chunks(6) {
        let mut cells = row![].spacing(3);
        for (entry,tone) in choices {
            let token = reaction_catalog::token(entry, *tone);
            let selected = message.reactions.iter().any(|reaction| reaction.emoji == token && user_reacted(reaction, &state.user_id));
            let action = if state.reaction_customizing { AppMessage::QuickReactionChosen(token.clone()) }
                else { AppMessage::ToggleReaction(message.id.clone(), token.clone()) };
            let glyph = reaction_catalog::glyph(&token, 32.0).expect("catalog reaction glyph");
            let glyph = container(glyph).width(Fill).height(Fill).center_x(Fill).center_y(Fill);
            let cell: Element<'static,AppMessage> = if entry.diverse {
                stack![glyph,container(container(space::horizontal()).width(4).height(4).style(ui_surface(Tone::Secondary,false,2.0)))
                    .align_right(Fill).align_top(Fill)].into()
            } else { glyph.into() };
            let cell = hint(button(cell).width(52).height(52).padding(6)
                .on_press_maybe((enabled || state.reaction_customizing).then_some(action))
                .style(ui_button(if selected { Tone::Selected } else { Tone::Transparent }, Tone::Hover, Tone::Text, 4.0)),
                format!("{}{}", if state.reaction_customizing { tr("Set as a quick reaction: ") } else if selected { tr("Remove reaction: ") } else { tr("React: ") }, entry.label()));
            let cell = mouse_area(cell).on_right_press(AppMessage::ReactionToneOpened(reactions::key(&token).expect("catalog reaction key").into()));
            let popup = (state.reaction_tone_for.as_deref() == reactions::key(&token)).then(|| {
                let mut tones=row![].spacing(2);
                for tone in 0..6 {
                    let token=reaction_catalog::token(entry,tone);
                    let action=if state.reaction_customizing {AppMessage::QuickReactionChosen(token.clone())} else {AppMessage::ToggleReaction(message.id.clone(),token.clone())};
                    tones=tones.push(hint(button(reaction_catalog::glyph(&token,30.0).expect("skin tone glyph"))
                        .width(36).height(40).padding(3).on_press_maybe((enabled || state.reaction_customizing).then_some(action))
                        .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)),preferences::SkinTone::ALL[tone].label()));
                }
                container(tones).padding(6).style(message_popup_style).into()
            });
            cells=cells.push(composer_popover::view_with_close(cell,popup,false,AppMessage::CloseReactionTone));
        }
        grid = grid.push(cells);
    }
    if choices.is_empty() {
        grid = grid.push(text(if recent { tr("Your recent reactions will appear here.") } else { tr("No emoji found.") }).size(14).style(ui_text(Tone::Secondary)));
        if recent { grid = grid.push(button(text("Explorar emojis").size(14)).on_press(AppMessage::ReactionCategorySelected(emoji::Category::Faces)).style(ui_button(Tone::Transparent,Tone::Hover,Tone::Accent,4.0))); }
    }
    let search = container(column![
        row![text_input(tr("Find something fun"), &state.reaction_query).id("reaction-picker-search")
            .on_input(AppMessage::ReactionSearchChanged).padding([8,12]).size(16)
            .style(|theme,status| text_input::Style { border: Border::default(), background:tone(theme,Tone::Canvas).into(), ..ui_input(false)(theme,status) }),
            container(icon_tone(Icon::Search, Tone::Secondary,18.0)).padding([8,8]),
        ].align_y(Alignment::Center),
        container(space::horizontal()).height(2).width(Fill).style(ui_surface(Tone::Accent,false,0.0)),
    ]).width(Fill);
    let search = row![search, hint(button(icon_tone(Icon::EmojiEdit,Tone::Secondary,22.0)).width(30).height(40).padding(2)
        .on_press(AppMessage::CustomizeReactions).style(ui_button(Tone::Transparent,Tone::Hover,Tone::Text,4.0)), tr("Customize quick reactions"))].spacing(20).align_y(Alignment::Center);
    let heading: Element<'static,AppMessage> = if state.reaction_customizing {
        let mut slots = row![text("Substituir:").size(12).style(ui_text(Tone::Secondary))].spacing(4).align_y(Alignment::Center);
        for (index,token) in state.preferences.quick_reactions.iter().enumerate() {
            slots = slots.push(hint(button(reaction_glyph(token,20.0)).padding(3).width(28).height(28)
                .on_press(AppMessage::QuickReactionSlot(index))
                .style(ui_button(if state.reaction_quick_slot==index {Tone::Selected} else {Tone::Transparent},Tone::Hover,Tone::Text,4.0)), tr("Choose a quick reaction slot")));
        }
        slots.into()
    } else {
        text(if !state.reaction_query.trim().is_empty() { tr("Results") } else { state.reaction_category.label() }).size(16).style(ui_text(Tone::Text)).into()
    };
    let mut categories = row![].spacing(0).align_y(Alignment::Center);
    for category in emoji::Category::ALL {
        let active = state.reaction_category == category;
        categories = categories.push(hint(button(reaction_catalog::category_glyph(category,active))
            .width(36).height(52).padding(8).on_press(AppMessage::ReactionCategorySelected(category))
            .style(ui_button(if active {Tone::PickerSelected} else {Tone::Transparent},Tone::Hover,Tone::Text,4.0)),category.label()));
    }
    container(column![
        container(search).padding(iced::Padding {top:18.0,right:4.0,bottom:0.0,left:18.0}),
        container(column![space::vertical().height(20),container(heading).height(if state.reaction_customizing {28} else {20}),space::vertical().height(20),
            scrollable(container(grid).padding(iced::Padding {left:2.0,top:6.0,..Default::default()}))
                .id("reaction-picker-grid").direction(scroll_axis()).style(ui_scroll).height(if state.reaction_customizing {258} else {266}),
        ]).padding([0,18]),
        container(divider()).padding([0,10]),
        container(categories).padding([4,10]).height(60).center_y(60),
    ]).padding(iced::Padding {bottom:3.0,..Default::default()}).width(384).style(message_popup_style).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selecting_extended_reaction_changes_only_the_origin_message_and_records_actual_use() {
        let mut state = TeamsApp::from_client_id(String::new());
        let id=state.messages.last().unwrap().id.clone();
        drop(update(&mut state,AppMessage::DraftChanged("Rascunho 👩‍💻 preservado".into())));
        let draft=state.draft.clone();
        drop(update(&mut state,AppMessage::OpenReactionPicker(id.clone())));
        drop(update(&mut state,AppMessage::ReactionSearchChanged("punho".into())));
        assert_eq!(state.reaction_query,"punho");
        drop(update(&mut state,AppMessage::ToggleReaction(id.clone(),"1f44a_oncomingfist-tone1".into())));
        assert_eq!(state.draft,draft);
        assert_eq!(state.preferences.recent_reactions,["1f44a_oncomingfist-tone1"]);
        assert!(state.messages.iter().find(|message|message.id==id).unwrap().reactions.iter().any(|reaction|reaction.emoji=="1f44a_oncomingfist-tone1"));
        assert!(state.reaction_picker_for.is_none());
        assert!(!state.preferences_saving);
        drop(update(&mut state,AppMessage::ReactionSearchChanged("evento atrasado".into())));
        assert_eq!(state.reaction_query,"punho");
    }
    #[test]
    fn quick_reaction_customization_never_sends_a_message_reaction() {
        let mut state=TeamsApp::from_client_id(String::new());
        let id=state.messages.last().unwrap().id.clone();
        drop(update(&mut state,AppMessage::OpenReactionPicker(id.clone())));
        let before=state.messages.clone();
        drop(update(&mut state,AppMessage::CustomizeReactions));
        drop(update(&mut state,AppMessage::QuickReactionSlot(2)));
        drop(update(&mut state,AppMessage::QuickReactionChosen("clappinghands".into())));
        assert!(state.messages == before);
        assert_eq!(state.preferences.quick_reactions[2],"clappinghands");
        assert!(!state.reaction_customizing);
        assert!(state.pending_reactions.is_empty());
    }
}
