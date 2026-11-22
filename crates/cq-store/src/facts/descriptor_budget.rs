//! Descriptor admission for concurrent external merges. A task reserves its
//! complete input/output set before opening any file, so blocked tasks cannot
//! retain partial reservations and deadlock each other.
use cq_core::{Error, Result};
use std::sync::{Condvar, Mutex};

#[derive(Default)]
struct Usage {
    used: usize,
    peak: usize,
}
#[derive(Default)]
struct Budget {
    usage: Mutex<Usage>,
    changed: Condvar,
}
static BUDGET: Budget = Budget {
    usage: Mutex::new(Usage { used: 0, peak: 0 }),
    changed: Condvar::new(),
};
pub(super) struct Lease<'a> {
    budget: &'a Budget,
    count: usize,
}

fn os_limit() -> Result<u64> {
    let mut value = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: value is initialized and writable for the complete rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut value) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(value.rlim_cur)
}
pub(super) fn capacity() -> Result<usize> {
    // Packed writers, locator caches, journals and other threads also own
    // descriptors. Merge workers may use at most a quarter of the soft limit.
    // The fixed ceiling bounds concurrent I/O on hosts with huge OS limits.
    let capacity = (os_limit()? / 4).min(1024) as usize;
    if capacity < 3 {
        return Err(Error::new(
            "IndexDescriptorBudget",
            "file limit must allow at least two merge inputs and one output; no revision published",
        ));
    }
    Ok(capacity)
}
impl Budget {
    fn acquire(&self, count: usize, capacity: usize) -> Result<Lease<'_>> {
        if count > capacity {
            return Err(Error::new(
                "IndexDescriptorBudget",
                format!(
                    "merge needs {count} descriptors but its shared budget is {capacity}; compact inputs before merging"
                ),
            ));
        }
        let mut state = self.usage.lock().unwrap();
        while state.used + count > capacity {
            state = self.changed.wait(state).unwrap();
        }
        state.used += count;
        state.peak = state.peak.max(state.used);
        Ok(Lease {
            budget: self,
            count,
        })
    }
}
pub(super) fn acquire(count: usize) -> Result<Lease<'static>> {
    BUDGET.acquire(count, capacity()?)
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.budget.usage.lock().unwrap().used -= self.count;
        self.budget.changed.notify_all();
    }
}
pub(super) fn diagnostics() -> serde_json::Value {
    let usage = BUDGET.usage.lock().unwrap();
    serde_json::json!({"soft_limit":os_limit().ok(),"merge_capacity":capacity().ok(),
        "merge_reserved":usage.used,"merge_peak_reserved":usage.peak})
}
pub(super) fn io(error: std::io::Error, operation: &str, path: &std::path::Path) -> Error {
    if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
        Error::new(
            "FileDescriptorExhausted",
            format!(
                "{operation} {}: {error}; resources={}",
                path.display(),
                diagnostics()
            ),
        )
    } else {
        error.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn whole_reservations_are_bounded_and_drop_wakes_waiters() {
        let budget = Budget::default();
        let first = budget.acquire(3, 4).unwrap();
        assert!(budget.acquire(5, 4).is_err());
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let budget = &budget;
            scope.spawn(move || {
                let _next = budget.acquire(3, 4).unwrap();
                tx.send(()).unwrap();
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(20))
                    .is_err()
            );
            drop(first);
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        });
        let usage = budget.usage.lock().unwrap();
        assert_eq!(usage.used, 0);
        assert_eq!(usage.peak, 3);
    }
}
