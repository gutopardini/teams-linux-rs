//! Native org.freedesktop.Notifications transport. One bounded worker per session;
//! no shell/browser, and no notification content is logged.
use iced::futures::{StreamExt, future::join_all};
use std::{collections::{HashMap, VecDeque}, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, time::Duration};
use tokio::sync::mpsc as commands;

#[derive(Clone, Copy)]
pub enum Kind { Message, IncomingCall, MeetingStarted { can_chat: bool } }
pub struct MessageNotice {
    pub sound: bool,
    pub kind: Kind,
    pub chat_id: String,
    pub title: String,
    pub body: String,
}
enum Command { Show(u64, MessageNotice), Close(String) }
struct Worker { commands: commands::Sender<Command>, stop: Arc<AtomicBool> }
#[derive(Default)]
pub struct DesktopMessages {
    worker: Option<Worker>,
    events: Option<mpsc::Receiver<(u64, String)>>,
    // Unit tests must opt into an explicit private bus; never the user's DE.
    #[cfg(test)]
    address: Option<String>,
}
impl DesktopMessages {
    pub fn show(&mut self, epoch: u64, notice: MessageNotice) -> bool {
        #[cfg(test)]
        let address = match self.address.clone() { Some(address) => Some(address), None => return false };
        #[cfg(not(test))]
        let address: Option<String> = None;
        if self.worker.as_ref().is_some_and(|worker| worker.commands.is_closed()) { self.reset(); }
        if self.worker.is_none() {
            let (tx, rx) = commands::channel(128);
            let (events, receiver) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let cancel = stop.clone();
            if std::thread::Builder::new().name("teams-desktop-notices".into()).spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    if runtime.block_on(run(rx, events, cancel, address.as_deref())).is_err() {
                        eprintln!("Serviço de notificações do desktop indisponível");
                    }
                }
            }).is_err() { return false; }
            self.worker = Some(Worker { commands: tx, stop });
            self.events = Some(receiver);
        }
        let worker = self.worker.as_ref().unwrap();
        let queued=worker.commands.try_send(Command::Show(epoch, notice)).is_ok();
        crate::live_trace::record(if queued {"desktop_notice_queued"} else {"desktop_notice_queue_full"},0,1,0);
        queued
    }
    pub fn close_chat(&self, chat: &str) {
        if let Some(worker) = &self.worker { let _ = worker.commands.try_send(Command::Close(chat.to_owned())); }
    }
    pub fn has_worker(&self) -> bool { self.worker.as_ref().is_some_and(|w| !w.commands.is_closed()) }
    pub fn drain(&self) -> Vec<(u64, String)> { self.events.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default() }
    pub fn reset(&mut self) {
        if let Some(worker) = self.worker.take() { worker.stop.store(true, Ordering::Release); }
        self.events = None;
    }
}
impl Drop for DesktopMessages { fn drop(&mut self) { self.reset(); } }

