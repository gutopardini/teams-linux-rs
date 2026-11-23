//! Typed mention identities. Display names and ordinal item IDs are never MRIs.
use scraper::ElementRef;
use serde_json::Value;
use std::collections::HashMap;
#[derive(Clone,Debug,Default,PartialEq,Eq)]
pub enum Target { Person(String), Everyone, #[default] Unknown }
#[derive(Clone,Debug,Default,PartialEq,Eq)]
pub struct Mention { pub item_id:Option<String>,pub target:Target,pub own:bool }
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub enum Badge {#[default] None,Everyone,Person}
impl Mention {pub fn attention(&self)->bool{self.own||self.target==Target::Everyone}}
fn value<'a>(object:&'a Value,key:&str)->Option<&'a str>{object.as_object()?.iter().find(|(name,_)|name.eq_ignore_ascii_case(key))?.1.as_str()}
fn item_key(object:&Value)->Option<String>{
    let raw=object.as_object()?.iter().find(|(name,_)|name.eq_ignore_ascii_case("itemid"))?.1;
    let id=match raw{
        Value::String(id)=>id.trim().to_owned(),
        Value::Number(id)=>id.as_i64().filter(|id|*id>=0).map(|id|id.to_string())?,
        _=>return None,
    };
    (!id.is_empty()&&id.len()<=256).then_some(id)
}
fn mention_identity(object:&Value)->Option<&str>{value(object,"mri").or_else(||value(object,"userId")).or_else(||value(object,"userMri"))}
fn target(kind:Option<&str>,identity:Option<&str>)->Target {
    match kind {
        Some("everyone")=>Target::Everyone,
        None|Some("person")=>identity.and_then(crate::avatars::person_id).map(Target::Person).unwrap_or_default(),
        _=>Target::Unknown,
    }
}
pub fn from_element(element:ElementRef<'_>)->Mention {
    let node=element.value();
    let item_id=["itemid","id","data-mention-id"].iter().find_map(|name|node.attr(name)).filter(|id|id.len()<=256).map(str::to_owned);
    let props=node.attr("data-itemprops").filter(|s|s.len()<=65536).and_then(|s|serde_json::from_str::<Value>(s).ok());
    let kind=node.attr("type").or_else(||props.as_ref().and_then(|v|value(v,"mentionType")));
    let identity=node.attr("mri").or_else(||props.as_ref().and_then(mention_identity)).or(item_id.as_deref());
    let resolved=target(kind,identity);
    Mention{item_id,target:resolved,own:false}
}
pub fn metadata(input:Option<&Value>)->HashMap<String,Target>{
    let parsed=input.and_then(Value::as_str).filter(|s|s.len()<=262144).and_then(|s|serde_json::from_str::<Value>(s).ok());
    let Some(entries)=parsed.as_ref().or(input).and_then(Value::as_array)else{return HashMap::new();};
    let mut result=HashMap::new();
    for entry in entries.iter().take(512) {
        let Some(id)=item_key(entry)else{continue;};
        let resolved=target(value(entry,"mentionType"),mention_identity(entry));
        result.entry(id.to_owned()).and_modify(|old|{if *old!=resolved{*old=Target::Unknown;}}).or_insert(resolved);
    }
    result
}
pub fn resolve(mention:&mut Mention,metadata:&HashMap<String,Target>,owner:&str,quoted:bool){
    if !quoted && let Some(resolved)=mention.item_id.as_ref().and_then(|id|metadata.get(id)){
        mention.target=if mention.target==Target::Unknown||mention.target==*resolved{resolved.clone()}else{Target::Unknown};
    }
    mention.own=!quoted&&match(&mention.target,crate::avatars::person_id(owner)){(Target::Person(id),Some(owner))=>*id==owner,_=>false};
}
#[cfg(test)]mod tests{
    use super::*;
    const OWNER:&str="aaaaaaaa-1111-2222-3333-444444444444";
    #[test]fn ordinal_identity_uses_metadata_and_conflicts_fail_closed(){
        let entries=metadata(Some(&serde_json::json!([{"itemid":"0","mri":format!("8:orgid:{OWNER}"),"mentionType":"person"}])));
        let mut mention=Mention{item_id:Some("0".into()),..Default::default()};resolve(&mut mention,&entries,OWNER,false);assert!(mention.own);
        let conflict=metadata(Some(&serde_json::json!([{"itemid":"0","mri":OWNER},{"itemid":"0","mri":"bbbbbbbb-1111-2222-3333-444444444444"}])));
        let mut mention=Mention{item_id:Some("0".into()),..Default::default()};resolve(&mut mention,&conflict,OWNER,false);assert_eq!(mention.target,Target::Unknown);
        let mut quoted=Mention{item_id:Some("0".into()),..Default::default()};resolve(&mut quoted,&entries,OWNER,true);assert!(!quoted.own);
        let wire=serde_json::Value::String(serde_json::json!([{"itemid":0,"userId":format!("8:orgid:{OWNER}"),"mentionType":"person"}]).to_string());
        let mut doc=crate::message_body::Document::from_html("<p><span itemtype='http://schema.skype.com/Mention' itemid='0'>Ana</span></p>").unwrap();
        doc.resolve_mentions(Some(&wire),OWNER);
        assert_eq!(doc.mention_badge(),Badge::Person);
        assert!(doc.blocks[0].runs.iter().any(|run|run.style.mention_info.as_ref().is_some_and(|mention|mention.own&&mention.attention())));
    }
    #[test]fn own_and_other_people_with_equal_names_keep_distinct_roles_and_cards(){
        let peer="bbbbbbbb-1111-2222-3333-444444444444";
        let html="<p><span itemtype='http://schema.skype.com/Mention' itemid='0'>Ana <b>Silva</b></span> e <span itemtype='http://schema.skype.com/Mention' itemid='1'>Ana Silva</span>. Ana Silva é texto comum.</p>";
        let metadata=serde_json::json!([{"itemid":"0","mri":OWNER,"mentionType":"person"},{"itemid":"1","mri":peer,"mentionType":"person"}]);
        let mut doc=crate::message_body::Document::from_html(html).unwrap();doc.resolve_mentions(Some(&metadata),OWNER);
        assert_eq!(doc.mention_badge(),Badge::Person);assert!(doc.mentioned_person(peer).is_some());
        let runs=&doc.blocks[0].runs;
        assert!(runs.iter().filter(|r|r.style.mention_info.as_ref().is_some_and(|m|m.own)).all(|r|r.style.mention_info.as_ref().unwrap().attention()));
        assert!(runs.iter().filter(|r|r.style.mention_info.as_ref().is_some_and(|m|matches!(&m.target,Target::Person(id)if id==peer))).all(|r|!r.style.mention_info.as_ref().unwrap().attention()));
    }
    #[test]fn everyone_is_typed_and_quotes_plain_names_and_numeric_ids_do_not_notify(){
        for html in ["<p>Ana Silva</p>","<p><span itemtype='http://schema.skype.com/Mention' itemid='0'>Ana</span></p>","<blockquote><span itemtype='http://schema.skype.com/Mention' itemid='0'>Ana</span></blockquote>"]{
            let mut doc=crate::message_body::Document::from_html(html).unwrap();doc.resolve_mentions(None,OWNER);assert_eq!(doc.mention_badge(),Badge::None);
        }
        let mut doc=crate::message_body::Document::from_html("<p><span itemtype='http://schema.skype.com/Mention' type='everyone'>Todos</span></p>").unwrap();doc.resolve_mentions(None,OWNER);assert_eq!(doc.mention_badge(),Badge::Everyone);
        let mut doc=crate::message_body::Document::from_html(&format!("<p><span itemtype='http://schema.skype.com/Mention' data-itemprops='{{&quot;mri&quot;:&quot;8:orgid:{OWNER}&quot;}}'>Ana</span></p>")).unwrap();doc.resolve_mentions(None,OWNER);assert_eq!(doc.mention_badge(),Badge::Person);
    }
}

