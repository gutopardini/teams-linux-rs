//! Mention composition: identity comes only from the current native roster.
//! A blank chat-service name may use a display name already bound to that same id.
use crate::i18n::tr;
use serde::{Serialize,Deserialize};
use unicode_segmentation::UnicodeSegmentation;
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub struct Entry {pub id:String,pub name:String,pub everyone:bool}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub struct Range {pub start:usize,pub end:usize,pub entry:Entry}
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct Trigger {pub start:usize,pub end:usize,pub query:String}
#[derive(Default)]
pub struct State {
    pub chat:Option<String>,pub sequence:u64,pub editor:bool,pub caret:usize,
    pub trigger:Option<Trigger>,pub selected:usize,pub members:Vec<Entry>,
    pub loading:bool,pub loaded:bool,pub error:bool,pub group:bool,
    dismissed:Option<Trigger>,
}
fn fold(value:&str)->String {value.to_lowercase().chars().map(|c|match c {
    'á'|'à'|'â'|'ã'|'ä'=>'a','é'|'è'|'ê'|'ë'=>'e','í'|'ì'|'î'|'ï'=>'i',
    'ó'|'ò'|'ô'|'õ'|'ö'=>'o','ú'|'ù'|'û'|'ü'=>'u','ç'=>'c','ñ'=>'n',_=>c,
}).collect()}
pub fn trigger(text:&str,caret:usize,ranges:&[Range])->Option<Trigger> {
    let chars:Vec<_>=text.graphemes(true).collect();let end=caret.min(chars.len());
    let start=chars[..end].iter().rposition(|c|*c=="@")?;
    if start>0 && !chars[start-1].chars().all(char::is_whitespace) && !["(","[","{"].contains(&chars[start-1]) {return None;}
    let query=chars[start+1..end].concat();
    if query.contains(['\n','\r','@'])||query.chars().count()>64||ranges.iter().any(|r|r.start<end&&r.end>start){return None;}
    Some(Trigger{start,end,query})
}
/// One roster person. The owner, a missing id and a name that is not verified are omitted.
pub fn person(id:&str,wire_name:&str,bound:Option<&str>,owner:Option<&str>)->Option<Entry> {
    let id=crate::avatars::person_id(id)?;
    if owner.and_then(crate::avatars::person_id).as_deref()==Some(id.as_str()){return None;}
    let name=crate::sender_names::known(wire_name).or_else(||bound.and_then(crate::sender_names::known))?;
    Some(Entry{id,name:name.to_owned(),everyone:false})
}
pub fn edit_cursor(old:&str,new:&str)->usize {
    let a:Vec<_>=old.graphemes(true).collect();let b:Vec<_>=new.graphemes(true).collect();
    let prefix=a.iter().zip(&b).take_while(|(a,b)|a==b).count();
    let suffix=a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(a,b)|a==b).count();b.len()-suffix
}
impl State {
    pub fn context(&mut self,chat:Option<String>,group:bool){
        if self.chat!=chat {let sequence=self.sequence.wrapping_add(1);*self=Self{chat,sequence,group,..Self::default()};}
        self.group=group;
    }
    pub fn sync(&mut self,text:&str,ranges:&[Range]) {
        self.editor|=text.contains('@')||!ranges.is_empty();
        let next=trigger(text,self.caret,ranges);
        let next=next.filter(|t|self.dismissed.as_ref()!=Some(t));
        if self.trigger!=next {self.selected=0;self.trigger=next;}
    }
    pub fn close(&mut self){self.dismissed=self.trigger.take();}
    pub fn choices(&self)->Vec<Entry>{
        let Some(trigger)=&self.trigger else{return Vec::new();};let query=fold(&trigger.query);
        let matches=|name:&str|{let name=fold(name);query.split_whitespace().all(|word|name.contains(word))};
        let mut entries:Vec<_>=self.members.iter().filter(|p|matches(&p.name)).cloned().collect();
        if self.group && self.loaded && !self.error && matches(tr("Everyone")) {entries.push(Entry{id:self.chat.clone().unwrap_or_default(),name:tr("Everyone").into(),everyone:true});}
        entries
    }
    pub fn select(&self,entry:&Entry,text:&str)->Option<(String,Range,usize)> {
        let t=self.trigger.as_ref()?;
        if !self.choices().contains(entry){return None;}
        let chars:Vec<_>=text.graphemes(true).collect();
        if t.end>chars.len(){return None;}
        let label=entry.name.clone();let end=t.start+label.graphemes(true).count();
        let suffix=chars[t.end..].concat();let space=if suffix.starts_with(' '){""}else{" "};
        Some((format!("{}{label}{space}{suffix}",chars[..t.start].concat()),Range{start:t.start,end,entry:entry.clone()},end+1))
    }
}
/// Edits intersecting a mention remove its identity; unrelated edits shift it.
pub fn adjust(ranges:&mut Vec<Range>,old:&str,new:&str){
    let a:Vec<_>=old.graphemes(true).collect();let b:Vec<_>=new.graphemes(true).collect();
    let prefix=a.iter().zip(&b).take_while(|(a,b)|a==b).count();
    let suffix=a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(a,b)|a==b).count();
    let old_end=a.len()-suffix;let new_end=b.len()-suffix;
    if old==new{return;}
    ranges.retain_mut(|r|{
        if old_end<=r.start {r.start=r.start+new_end-old_end;r.end=r.end+new_end-old_end;true}
        else if prefix>=r.end {true}else{false}
    });
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn triggers_respect_caret_emails_unicode_and_dismissal(){
        assert_eq!(trigger("Oi @An depois",6,&[]).unwrap().query,"An");
        assert!(trigger("ana@example.com",15,&[]).is_none());
        assert_eq!(trigger("👩🏽‍💻 @Á",4,&[]).unwrap().query,"Á");
        let mut state=State::default();state.caret=1;state.sync("@",&[]);state.close();state.sync("@",&[]);assert!(state.trigger.is_none());
        state.caret=2;state.sync("@a",&[]);assert!(state.trigger.is_some());
    }
    #[test]fn selection_uses_identity_and_only_replaces_active_query(){
        let entry=Entry{id:"person-1".into(),name:"Ána Silva".into(),everyone:false};
        let mut state=State::default();state.members=vec![entry.clone()];state.caret=7;state.sync("Oi @ana depois",&[]);
        let (text,range,caret)=state.select(&entry,"Oi @ana depois").unwrap();assert_eq!(text,"Oi Ána Silva depois");assert_eq!(caret,13);
        let mut ranges=vec![range];adjust(&mut ranges,&text,&format!("👍 {text}"));assert_eq!(ranges[0].start,5);
        let old=format!("👍 {text}");adjust(&mut ranges,&old,&old.replace("Ána","Outra"));assert!(ranges.is_empty());
        assert!(state.select(&Entry{id:"homonym".into(),..entry},"Oi @ana depois").is_none());
    }
    #[test]fn person_uses_the_roster_id_and_a_verified_name(){
        let id="bbbbbbbb-2222-2222-2222-222222222222";
        let owner="aaaaaaaa-1111-1111-1111-111111111111";
        assert_eq!(person(id,"",Some("Ana Beatriz"),Some(owner)).unwrap().name,"Ana Beatriz");
        assert_eq!(person(&format!("8:ORGID:{id}"),"Ana Costa",Some("Outro"),Some(owner)).unwrap().name,"Ana Costa");
        assert!(person(id,"",None,Some(owner)).is_none());
        assert!(person(id,tr("Participant"),None,Some(owner)).unwrap().name==tr("Participant"));
        assert!(person(owner,"Eu",Some("Eu"),Some(&format!("8:orgid:{owner}"))).is_none());
        assert!(person("bot","Bot",None,Some(owner)).is_none());
    }
}

