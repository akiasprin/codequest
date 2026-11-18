//! Workers own only source frames. The immutable archive/catalogue is shared;
//! source metadata and records are committed in provider order by one owner.
use super::*;
use crate::semantic::native_batch::{self, Catalogue, FrameOutput, FrameSources};
use std::{
    sync::{Condvar, Mutex, mpsc},
    time::Duration,
};

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

#[derive(Clone, Copy)]
#[repr(usize)]
enum Stage {
    Planning,
    Waiting,
    Preparing,
    Ready,
    Merging,
}
#[derive(Default)]
struct Usage {
    bytes: u64,
    tasks: usize,
    next_issue: usize,
    next_admit: usize,
    completed: u64,
    stages: [usize; 5],
    cancelled: bool,
}
struct Budget {
    state: Mutex<Usage>,
    changed: Condvar,
    bytes: u64,
    tasks: usize,
}
impl Budget {
    fn claim(&self, total: usize) -> Option<Permit<'_>> {
        let mut state = self.state.lock().unwrap();
        while !state.cancelled && state.next_issue < total && state.tasks >= self.tasks {
            state = self.changed.wait(state).unwrap();
        }
        if state.cancelled || state.next_issue == total {
            return None;
        }
        let index = state.next_issue;
        state.next_issue += 1;
        state.tasks += 1;
        state.stages[Stage::Planning as usize] += 1;
        Some(Permit {
            budget: self,
            index,
            bytes: 0,
            stage: Stage::Planning,
            released: false,
        })
    }
    fn cancel(&self) {
        self.state.lock().unwrap().cancelled = true;
        self.changed.notify_all();
    }
    fn report(&self, project: &str, total: usize, workers: usize, sink: &dyn Store) {
        let (completed, pipeline) = {
            let s = self.state.lock().unwrap();
            (
                s.completed,
                crate::progress::Pipeline {
                    workers,
                    active: s.stages[0] + s.stages[2],
                    planning: s.stages[0],
                    preparing: s.stages[2],
                    waiting_admission: s.stages[1],
                    queued: total - s.next_issue,
                    ready: s.stages[3],
                    merging: s.stages[4],
                    inflight_bytes: s.bytes,
                    budget_bytes: self.bytes,
                },
            )
        };
        sink.progress_pipeline(project, completed, total as u64, &pipeline);
    }
}
struct Permit<'a> {
    budget: &'a Budget,
    index: usize,
    bytes: u64,
    stage: Stage,
    released: bool,
}
type FrameResult<'a> = (usize, Result<FrameOutput>, Permit<'a>);
impl Permit<'_> {
    fn admit(&mut self, bytes: u64) -> bool {
        let mut s = self.budget.state.lock().unwrap();
        s.stages[self.stage as usize] -= 1;
        self.stage = Stage::Waiting;
        s.stages[self.stage as usize] += 1;
        // Plans finish out of order, but payload reservations must follow file
        // order. Otherwise later results can fill the budget and prevent the
        // earliest outstanding frame from ever being admitted or merged.
        while !s.cancelled
            && (self.index != s.next_admit
                || (s.bytes != 0 && bytes > self.budget.bytes.saturating_sub(s.bytes)))
        {
            s = self.budget.changed.wait(s).unwrap();
        }
        if s.cancelled {
            return false;
        }
        s.bytes += bytes;
        self.bytes = bytes;
        s.next_admit += 1;
        s.stages[self.stage as usize] -= 1;
        self.stage = Stage::Preparing;
        s.stages[self.stage as usize] += 1;
        self.budget.changed.notify_all();
        true
    }
    fn transition(&mut self, stage: Stage) {
        let mut s = self.budget.state.lock().unwrap();
        s.stages[self.stage as usize] -= 1;
        self.stage = stage;
        s.stages[self.stage as usize] += 1;
    }
    fn release(&mut self, complete: bool) {
        if self.released {
            return;
        }
        let mut s = self.budget.state.lock().unwrap();
        s.tasks -= 1;
        s.bytes -= self.bytes;
        s.stages[self.stage as usize] -= 1;
        if complete {
            s.completed += 1;
        }
        self.released = true;
        self.budget.changed.notify_all();
    }
    fn complete(mut self) {
        self.release(true);
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.release(false);
    }
}
struct Cancel<'a>(&'a Budget);
impl Drop for Cancel<'_> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Archive {
    fn frame_estimate<'a>(&self, rows: impl Iterator<Item = &'a Value>) -> u64 {
        let bytes = rows
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
    let project = s.project.clone();
    budget.report(&project, files.len(), workers, sink);
    let result = std::thread::scope(|scope| -> Result<()> {
        let _cancel = Cancel(&budget);
        let (results_tx, results_rx) = mpsc::channel::<FrameResult<'_>>();
        for _ in 0..workers {
            let tx = results_tx.clone();
            let template = &template;
            let source_state = &initial_sources;
            let budget = &budget;
            let files = &files;
            let catalogue = &catalogue;
            scope.spawn(move || {
                while let Some(mut permit) = budget.claim(files.len()) {
                    let index = permit.index;
                    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // Expensive closure discovery is worker work. Only row
                        // numbers are retained until ordered memory admission.
                        let plan = catalogue.plan(files[index])?;
                        if !permit.admit(archive.frame_estimate(catalogue.plan_rows(&plan))) {
                            return Ok(None);
                        }
                        let frame =
                            catalogue.render_with(&plan, |row| archive.hydrate_row_ref(row))?;
                        let mut output = native_batch::prepare_frame(
                            template,
                            &frame,
                            mapping,
                            content,
                            provider,
                            language,
                            version,
                            files[index],
                            source_state,
                        )?;
                        // The journal is a concurrent, conflict-checking sink.
                        // Encode/hash/store facts in workers; the coordinator
                        // orders source/provenance/coverage metadata only.
                        // Downstream consumers read the journal after all
                        // workers join and enumerate canonical record keys.
                        crate::semantic::records::drain(&mut output.fragment, sink)?;
                        Ok(Some(output))
                    }))
                    .unwrap_or_else(|_| {
                        Err(Error::new(
                            "NormalizationWorkerPanic",
                            "source frame worker panicked",
                        ))
                    });
                    let output = match output {
                        Ok(None) => break,
                        Ok(Some(output)) => Ok(output),
                        Err(e) => Err(e),
                    };
                    permit.transition(Stage::Ready);
                    if tx.send((index, output, permit)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(results_tx);
        let mut pending = BTreeMap::new();
        let mut next = 0;
        while next < files.len() {
            match results_rx.recv_timeout(Duration::from_millis(200)) {
                Ok((index, result, permit)) => {
                    pending.insert(index, (result, permit));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    budget.report(&project, files.len(), workers, sink);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::new(
                        "NormalizationWorkerStopped",
                        "source frame results incomplete",
                    ));
                }
            }
            while let Some((result, mut permit)) = pending.remove(&next) {
                permit.transition(Stage::Merging);
                let output = result?;
                peak = peak.max(output.peak);
                native_batch::merge_frame(s, output, sink, &mut first, &mut gaps, &mut sources)?;
                permit.complete();
                next += 1;
                budget.report(&project, files.len(), workers, sink);
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
