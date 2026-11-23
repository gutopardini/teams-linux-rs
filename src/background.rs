//! Bounded workers for synchronous operations. Iced may first-poll Tasks on UI.
//!
//! Polling (chat list, history refresh, receipts) and gestures (open a chat,
//! send, search) use separate pools. A slow directory sync must not sit in
//! front of Enter or the search field. Neither pool runs on the Iced executor:
//! a blocking Graph call there stalls redraws.
use std::sync::{mpsc, Arc, Mutex, OnceLock};
type Job = Box<dyn FnOnce() + Send + 'static>;
static WORKERS: OnceLock<Result<mpsc::SyncSender<Job>, String>> = OnceLock::new();
static INTERACTIVE: OnceLock<Result<mpsc::SyncSender<Job>, String>> = OnceLock::new();

fn pool(
    slot: &'static OnceLock<Result<mpsc::SyncSender<Job>, String>>,
    name: &str,
) -> Result<&'static mpsc::SyncSender<Job>, String> {
    slot.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Job>(64);
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..4 {
            let receiver = receiver.clone();
            let name = name.to_owned();
            std::thread::Builder::new().name(name).spawn(move || loop {
                let job = { receiver.lock().unwrap().recv() };
                let Ok(job) = job else { break; };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }).map_err(|_| "Não foi possível iniciar o serviço em segundo plano.".to_owned())?;
        }
        Ok(sender)
    }).as_ref().map_err(Clone::clone)
}

fn workers() -> Result<&'static mpsc::SyncSender<Job>, String> {
    pool(&WORKERS, "teams-io")
}

fn interactive_workers() -> Result<&'static mpsc::SyncSender<Job>, String> {
    pool(&INTERACTIVE, "teams-ui")
}

async fn dispatch<T: Send + 'static>(
    workers: Result<&'static mpsc::SyncSender<Job>, String>,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    workers?.try_send(Box::new(move || {
        let started = std::time::Instant::now();
        let result = job();
        crate::live_trace::record("background_io", started.elapsed().as_micros() as u64, 0, 0);
        let _ = sender.send(result);
    })).map_err(|_| "Serviço ocupado. Tente novamente em instantes.".to_owned())?;
    receiver.await.map_err(|_| "A operação em segundo plano foi interrompida.".to_owned())?
}

pub async fn run<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    dispatch(workers(), job).await
}

/// Chat open, send, and search. Kept off the polling pool.
pub async fn run_interactive<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    dispatch(interactive_workers(), job).await
}

/// Same lane as [`run_interactive`], for a value that is not `Result<_, String>`.
pub async fn run_interactive_value<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    interactive_workers()?.try_send(Box::new(move || {
        let started = std::time::Instant::now();
        let result = job();
        crate::live_trace::record("background_io", started.elapsed().as_micros() as u64, 0, 0);
        let _ = sender.send(result);
    })).map_err(|_| "Serviço ocupado. Tente novamente em instantes.".to_owned())?;
    receiver.await.map_err(|_| "A operação em segundo plano foi interrompida.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronous_body_always_runs_on_another_thread() {
        let caller = std::thread::current().id();
        let actual = iced::futures::executor::block_on(run(|| Ok(std::thread::current().id()))).unwrap();
        assert_ne!(caller, actual);
    }
    #[test]
    fn failed_worker_returns_error_and_pool_remains_usable() {
        let result = iced::futures::executor::block_on(run::<()>(|| panic!("synthetic worker failure")));
        assert!(result.is_err());
        assert_eq!(iced::futures::executor::block_on(run(|| Ok(42))).unwrap(), 42);
    }
}
