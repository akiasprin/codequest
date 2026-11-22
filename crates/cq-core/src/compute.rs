//! One bounded compute pool for analysis and storage preparation. Coordinators
//! own ordering; workers perform independent transformations and bounded sorts.
use crate::{Error, Result};
use rayon::prelude::*;
use std::sync::{Condvar, Mutex, OnceLock, mpsc};

pub const MAX_WORKERS: usize = 128;
const QUEUED_BYTES: u64 = 512 * 1024 * 1024;
static ADMISSION: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());
struct Permit(u64);
impl Drop for Permit {
    fn drop(&mut self) {
        *ADMISSION.0.lock().unwrap() -= self.0;
        ADMISSION.1.notify_all();
    }
}
pub struct Job<T>(mpsc::Receiver<Result<T>>);
impl<T> Job<T> {
    pub fn try_join(self) -> std::result::Result<Result<T>, Self> {
        match self.0.try_recv() {
            Ok(result) => Ok(result),
            Err(mpsc::TryRecvError::Empty) => Err(self),
            Err(mpsc::TryRecvError::Disconnected) => {
                Ok(Err(Error::new("WorkerFailed", "queued task disconnected")))
            }
        }
    }
    pub fn join(self) -> Result<T> {
        self.0
            .recv()
            .map_err(|_| Error::new("WorkerFailed", "queued task disconnected"))?
    }
}
/// Input memory is reserved by a coordinator before submission. Only small
/// completion receipts may outlive this reservation; callers join before
/// removing any task-owned files. Worker-side admission is rejected to avoid
/// nested pool starvation with a one-thread configuration.
pub fn submit<T: Send + 'static>(
    bytes: u64,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Job<T>> {
    if rayon::current_thread_index().is_some() {
        return Err(Error::new(
            "NestedWorkerAdmission",
            "submit from the coordinator",
        ));
    }
    let pool = pool()?;
    let mut used = ADMISSION.0.lock().unwrap();
    while *used != 0 && bytes > QUEUED_BYTES.saturating_sub(*used) {
        used = ADMISSION.1.wait(used).unwrap();
    }
    *used += bytes;
    drop(used);
    let permit = Permit(bytes);
    let (tx, rx) = mpsc::sync_channel(1);
    pool.spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .unwrap_or_else(|_| Err(Error::new("WorkerFailed", "queued task panicked")));
        drop(permit);
        let _ = tx.send(result);
    });
    Ok(Job(rx))
}

fn configured(value: Option<&str>, cpus: usize) -> Result<usize> {
    value.map_or(Ok(cpus.saturating_mul(2).clamp(1, MAX_WORKERS)), |v| {
        v.parse()
            .ok()
            .filter(|n| (1..=MAX_WORKERS).contains(n))
            .ok_or_else(|| Error::new("InvalidWorkerBudget", "CQ_WORKERS must be 1..128"))
    })
}
pub fn workers() -> Result<usize> {
    let value = std::env::var("CQ_WORKERS").map_err(|e| match e {
        std::env::VarError::NotPresent => None,
        _ => Some(Error::new(
            "InvalidWorkerBudget",
            "CQ_WORKERS must be UTF-8",
        )),
    });
    match value {
        Ok(v) => configured(Some(&v), 1),
        Err(None) => configured(
            None,
            std::thread::available_parallelism().map_or(1, usize::from),
        ),
        Err(Some(e)) => Err(e),
    }
}
pub fn pool() -> Result<&'static rayon::ThreadPool> {
    static POOL: OnceLock<std::result::Result<rayon::ThreadPool, String>> = OnceLock::new();
    // Validate even after initialization, so invalid requests never silently use
    // a previously constructed pool. Environment is fixed for a CLI process.
    let count = workers()?;
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(count)
            .thread_name(|i| format!("cq-work-{i}"))
            .build()
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| Error::new("WorkerPool", e))
}
pub fn map<T: Send, U: Send>(
    items: Vec<T>,
    f: impl Fn(T) -> Result<U> + Sync + Send,
) -> Result<Vec<U>> {
    if items.len() <= 1 {
        return items.into_iter().map(f).collect();
    }
    pool()?.install(|| items.into_par_iter().map(f).collect())
}
pub fn sort_by<T: Send>(
    items: &mut [T],
    compare: impl Fn(&T, &T) -> std::cmp::Ordering + Sync + Send,
) -> Result<()> {
    pool()?.install(|| items.par_sort_unstable_by(compare));
    Ok(())
}

/// Bound splitting overhead for short per-record transformations. Coarse jobs
/// such as compiler units and index partitions keep using `map` unchanged.
pub fn map_records<T: Send, U: Send>(
    items: Vec<T>,
    f: impl Fn(T) -> Result<U> + Sync + Send,
) -> Result<Vec<U>> {
    if items.len() <= 1 {
        return items.into_iter().map(f).collect();
    }
    pool()?.install(|| items.into_par_iter().with_min_len(16).map(f).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_double_cpu_and_never_exceed_cap() {
        for (cpus, expected) in [(0, 1), (1, 2), (24, 48), (64, 128), (192, 128)] {
            assert_eq!(configured(None, cpus).unwrap(), expected);
        }
        for bad in ["0", "129", "-1", "wat"] {
            assert!(configured(Some(bad), 24).is_err());
        }
        assert_eq!(configured(Some("128"), 24).unwrap(), 128);
    }
    #[test]
    fn ordered_results_and_failures_cross_workers() {
        assert_eq!(
            map((0..1000).collect(), |n| Ok(n * 2)).unwrap(),
            (0..1000).map(|n| n * 2).collect::<Vec<_>>()
        );
        let error = map(vec![1, 2, 3], |n| {
            if n == 2 {
                Err(Error::new("Injected", "failure"))
            } else {
                Ok(n)
            }
        })
        .unwrap_err();
        assert_eq!(error.code(), "Injected");
        assert_eq!(submit(16, || Ok(42)).unwrap().join().unwrap(), 42);
        let failure = submit(16, || -> Result<()> {
            Err(Error::new("Injected", "worker failure"))
        })
        .unwrap()
        .join()
        .unwrap_err();
        assert_eq!(failure.code(), "Injected");
        assert_eq!(
            map(vec![0, 1], |_| submit(16, || Ok(1)).map(|_| ()))
                .unwrap_err()
                .code(),
            "NestedWorkerAdmission"
        );
    }
}
