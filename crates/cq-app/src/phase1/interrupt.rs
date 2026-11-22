//! Foreground indexing owns SIGINT; StoreHost tasks retain their own lifetime.
use cq_core::{Result, control::CaptureControl};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}
pub(super) fn run<T>(work: impl FnOnce(&CaptureControl) -> Result<T>) -> Result<T> {
    INTERRUPTED.store(false, Ordering::Relaxed);
    let control = CaptureControl::default();
    let watcher_control = control.clone();
    let done = Arc::new(AtomicBool::new(false));
    let watcher_done = done.clone();
    let previous =
        unsafe { libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t) };
    let watcher = std::thread::spawn(move || {
        while !watcher_done.load(Ordering::Relaxed) {
            if INTERRUPTED.load(Ordering::Relaxed) {
                watcher_control.cancel();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    let result = work(&control);
    done.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    unsafe {
        libc::signal(libc::SIGINT, previous);
    }
    result
}
