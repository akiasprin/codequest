use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[derive(Clone, Default)]
pub struct CaptureControl {
    cancelled: Arc<AtomicBool>,
}
impl CaptureControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
