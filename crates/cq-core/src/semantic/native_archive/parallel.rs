//! Workers own only source frames. The immutable archive/catalogue is shared;
//! source metadata and records are committed in provider order by one owner.
use super::*;
use crate::semantic::native_batch::{self, Catalogue, FrameMetadata, FrameOutput, FrameSources};
use std::{
    sync::{Condvar, Mutex, mpsc},
    time::Duration,
};

#[derive(Clone, Copy)]
pub(super) struct Config {
    pub workers: usize,
    pub inflight_bytes: u64,
    pub checkpoints: bool,
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
            checkpoints: true,
            workers: if std::env::var_os("CQ_WORKERS").is_some() {
                crate::compute::workers()?
            } else {
                value(
                    "CQ_NORMALIZE_WORKERS",
                    crate::compute::workers()? as u64,
                    crate::compute::MAX_WORKERS as u64,
                )? as usize
            },
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
    waiting: BTreeMap<usize, u64>,
    completed: u64,
    stages: [usize; 5],
    cancelled: bool,
    archive_rows: u64,
    archive_reads: u64,
    archive_read_bytes: u64,
}
struct Budget {
    state: Mutex<Usage>,
    changed: Condvar,
    bytes: u64,
    tasks: usize,
}
impl Budget {
    fn waiting_reason(&self, s: &Usage, index: usize, bytes: u64) -> Option<WaitReason> {
        let available = self.bytes.saturating_sub(s.bytes);
        // Oversized frames keep the existing run-alone exception. Later
        // frames may not use that exception ahead of an unfinished predecessor.
        if bytes > available && (s.bytes != 0 || index as u64 != s.completed) {
            return Some(WaitReason::Memory);
        }
        // Prefer the oldest planned frame whenever it can run. If it cannot
        // fit, smaller frames may use the remaining capacity. Their completed
        // metadata is spooled, so it cannot strand the predecessor's budget.
        if let Some((&first, &needed)) = s.waiting.first_key_value()
            && first < index
            && (needed <= available || (s.bytes == 0 && first as u64 == s.completed))
        {
            return Some(WaitReason::Priority);
        }
        None
    }
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
                    waiting_memory: s
                        .waiting
                        .iter()
                        .filter(|(index, bytes)| {
                            matches!(
                                self.waiting_reason(&s, **index, **bytes),
                                Some(WaitReason::Memory)
                            )
                        })
                        .count(),
                    waiting_priority: s
                        .waiting
                        .iter()
                        .filter(|(index, bytes)| {
                            matches!(
                                self.waiting_reason(&s, **index, **bytes),
                                Some(WaitReason::Priority)
                            )
                        })
                        .count(),
                    queued: total - s.next_issue,
                    ready: s.stages[3],
                    merging: s.stages[4],
                    inflight_bytes: s.bytes,
                    budget_bytes: self.bytes,
                    archive_rows: s.archive_rows,
                    archive_reads: s.archive_reads,
                    archive_read_bytes: s.archive_read_bytes,
                },
            )
        };
        sink.progress_pipeline(project, completed, total as u64, &pipeline);
    }
}
enum WaitReason {
    Memory,
    Priority,
}
struct Permit<'a> {
    budget: &'a Budget,
    index: usize,
    bytes: u64,
    stage: Stage,
    released: bool,
}
// Facts have already reached the conflict-checking journal. Only the remaining
// source/provenance metadata waits here. Keeping it on disk prevents a later
// frame from holding the memory needed by an earlier, larger frame. At most
// `Budget::tasks` temporary files can exist, and cancellation drops them all.
struct StoredFrame {
    file: tempfile::NamedTempFile,
}
impl StoredFrame {
    fn write(output: FrameOutput) -> Result<Self> {
        let file = tempfile::NamedTempFile::new()?;
        let mut writer = BufWriter::new(file.as_file());
        serde_json::to_writer(&mut writer, &output.into_metadata()?)?;
        writer.flush()?;
        drop(writer);
        Ok(Self { file })
    }
    fn read(self) -> Result<FrameMetadata> {
        Ok(serde_json::from_reader(BufReader::new(
            self.file.reopen()?,
        ))?)
    }
}
type FrameResult<'a> = (usize, Result<StoredFrame>, Permit<'a>);
impl Permit<'_> {
    fn admit(&mut self, bytes: u64) -> bool {
        let mut s = self.budget.state.lock().unwrap();
        s.stages[self.stage as usize] -= 1;
        self.stage = Stage::Waiting;
        s.stages[self.stage as usize] += 1;
        s.waiting.insert(self.index, bytes);
        self.budget.changed.notify_all();
        // Claiming is ordered and reserves a worker for the earliest plan.
        // The independent task window bounds speculative execution. Memory
        // admission may be out of order; metadata merge remains ordered.
        while !s.cancelled && self.budget.waiting_reason(&s, self.index, bytes).is_some() {
            s = self.budget.changed.wait(s).unwrap();
        }
        if s.cancelled {
            return false;
        }
        s.bytes += bytes;
        self.bytes = bytes;
        s.waiting.remove(&self.index);
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
    fn ready(&mut self, retained_bytes: u64) {
        let mut s = self.budget.state.lock().unwrap();
        let retained = retained_bytes.min(self.bytes);
        s.bytes -= self.bytes - retained;
        self.bytes = retained;
        s.stages[self.stage as usize] -= 1;
        self.stage = Stage::Ready;
        s.stages[self.stage as usize] += 1;
        self.budget.changed.notify_all();
    }
    fn release(&mut self, complete: bool) {
        if self.released {
            return;
        }
        let mut s = self.budget.state.lock().unwrap();
        s.tasks -= 1;
        s.waiting.remove(&self.index);
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
        bytes
            .saturating_mul(8)
            .max(1024 * 1024)
            .saturating_add(READ_AHEAD_BYTES as u64)
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
    let cursor = if config.checkpoints {
        sink.normalization_cursor()?
    } else {
        Value::Null
    };
    let resumed = cursor["frames"].as_u64().unwrap_or(0) as usize;
    if resumed > files.len() {
        return Err(Error::new(
            "InvalidStageCheckpoint",
            "normalization frame cursor",
        ));
    }
    let workers = config.workers.max(1).min(files.len());
    let budget = Budget {
        state: Mutex::new(Usage {
            next_issue: resumed,
            completed: resumed as u64,
            ..Usage::default()
        }),
        changed: Condvar::new(),
        bytes: config.inflight_bytes,
        tasks: workers * 2,
    };
    let catalogue = Catalogue::new(&archive.metadata)?;
    let initial_sources = FrameSources::new(s, mapping);
    let mut sources = FrameSources::new(s, mapping);
    let template = SemanticSnapshot::empty(s.project.clone(), s.root.clone());
    let mut peak = cursor["peak"].as_u64().unwrap_or(0) as usize;
    let mut first = resumed == 0;
    let mut gaps = s
        .gaps
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<HashSet<_>, _>>()?;
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
                    if sink.is_cancelled() {
                        budget.cancel();
                        break;
                    }
                    let index = permit.index;
                    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // Expensive closure discovery is worker work. Only row
                        // numbers are retained until bounded memory admission.
                        let plan = catalogue.plan(files[index])?;
                        if !permit.admit(archive.frame_estimate(catalogue.plan_rows(&plan))) {
                            return Ok(None);
                        }
                        let mut reader = RowReader::new(archive);
                        let frame = catalogue.render_with(&plan, |row| reader.hydrate(row))?;
                        {
                            let mut state = budget.state.lock().unwrap();
                            state.archive_rows += reader.rows;
                            state.archive_reads += reader.reads;
                            state.archive_read_bytes += reader.bytes;
                        }
                        drop(reader);
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
                        drop(frame);
                        Ok(Some(StoredFrame::write(output)?))
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
                    // Payloads have been persisted and dropped. Keep only the
                    // spooled metadata until the ordered coordinator merges
                    // it; the task-window bound remains in force independently.
                    permit.ready(0);
                    if tx.send((index, output, permit)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(results_tx);
        let mut pending = BTreeMap::new();
        let mut next = resumed;
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
                let output = result?.read()?;
                peak = peak.max(output.peak);
                native_batch::merge_metadata(s, output, &mut first, &mut gaps, &mut sources)?;
                permit.complete();
                next += 1;
                if config.checkpoints && (next.is_multiple_of(64) || next == files.len()) {
                    sources.checkpoint(s);
                    sink.checkpoint_normalization(s, next as u64, files.len() as u64, peak as u64)?;
                }
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
