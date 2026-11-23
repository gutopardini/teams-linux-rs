//! Periodic report sender shares the selected datagram route, never a receiver.
use super::{
    audio_control::{ControlStats, Reports},
    datagram::{Datagram, DatagramIo},
    media_protection::ControlCipher,
    srtp::SrtpContext,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};
pub struct ControlSession {
    pub origin: Instant,
    pub(crate) state: Mutex<State>,
}
pub(crate) struct State {
    pub reports: Reports,
    pub cipher: ControlCipher,
}
impl ControlSession {
    pub fn new(
        context: &SrtpContext,
        ssrc: u32,
        clock: u32,
        bandwidth: u32,
    ) -> anyhow::Result<Self> {
        let cname = format!("{}@native.invalid", uuid::Uuid::new_v4().simple());
        Ok(Self {
            origin: Instant::now(),
            state: Mutex::new(State {
                reports: Reports::new(ssrc, clock, bandwidth, cname)?,
                cipher: ControlCipher::new(context, ssrc),
            }),
        })
    }
    pub async fn stats(&self) -> ControlStats {
        self.state.lock().await.reports.stats()
    }
    pub async fn send(
        &self,
        socket: &Datagram,
        remote: SocketAddr,
        bye: bool,
    ) -> anyhow::Result<()> {
        let (plan, protected) = {
            let mut state = self.state.lock().await;
            let plan = state
                .reports
                .plan(self.origin.elapsed(), super::rtcp::ntp_timestamp(), bye);
            let protected = state.cipher.protect(&plan.bytes)?;
            (plan, protected)
        };
        // Neither a media crypto lock nor report state is held during I/O.
        let sent = tokio::time::timeout(
            Duration::from_millis(300),
            socket.send_to(&protected, remote),
        )
        .await??;
        if sent != protected.len() {
            anyhow::bail!("Incomplete SRTCP send");
        }
        self.state.lock().await.reports.commit(&plan);
        Ok(())
    }
    pub async fn run(self: Arc<Self>, socket: Arc<Datagram>, remote: SocketAddr) {
        let mut initial = true;
        loop {
            let random =
                u32::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap()) as f64
                    / (u32::MAX as f64 + 1.0);
            let delay = self.state.lock().await.reports.interval(initial, random);
            tokio::time::sleep(delay).await;
            if self.send(&socket, remote, false).await.is_err() {
                break;
            }
            initial = false;
        }
    }
}
