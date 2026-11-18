//! Workers own only source frames. The immutable archive/catalogue is shared;
//! source metadata and records are committed in provider order by one owner.
use super::*;
use crate::semantic::native_batch::{self, Catalogue, FrameOutput, FrameSources};
use std::sync::{Arc, Condvar, Mutex, mpsc};

#[derive(Clone, Copy)]
pub(super) struct Config {
    pub workers: usize,
    pub inflight_bytes: u64,
}
impl Config {
    pub fn from_env() -> Result<Self> {
        fn value(name: &str, default: u64, max: u64) -> Result<u64> {
            match std::env::var(name) {
                Err(std::env::VarError::NotPresent) => Ok(default),
                raw => raw
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .filter(|v| *v > 0 && *v <= max)
                    .ok_or_else(|| {
                        Error::new("InvalidProviderBudget", format!("{name} must be 1..{max}"))
                    }),
            }
        }
        Ok(Self {
            workers: value(
                "CQ_NORMALIZE_WORKERS",
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(48) as u64,
                48,
            )? as usize,
            inflight_bytes: value(
                "CQ_NORMALIZE_INFLIGHT_BYTES",
                512 * 1024 * 1024,
                16 * 1024 * 1024 * 1024,
            )?,
        })
    }
}

#[derive(Default)]
struct Usage {
    bytes: u64,
    tasks: usize,
    cancelled: bool,
}
struct Budget {
    state: Mutex<Usage>,
    changed: Condvar,
    bytes: u64,
    tasks: usize,
}
impl Budget {
    fn reserve(&self, bytes: u64) -> Option<Permit<'_>> {
        let mut state = self.state.lock().unwrap();
        // A frame bigger than the estimate budget runs alone. This is a bound
        // on estimated frame working sets, not a process RSS or fact-size cap.
        while !state.cancelled
            && (state.tasks >= self.tasks
                || (state.tasks != 0 && bytes > self.bytes.saturating_sub(state.bytes)))
        {
            state = self.changed.wait(state).unwrap();
        }
        if state.cancelled {
            return None;
        }
        state.tasks += 1;
        state.bytes = state.bytes.saturating_add(bytes);
        Some(Permit {
            budget: self,
            bytes,
        })
    }
    fn cancel(&self) {
        self.state.lock().unwrap().cancelled = true;
        self.changed.notify_all();
    }
    fn cancelled(&self) -> bool {
        self.state.lock().unwrap().cancelled
    }
}
struct Permit<'a> {
    budget: &'a Budget,
    bytes: u64,
}
type FrameResult<'a> = (usize, Result<FrameOutput>, Option<Permit<'a>>);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock().unwrap();
        state.tasks -= 1;
        state.bytes -= self.bytes;
        self.budget.changed.notify_all();
    }
}
struct Cancel<'a>(&'a Budget);
impl Drop for Cancel<'_> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Archive {
    fn frame_estimate(&self, selected: &Value) -> u64 {
        let bytes = selected
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|v| v["__cq_row"].as_u64())
            .filter_map(|i| self.rows.get(i as usize))
            .fold(0u64, |n, row| n.saturating_add(row.bytes));
        // JSON allocations, the normalized facts and both representations can
        // coexist. Calibration can change without changing semantic results.
        bytes.saturating_mul(8).max(1024 * 1024)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn normalize(
    archive: &Archive,
    s: &mut SemanticSnapshot,
    mapping: &[MaterializedInput],
    content: &dyn ContentRepository,
    provider: &str,
    language: Language,
    version: &str,
    sink: &dyn Store,
    config: Config,
) -> Result<()> {
    let files = archive.metadata["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_u64())
        .chain(std::iter::once(None))
        .collect::<Vec<_>>();
    let workers = config.workers.max(1).min(files.len());
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: config.inflight_bytes,
        tasks: workers * 2,
    };
    let catalogue = Catalogue::new(&archive.metadata);
    let initial_sources = FrameSources::new(s, mapping);
    let mut sources = FrameSources::new(s, mapping);
    let mut template = SemanticSnapshot::empty(s.project.clone(), s.root.clone());
    template.builds.extend(s.builds.last().cloned());
    template.providers = s.providers.clone();
    template.provenance_records = s.provenance_records.clone();
    let mut peak = 0;
    let mut first = true;
    let mut gaps = HashSet::new();
    sink.progress_workers(&s.project, 0, files.len() as u64, workers);
    let result = std::thread::scope(|scope| -> Result<()> {
        // Drop before scope joins, including early sink errors: no producer or
        // worker may wait forever for a reservation held by the reorder queue.
        let _cancel = Cancel(&budget);
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        let (results_tx, results_rx) = mpsc::channel::<FrameResult<'_>>();
        let dispatch_results = results_tx.clone();
        let budget_ref = &budget;
        let files_ref = &files;
        let catalogue_ref = &catalogue;
        scope.spawn(move || {
            for (index, file) in files_ref.iter().copied().enumerate() {
                if budget_ref.cancelled() {
                    break;
                }
                let selected = match catalogue_ref.frame(file) {
                    Ok(frame) => frame,
                    Err(e) => {
                        let _ = dispatch_results.send((index, Err(e), None));
                        break;
                    }
                };
                let Some(permit) = budget_ref.reserve(archive.frame_estimate(&selected)) else {
                    break;
                };
                if jobs_tx.send((index, file, selected, permit)).is_err() {
                    break;
                }
            }
        });
        for _ in 0..workers {
            let jobs_rx = Arc::clone(&jobs_rx);
            let tx = results_tx.clone();
            let template = &template;
            let sources = &initial_sources;
            scope.spawn(move || {
                loop {
                    let job = jobs_rx.lock().unwrap().recv();
                    let Ok((index, file, selected, permit)) = job else {
                        break;
                    };
                    if budget_ref.cancelled() {
                        break;
                    }
                    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let frame = archive.hydrate(selected)?;
                        native_batch::prepare_frame(
                            template, &frame, mapping, content, provider, language, version, file,
                            sources,
                        )
                    }))
                    .unwrap_or_else(|_| {
                        Err(Error::new(
                            "NormalizationWorkerPanic",
                            "source frame worker panicked",
                        ))
                    });
                    if tx.send((index, output, Some(permit))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(results_tx);
        let mut pending = BTreeMap::new();
        let mut next = 0;
        while next < files.len() {
            let (index, result, permit) = results_rx.recv().map_err(|_| {
                Error::new(
                    "NormalizationWorkerStopped",
                    "source frame results incomplete",
                )
            })?;
            pending.insert(index, (result, permit));
            while let Some((result, _permit)) = pending.remove(&next) {
                let output = result?;
                peak = peak.max(output.peak);
                native_batch::merge_frame(s, output, sink, &mut first, &mut gaps, &mut sources)?;
                next += 1;
                sink.progress_workers(&s.project, next as u64, files.len() as u64, workers);
            }
        }
        Ok(())
    });
    sources.finish(s);
    result?;
    native_batch::record_transport(s, peak, files.len());
    Ok(())
}

#[cfg(test)]
mod tests;