// Summary is plain text; notification bodies support markup on many DEs.
fn body_text(text: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let truncated: String = text.graphemes(true).take(240).collect();
    truncated.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
pub(crate) async fn close(proxy: &zbus::Proxy<'_>, id: u32) {
    let _ = tokio::time::timeout(Duration::from_secs(1), proxy.call::<_, _, ()>("CloseNotification", &(id,))).await;
}
pub(crate) async fn notify(proxy: &zbus::Proxy<'_>, replaces: u32, notice: &MessageNotice) -> Result<u32, ()> {
    use zbus::zvariant::Value;
    let mut hints = HashMap::<&str, Value<'_>>::new();
    hints.insert("desktop-entry", Value::from(crate::APP_ID));
    let (category, sound, icon, actions, timeout) = match notice.kind {
        Kind::Message => ("im.received", "message-new-instant", "mail-message-new", vec!["default", crate::i18n::tr("Open chat")], -1i32),
        Kind::IncomingCall => ("call.incoming", "phone-incoming-call", "call-start", vec!["default", crate::i18n::tr("View call")], 30_000i32),
        Kind::MeetingStarted { can_chat } => ("call.started", "message-new-instant", "appointment-soon",
            if can_chat {vec!["join", crate::i18n::tr("Join"), "chat", crate::i18n::tr("Message")]} else {vec!["join", crate::i18n::tr("Join")]},60_000i32),
    };
    hints.insert("category", Value::from(category));
    if notice.sound { hints.insert("sound-name", Value::from(sound)); }
    else { hints.insert("suppress-sound", Value::from(true)); }
    hints.insert("urgency", Value::from(1u8));
    // Timeout -1 and normal priority leave presentation, sound and DND to the DE.
    let args = ("Teams Linux", replaces, icon, notice.title.as_str(),
        body_text(&notice.body), actions, hints, timeout);
    tokio::time::timeout(Duration::from_secs(5), proxy.call("Notify", &args)).await.map_err(|_| ())?.map_err(|_| ())
}
/// Meeting buttons use the server's returned ID, never an untrusted signal ID.
pub(crate) async fn meeting_notice(proxy:&zbus::Proxy<'_>, notice:&MessageNotice, stop:&AtomicBool) -> Result<Option<String>,()> {
    let Kind::MeetingStarted{can_chat}=notice.kind else{return Err(());};
    let mut actions=proxy.receive_signal("ActionInvoked").await.map_err(|_|())?;
    let mut closed=proxy.receive_signal("NotificationClosed").await.map_err(|_|())?;
    if stop.load(Ordering::Acquire){return Ok(None);}
    let id=notify(proxy,0,notice).await?;
    if id==0{return Err(());}
    crate::live_trace::record("desktop_meeting_delivered",0,1,0);
    let start=std::time::Instant::now();
    let action=loop {
        if stop.load(Ordering::Acquire)||start.elapsed()>=Duration::from_secs(60){break None;}
        tokio::select! {
            message=actions.next()=>match message {
                Some(message)=>if let Ok((received,key))=message.body().deserialize::<(u32,String)>() {
                    if received==id && (key=="join" || (key=="chat" && can_chat)){break Some(key);}
                },
                None=>break None,
            },
            message=closed.next()=>match message {
                Some(message)=>if message.body().deserialize::<(u32,u32)>().is_ok_and(|(received,_)|received==id){break None;},
                None=>break None,
            },
            _=tokio::time::sleep(Duration::from_millis(100))=>{},
        }
    };
    close(proxy,id).await;
    Ok(action)
}

async fn run(
    mut commands: commands::Receiver<Command>, events: mpsc::Sender<(u64, String)>,
    stop: Arc<AtomicBool>, address: Option<&str>,
) -> Result<(), ()> {
    let connect = async {
        match address {
            Some(address) => zbus::connection::Builder::address(address).map_err(|_| ())?.build().await.map_err(|_| ()),
            None => zbus::Connection::session().await.map_err(|_| ()),
        }
    };
    let connection = tokio::time::timeout(Duration::from_secs(5), connect).await.map_err(|_| ())??;
    let proxy = zbus::Proxy::new(&connection, "org.freedesktop.Notifications", "/org/freedesktop/Notifications", "org.freedesktop.Notifications").await.map_err(|_| ())?;
    let mut actions = proxy.receive_signal("ActionInvoked").await.map_err(|_| ())?;
    let mut closed = proxy.receive_signal("NotificationClosed").await.map_err(|_| ())?;
    let mut active = HashMap::<String, (u32, u64)>::new();
    let mut order = VecDeque::<String>::new();
    let mut result = Ok(());
    while !stop.load(Ordering::Acquire) {
        tokio::select! {
            biased;
            action = actions.next() => match action {
                Some(message) => if let Ok((id, action)) = message.body().deserialize::<(u32, String)>() {
                    if action == "default" {
                        if let Some((chat, (_, epoch))) = active.iter().find(|(_, n)| n.0 == id) {
                            let _ = events.send((*epoch, chat.clone()));
                        }
                    }
                },
                None => { result = Err(()); break; }
            },
            command = commands.recv() => match command {
                Some(Command::Show(epoch, notice)) => {
                    if stop.load(Ordering::Acquire) { break; }
                    let replaces = active.get(&notice.chat_id).map_or(0, |n| n.0);
                    if replaces == 0 && active.len() >= 32 {
                        if let Some(chat) = order.pop_front() {
                            if let Some((id, _)) = active.remove(&chat) { close(&proxy, id).await; }
                        }
                    }
                    match notify(&proxy, replaces, &notice).await {
                        Ok(id) if id != 0 => {
                            order.retain(|chat| chat != &notice.chat_id);
                            order.push_back(notice.chat_id.clone());
                            active.insert(notice.chat_id, (id, epoch));
                            crate::live_trace::record("desktop_notice_delivered",0,1,0);
                        }
                        _ => {
                            // A rejected notice must not destroy other queued alerts.
                            crate::live_trace::record("desktop_notice_failed",0,1,0);
                            eprintln!("Não foi possível entregar um aviso ao desktop");
                        }
                    }
                }
                Some(Command::Close(chat)) => {
                    order.retain(|item| item != &chat);
                    if let Some((id, _)) = active.remove(&chat) { close(&proxy, id).await; }
                }
                None => break,
            },
            message = closed.next() => match message {
                Some(message) => if let Ok((id, _)) = message.body().deserialize::<(u32, u32)>() {
                    active.retain(|_, n| n.0 != id);
                    order.retain(|chat| active.contains_key(chat));
                },
                None => { result = Err(()); break; }
            },
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
    // Close only IDs returned to this worker, including after account teardown.
    join_all(active.values().map(|(id, _)| close(&proxy, *id))).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{BufRead, BufReader}, process::{Child, Command as Process, Stdio}, sync::Mutex};
    struct Bus(Child, String);
    impl Bus {
        fn new() -> Self {
            let mut child = Process::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("private D-Bus daemon");
            let mut address = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut address).unwrap();
            Self(child, address.trim().into())
        }
    }
    impl Drop for Bus { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    #[derive(Clone, Debug)]
    struct Received { replaces: u32, title: String, body: String, actions: Vec<String>, timeout: i32, category: String, sound: Option<String>, suppressed: bool }
    struct Service { fail_first: AtomicBool, received: Arc<Mutex<Vec<Received>>>, closed: Arc<Mutex<Vec<u32>>>, next: std::sync::atomic::AtomicU32 }
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Service {
        #[allow(clippy::too_many_arguments)]
        fn notify(&self, app_name: &str, replaces_id: u32, app_icon: &str, summary: &str, body: &str,
            actions: Vec<String>, hints: HashMap<String, zbus::zvariant::OwnedValue>, expire_timeout: i32) -> zbus::fdo::Result<u32> {
            if self.fail_first.swap(false, Ordering::AcqRel) { return Err(zbus::fdo::Error::Failed("fixture transient failure".into())); }
            assert_eq!(app_name, "Teams Linux"); assert!(["mail-message-new","call-start","appointment-soon"].contains(&app_icon));
            assert_eq!(<&str>::try_from(hints.get("desktop-entry").unwrap()).unwrap(), crate::APP_ID);
            let category = <&str>::try_from(hints.get("category").unwrap()).unwrap().to_owned();
            self.received.lock().unwrap().push(Received { replaces: replaces_id, title: summary.into(), body: body.into(), actions, timeout: expire_timeout, category,
                sound:hints.get("sound-name").and_then(|v|<&str>::try_from(v).ok()).map(str::to_owned),
                suppressed:hints.get("suppress-sound").and_then(|v|bool::try_from(v).ok()).unwrap_or(false) });
            Ok(if replaces_id == 0 { self.next.fetch_add(1, Ordering::Relaxed) } else { replaces_id })
        }
        fn close_notification(&self, id: u32) { self.closed.lock().unwrap().push(id); }
        #[zbus(signal)]
        async fn action_invoked(emitter: &zbus::object_server::SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;
        #[zbus(signal)]
        async fn notification_closed(emitter: &zbus::object_server::SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;
    }
    async fn wait_for(check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !check() { tokio::time::sleep(Duration::from_millis(5)).await; }
        }).await.expect("private notification server did not observe event");
    }
    #[test]
    fn native_bus_delivers_replaces_routes_click_and_cleans_up_without_desktop_access() {
        let bus = Bus::new();
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let received = Arc::new(Mutex::new(Vec::new())); let closed = Arc::new(Mutex::new(Vec::new()));
            let server = zbus::connection::Builder::address(bus.1.as_str()).unwrap().name("org.freedesktop.Notifications").unwrap()
                .serve_at("/org/freedesktop/Notifications", Service { fail_first: AtomicBool::new(false), received: received.clone(), closed: closed.clone(), next: std::sync::atomic::AtomicU32::new(42) }).unwrap().build().await.unwrap();
            let (tx, rx) = commands::channel(32); let (events, actions) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false)); let cancel = stop.clone(); let address = bus.1.clone();
            let worker = tokio::spawn(async move { run(rx, events, cancel, Some(&address)).await });
            tx.send(Command::Show(7, MessageNotice { sound:true, kind:Kind::Message, chat_id: "fictional-chat".into(), title: "Conversa fictícia".into(), body: "Oi <b> & teste".into() })).await.unwrap();
            wait_for(|| received.lock().unwrap().len() == 1).await;
            {
                let values = received.lock().unwrap(); let n = &values[0];
                assert_eq!(n.replaces, 0); assert_eq!(n.title, "Conversa fictícia"); assert_eq!(n.body, "Oi &lt;b&gt; &amp; teste");
                assert_eq!(n.actions, ["default", "Open chat"]); assert_eq!(n.timeout, -1); assert_eq!(n.category, "im.received");
            }
            tx.send(Command::Show(7, MessageNotice { sound:true, kind:Kind::Message, chat_id: "fictional-chat".into(), title: "Conversa fictícia".into(), body: "Outra mensagem".into() })).await.unwrap();
            wait_for(|| received.lock().unwrap().len() == 2).await;
            assert_eq!(received.lock().unwrap()[1].replaces, 42);
            let emitter = zbus::object_server::SignalEmitter::new(&server, "/org/freedesktop/Notifications").unwrap();
            Service::action_invoked(&emitter, 999, "default").await.unwrap();
            Service::action_invoked(&emitter, 42, "unrecognized").await.unwrap();
            Service::action_invoked(&emitter, 42, "default").await.unwrap();
            let action = tokio::time::timeout(Duration::from_secs(3), async {
                loop { if let Ok(value) = actions.try_recv() { break value; } tokio::time::sleep(Duration::from_millis(5)).await; }
            }).await.unwrap();
            assert_eq!(action, (7, "fictional-chat".into())); assert!(actions.try_recv().is_err());
            tx.send(Command::Close("fictional-chat".into())).await.unwrap();
            wait_for(|| !closed.lock().unwrap().is_empty()).await;
            assert_eq!(*closed.lock().unwrap(), [42]);
            tx.send(Command::Show(8, MessageNotice { sound:true, kind:Kind::Message, chat_id: "second-fictional-chat".into(), title: "Outra conversa".into(), body: "Aviso".into() })).await.unwrap();
            wait_for(|| received.lock().unwrap().len() == 3).await;
            stop.store(true, Ordering::Release);
            assert!(tokio::time::timeout(Duration::from_secs(3), worker).await.unwrap().unwrap().is_ok());
            assert_eq!(*closed.lock().unwrap(), [42, 43]);
        });
    }
    #[test]
    fn one_failed_notice_does_not_discard_already_queued_notices() {
        let bus = Bus::new();
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let received=Arc::new(Mutex::new(Vec::new()));
            let _server=zbus::connection::Builder::address(bus.1.as_str()).unwrap().name("org.freedesktop.Notifications").unwrap()
                .serve_at("/org/freedesktop/Notifications", Service {fail_first:AtomicBool::new(true), received:received.clone(),closed:Arc::new(Mutex::new(Vec::new())),next:std::sync::atomic::AtomicU32::new(1)}).unwrap().build().await.unwrap();
            let (tx,rx)=commands::channel(8);let (events,_)=mpsc::channel();let stop=Arc::new(AtomicBool::new(false));let cancel=stop.clone();let address=bus.1.clone();
            for i in 0..2 {tx.send(Command::Show(1,MessageNotice{sound:true,kind:Kind::Message,chat_id:format!("fixture-{i}"),title:"Teste".into(),body:"Aviso fictício".into()})).await.unwrap();}
            let worker=tokio::spawn(async move{run(rx,events,cancel,Some(&address)).await});
            wait_for(||received.lock().unwrap().len()==1).await;
            stop.store(true,Ordering::Release);
            assert!(worker.await.unwrap().is_ok());
        });
    }

    #[test]
    fn incoming_call_uses_native_sound_and_muted_alert_explicitly_suppresses_sound() {
        let bus=Bus::new();
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let received=Arc::new(Mutex::new(Vec::new()));
            let _server=zbus::connection::Builder::address(bus.1.as_str()).unwrap().name("org.freedesktop.Notifications").unwrap()
                .serve_at("/org/freedesktop/Notifications",Service{fail_first:AtomicBool::new(false),received:received.clone(),closed:Arc::new(Mutex::new(Vec::new())),next:std::sync::atomic::AtomicU32::new(1)}).unwrap().build().await.unwrap();
            let client=zbus::connection::Builder::address(bus.1.as_str()).unwrap().build().await.unwrap();
            let proxy=zbus::Proxy::new(&client,"org.freedesktop.Notifications","/org/freedesktop/Notifications","org.freedesktop.Notifications").await.unwrap();
            for sound in [true,false] {
                assert!(notify(&proxy,0,&MessageNotice{sound,kind:Kind::IncomingCall,chat_id:"fixture".into(),title:"Chamada fictícia".into(),body:"Chamada recebida".into()}).await.is_ok());
            }
            for can_chat in [false,true] {
                assert!(notify(&proxy,0,&MessageNotice{sound:true,kind:Kind::MeetingStarted{can_chat},chat_id:"meeting".into(),title:"Reunião fictícia".into(),body:"Reunião iniciada".into()}).await.is_ok());
            }
            let values=received.lock().unwrap();
            assert_eq!(values[2].actions,["join","Join"]);assert_eq!(values[3].actions,["join","Join","chat","Message"]);
            assert_eq!(values[2].category,"call.started");assert_eq!(values[2].sound.as_deref(),Some("message-new-instant"));
            assert_eq!(values[0].category,"call.incoming");assert_eq!(values[0].timeout,30_000);
            assert_eq!(values[0].actions,["default","View call"]);
            assert_eq!(values[0].sound.as_deref(),Some("phone-incoming-call"));assert!(!values[0].suppressed);
            assert_eq!(values[1].sound,None);assert!(values[1].suppressed);
        });
    }

    #[test]
    fn meeting_click_is_id_scoped_and_cancel_removes_only_its_own_native_notice() {
        let bus=Bus::new();
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let received=Arc::new(Mutex::new(Vec::new()));let closed=Arc::new(Mutex::new(Vec::new()));
            let server=zbus::connection::Builder::address(bus.1.as_str()).unwrap().name("org.freedesktop.Notifications").unwrap()
                .serve_at("/org/freedesktop/Notifications",Service{fail_first:AtomicBool::new(false),received:received.clone(),closed:closed.clone(),next:std::sync::atomic::AtomicU32::new(41)}).unwrap().build().await.unwrap();
            let client=zbus::connection::Builder::address(bus.1.as_str()).unwrap().build().await.unwrap();
            let stop=Arc::new(AtomicBool::new(false));let cancel=stop.clone();
            let worker=tokio::spawn(async move {
                let proxy=zbus::Proxy::new(&client,"org.freedesktop.Notifications","/org/freedesktop/Notifications","org.freedesktop.Notifications").await.unwrap();
                meeting_notice(&proxy,&MessageNotice{sound:false,kind:Kind::MeetingStarted{can_chat:false},chat_id:"fixture".into(),title:"Reunião fictícia".into(),body:"Teste".into()},&cancel).await
            });
            wait_for(||received.lock().unwrap().len()==1).await;
            let emitter=zbus::object_server::SignalEmitter::new(&server,"/org/freedesktop/Notifications").unwrap();
            Service::action_invoked(&emitter,999,"join").await.unwrap();
            Service::action_invoked(&emitter,41,"chat").await.unwrap();
            Service::action_invoked(&emitter,41,"unknown").await.unwrap();
            // Sentinel signal: if invalid actions close the listener, no join survives.
            Service::action_invoked(&emitter,41,"join").await.unwrap();
            assert_eq!(tokio::time::timeout(Duration::from_secs(2),worker).await.unwrap().unwrap().unwrap(),Some("join".into()));
            assert_eq!(*closed.lock().unwrap(),[41]);
            let client=zbus::connection::Builder::address(bus.1.as_str()).unwrap().build().await.unwrap();
            let cancel=stop.clone();
            let worker=tokio::spawn(async move{
                let proxy=zbus::Proxy::new(&client,"org.freedesktop.Notifications","/org/freedesktop/Notifications","org.freedesktop.Notifications").await.unwrap();
                meeting_notice(&proxy,&MessageNotice{sound:false,kind:Kind::MeetingStarted{can_chat:true},chat_id:"fixture2".into(),title:"Reunião".into(),body:"Teste".into()},&cancel).await
            });
            wait_for(||received.lock().unwrap().len()==2).await;stop.store(true,Ordering::Release);
            assert_eq!(tokio::time::timeout(Duration::from_secs(2),worker).await.unwrap().unwrap().unwrap(),None);
            assert_eq!(*closed.lock().unwrap(),[41,42]);
        });
    }

}
