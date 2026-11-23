//! Desktop event cues, deliberately independent from capture/RTP/call devices.
use std::{sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, time::{Duration, Instant}, process::{Command, Stdio}};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue { Joined, Left }
impl Cue {
    fn event(self) -> &'static str { match self { Self::Joined=>"service-login", Self::Left=>"service-logout" } }
}
#[derive(Default)]
pub struct CallCues { connected: bool }
impl CallCues {
    pub fn observe(&mut self, phase: crate::native_call::Phase) -> Option<Cue> {
        if phase == crate::native_call::Phase::Connected && !self.connected {
            self.connected=true; Some(Cue::Joined)
        } else if phase.terminal() && std::mem::take(&mut self.connected) { Some(Cue::Left) }
        else { None }
    }
}
struct Worker { sender: mpsc::SyncSender<(Instant,Cue)>, stop: Arc<AtomicBool> }
#[derive(Default)]
pub struct Sounds { worker: Option<Worker> }
impl Sounds {
    pub fn play(&mut self, cue: Cue) {
        // Tests and ordinary fictional previews must never emit desktop audio.
        if cfg!(test) { return; }
        if self.worker.is_none() {
            let (sender,receiver)=mpsc::sync_channel::<(Instant,Cue)>(2);
            let stop=Arc::new(AtomicBool::new(false));let cancel=stop.clone();
            if std::thread::Builder::new().name("teams-event-sounds".into()).spawn(move|| {
                while !cancel.load(Ordering::Acquire) {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok((at,cue)) if at.elapsed()<Duration::from_secs(3) => {
                            if desktop_allows_sound(&cancel) && !cancel.load(Ordering::Acquire) {
                                let result=bounded(Command::new("canberra-gtk-play").args(["--id",cue.event(),"--description","Teams Linux"]),&cancel);
                                crate::live_trace::record(if result.is_some(){"event_sound_played"}else{"event_sound_failed"},0,1,0);
                            }
                        },
                        Ok(_) | Err(mpsc::RecvTimeoutError::Timeout)=>{},
                        Err(mpsc::RecvTimeoutError::Disconnected)=>break,
                    }
                }
            }).is_err() {return;}
            self.worker=Some(Worker{sender,stop});
        }
        if let Some(worker)=&self.worker {let _=worker.sender.try_send((Instant::now(),cue));}
    }
    pub fn reset(&mut self) {if let Some(worker)=self.worker.take(){worker.stop.store(true,Ordering::Release);}}
}
impl Drop for Sounds {fn drop(&mut self){self.reset();}}

// Every child has a deadline and is reaped; no shell and no UI-thread I/O.
fn bounded(command:&mut Command, stop:&AtomicBool)->Option<Vec<u8>> {
    let mut child=command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start=Instant::now();
    loop {
        if stop.load(Ordering::Acquire)||start.elapsed()>Duration::from_secs(3) {let _=child.kill();let _=child.wait();return None;}
        match child.try_wait() {
            Ok(Some(status))=>return if status.success(){child.wait_with_output().ok().map(|o|o.stdout)}else{None},
            Ok(None)=>std::thread::sleep(Duration::from_millis(20)),
            Err(_)=>{let _=child.kill();let _=child.wait();return None;}
        }
    }
}
fn desktop_allows_sound(stop:&AtomicBool)->bool {
    [
        ("org.gnome.desktop.notifications","show-banners"),
        ("org.gnome.desktop.sound","event-sounds"),
        ("org.gnome.desktop.notifications.application:/org/gnome/desktop/notifications/application/io-github-gutopardini-teamslinux/","enable"),
        ("org.gnome.desktop.notifications.application:/org/gnome/desktop/notifications/application/io-github-gutopardini-teamslinux/","enable-sound-alerts"),
    ].iter().all(|(schema,key)|bounded(Command::new("gsettings").args(["get",schema,key]),stop).is_some_and(|value|value==b"true\n"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_call::Phase;
    #[test]
    fn cues_require_actual_connection_and_do_not_repeat_on_pulses_or_failed_join() {
        let mut cues=CallCues::default();
        assert_eq!(cues.observe(Phase::Ended),None);
        assert_eq!(cues.observe(Phase::Connected),Some(Cue::Joined));
        assert_eq!(cues.observe(Phase::Connected),None);
        assert_eq!(cues.observe(Phase::Ending),None);
        assert_eq!(cues.observe(Phase::Ended),Some(Cue::Left));
        assert_eq!(cues.observe(Phase::Ended),None);
        assert_eq!(cues.observe(Phase::Connected),Some(Cue::Joined));
    }
    #[test]
    fn cancelled_children_are_reaped_without_waiting_for_their_deadline() {
        let stop=AtomicBool::new(true);let at=Instant::now();
        assert!(bounded(Command::new("sleep").arg("30"),&stop).is_none());
        assert!(at.elapsed()<Duration::from_secs(1));
    }
}