pub fn outgoing_metadata(document:&crate::message_body::Document,thread:&str)->Option<Value> {
    let mut entries=std::collections::BTreeMap::<String,(Target,String)>::new();
    for block in &document.blocks {if block.quote>0{continue;}for run in &block.runs {
        let Some(mention)=&run.style.mention_info else{continue;};let Some(id)=&mention.item_id else{continue;};if mention.target==Target::Unknown{continue;}
        let entry=entries.entry(id.clone()).or_insert_with(||(mention.target.clone(),String::new()));
        if entry.0!=mention.target{return None;}entry.1.push_str(&run.text);
    }}
    let result:Vec<_>=entries.into_iter().filter_map(|(id,(target,name))|{
        let (kind,mri)=match target {Target::Person(person)=>("person",format!("8:orgid:{person}")),Target::Everyone if thread.starts_with("19:")=>("everyone",thread.to_owned()),_=>return None};
        Some(serde_json::json!({"itemid":id,"mri":mri,"mentionType":kind,"displayName":name}))
    }).collect();(!result.is_empty()).then(||Value::Array(result))
}
pub fn with_metadata(payload:&mut Value,metadata:Option<Value>){if let Some(metadata)=metadata{payload["properties"]["mentions"]=Value::String(metadata.to_string());}}
pub fn fingerprint(document:&crate::message_body::Document,thread:&str)->String {
    let html=document.to_html();outgoing_metadata(document,thread).map_or_else(||html.clone(),|metadata|serde_json::json!([html,metadata]).to_string())
}
#[cfg(test)]mod outgoing_tests {
    use super::*;
    fn document(id:&str)->crate::message_body::Document {
        let formatting=crate::message_body::ComposerFormatting{mentions:vec![crate::mention_composer::Range{start:3,end:6,entry:crate::mention_composer::Entry{id:id.into(),name:"Ana".into(),everyone:false}}],..Default::default()};
        crate::message_body::Document::from_editor("Oi Ana & olá",&formatting)
    }
    #[test]fn outgoing_identity_is_separate_from_ordinal_html_and_retry_fingerprint(){
        let doc=document("11111111-1111-1111-1111-111111111111");let html=doc.to_html();
        assert!(html.contains("itemid=\"0\""));assert!(!html.contains("11111111"));
        let mut body=serde_json::json!({"content":html,"properties":{"replyToId":"fixture"}});
        with_metadata(&mut body,outgoing_metadata(&doc,"19:fixture@thread.v2"));
        let metadata:Value=serde_json::from_str(body["properties"]["mentions"].as_str().unwrap()).unwrap();
        assert_eq!(metadata[0]["mentionType"],"person");assert_eq!(metadata[0]["displayName"],"Ana");assert_eq!(body["properties"]["replyToId"],"fixture");
        let homonym=document("22222222-2222-2222-2222-222222222222");assert_eq!(doc.to_html(),homonym.to_html());assert_ne!(fingerprint(&doc,"chat"),fingerprint(&homonym,"chat"));
    }
}