pub fn view(state:&crate::TeamsApp)->Option<iced::Element<'_,crate::AppMessage>> {
    use crate::{AppMessage,appearance::{Tone,ui_text,ui_button,ui_surface}};
    use iced::{Element,Fill,Alignment};
    use iced::widget::{column,row,container,button,text,scrollable};
    let model=&state.mention_composer;
    if state.section!=crate::Section::Chats || state.selected_chat!=model.chat {return None;}
    model.trigger.as_ref()?;
    let mut rows=column![].spacing(0);
    if model.loading {rows=rows.push(container(text(tr("Loading participants…")).size(16).style(ui_text(Tone::Secondary))).padding(24));}
    else if model.error {
        rows=rows.push(container(column![text(tr("Could not load participants.")).size(15),button(text(tr("Try again"))).on_press(AppMessage::RetryMentions)].spacing(12)).padding(20));
    } else {
        let choices=model.choices();
        if choices.is_empty(){rows=rows.push(container(text(tr("No people found in this chat.")).size(15).style(ui_text(Tone::Secondary))).padding(24));}
        for (index,entry) in choices.into_iter().enumerate() {
            let avatar:Element<'_,AppMessage>=if entry.everyone {
                container(crate::icon_tone(crate::Icon::MentionEveryone,Tone::MenuText,24.0)).width(38).height(38).center_x(38).center_y(38).into()
            }else{state.person_avatar(&entry.name,Some(&entry.id),38.0)};
            let subtitle=if entry.everyone {tr("Notify everyone in this chat")}else{tr("Chat participant")};
            rows=rows.push(button(row![avatar,column![text(entry.name.clone()).size(18).style(ui_text(Tone::Text)),text(subtitle).size(15).style(ui_text(Tone::MenuText))].spacing(2).width(Fill)].spacing(10).align_y(Alignment::Center))
                .width(Fill).height(64).padding([10,24]).on_press(AppMessage::MentionPick(entry))
                .style(ui_button(if model.selected==index{Tone::PickerSelected}else{Tone::Transparent},Tone::PickerSelected,Tone::Text,0.0)));
        }
    }
    Some(container(column![container(text(tr("Suggestions")).size(18).style(ui_text(Tone::MenuText))).padding([12,24]).height(52).center_y(52),
        scrollable(rows).id("mention-suggestions").direction(crate::scroll_axis()).style(crate::ui_scroll).height(if model.error {112.0}else{(model.choices().len().clamp(1,6)*64) as f32}),
    ]).width(520).style(|theme|{let mut s=ui_surface(Tone::Surface,true,4.0)(theme);s.shadow=iced::Shadow{color:iced::Color::BLACK.scale_alpha(0.3),offset:iced::Vector::new(0.,5.),blur_radius:18.};s}).into())
}
