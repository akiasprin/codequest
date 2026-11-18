//! One bounded compute pool for analysis and storage preparation. Coordinators
//! own ordering; workers perform independent transformations and bounded sorts.
use crate::{Error, Result};
use rayon::prelude::*;
use std::sync::OnceLock;

pub const MAX_WORKERS: usize = 128;

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
    }
}
