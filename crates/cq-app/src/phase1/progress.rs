//! Human-facing terminal status stays on stderr; stdout remains JSON for clients.
use std::{
    io::{self, IsTerminal},
    sync::mpsc,
    thread::JoinHandle,
    time::{Duration, Instant},
};
pub(super) struct Progress {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
    label: String,
    started: Instant,
    announce: bool,
}
impl Progress {
    pub fn start(label: impl Into<String>) -> Option<Self> {
        Self::new(label.into(), true)
    }
    pub fn waiting(label: impl Into<String>) -> Option<Self> {
        Self::new(label.into(), false)
    }
    fn new(label: String, announce: bool) -> Option<Self> {
        if !io::stdin().is_terminal() {
            return None;
        }
        if announce {
            eprintln!("[CodeQuest] {label}…");
        }
        let (stop, receive) = mpsc::channel();
        let started = Instant::now();
        let message = label.clone();
        let worker = std::thread::spawn(move || {
            while receive.recv_timeout(Duration::from_secs(2))
                == Err(mpsc::RecvTimeoutError::Timeout)
            {
                eprintln!(
                    "\n[CodeQuest] {message}：已等待 {:.0} 秒；Ctrl+C 取消",
                    started.elapsed().as_secs_f64()
                );
            }
        });
        Some(Self {
            stop,
            worker: Some(worker),
            label,
            started,
            announce,
        })
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if self.announce {
            eprintln!(
                "[CodeQuest] {} · {:.2} 秒",
                self.label,
                self.started.elapsed().as_secs_f64()
            );
        }
    }
}
