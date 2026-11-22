//! Initial directory construction sorts bounded runs on disk. Final group trees
//! are built once; the incremental writer retains its ordinary copy-on-write path.
#[path = "bulk_resume.rs"]
mod resume;
use super::{descriptor_budget, objects::Objects, tree, writer::Group};
use cq_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
const RUN_BYTES: usize = 8 * 1024 * 1024;
const FAN_IN: usize = 32;
fn fan_in() -> Result<usize> {
    Ok(FAN_IN.min(descriptor_budget::capacity()? - 1))
}
const DISK_BYTES: u64 = 32 * 1024 * 1024 * 1024;
// Keep ordinary directories whole. Only the complete fact/family inventories
// need key partitioning; splitting every tiny directory creates millions of
// temporary member trees that the final merge would immediately rebuild.
fn key_partitioned(directory: &str) -> bool {
    directory == "facts" || directory.starts_with("all:")
}
fn disk_budget(value: Option<&str>) -> Result<u64> {
    value.map_or(Ok(DISK_BYTES), |v| {
        v.parse::<u64>()
            .ok()
            .filter(|n| *n > 0 && *n <= 1024 * 1024 * 1024 * 1024)
            .ok_or_else(|| {
                Error::new(
                    "InvalidIndexBudget",
                    "CQ_INDEX_SCRATCH_BYTES must be 1..1099511627776 bytes per sorter",
                )
            })
    })
}
#[derive(Debug)]
struct ScratchBudget;
impl std::fmt::Display for ScratchBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("initial membership sort exhausted its live compressed scratch budget")
    }
}
impl std::error::Error for ScratchBudget {}
fn scratch_error(error: io::Error) -> Error {
    if error.get_ref().is_some_and(|e| e.is::<ScratchBudget>()) {
        Error::new(
            "IndexBuildBudget",
            "initial membership sort exceeded its live compressed scratch budget; no revision published",
        )
    } else {
        error.into()
    }
}
struct SharedDisk {
    used: AtomicU64,
    limit: u64,
}
impl SharedDisk {
    fn charge(&self, bytes: u64) -> io::Result<()> {
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(bytes).filter(|n| *n <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| io::Error::other(ScratchBudget))
    }
    fn release(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}
struct CappedFile {
    file: File,
    written: u64,
    limit: u64,
    shared: Arc<SharedDisk>,
}
impl Write for CappedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(io::Error::other(ScratchBudget));
        }
        self.shared.charge(bytes.len() as u64)?;
        match self.file.write(bytes) {
            Ok(n) => {
                self.shared.release((bytes.len() - n) as u64);
                self.written += n as u64;
                Ok(n)
            }
            Err(e) => {
                self.shared.release(bytes.len() as u64);
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct RunWriter(BufWriter<lz4_flex::frame::FrameEncoder<CappedFile>>);
impl RunWriter {
    fn new(path: &Path, allowance: u64, shared: Arc<SharedDisk>) -> Result<Self> {
        let file = CappedFile {
            file: File::create(path)
                .map_err(|e| descriptor_budget::io(e, "create sorted run", path))?,
            written: 0,
            limit: allowance,
            shared,
        };
        let info = lz4_flex::frame::FrameInfo::new().content_checksum(true);
        let encoder = lz4_flex::frame::FrameEncoder::with_frame_info(info, file);
        Ok(Self(BufWriter::new(encoder)))
    }
    fn push(&mut self, row: &Row) -> Result<()> {
        // Ordered hashes can be recomputed when a run is read. Persisting both
        // prefixes for every posting needlessly adds 130 bytes per row.
        let directory = row.directory.split_once(':').unwrap().1;
        let key = row.key.split_once(':').unwrap().1;
        self.0
            .write_all(&serde_json::to_vec(&(directory, key, &row.value))?)
            .map_err(scratch_error)?;
        self.0.write_all(b"\n").map_err(scratch_error)?;
        Ok(())
    }
    fn finish(self) -> Result<u64> {
        let encoder = self
            .0
            .into_inner()
            .map_err(|e| scratch_error(e.into_error()))?;
        let file = encoder.finish().map_err(|e| scratch_error(e.into()))?;
        Ok(file.written)
    }
}
#[derive(Serialize, Deserialize)]
struct Row {
    directory: String,
    key: Arc<str>,
    value: Arc<Value>,
}
pub(super) struct PreparedRows(Vec<(Row, usize)>);
/// A fact's ordered key and immutable reference are shared by all memberships.
/// This is only an in-memory representation; scratch tuples and Merkle leaves
/// retain their existing encodings.
pub(super) fn prepare_memberships(
    directories: Vec<String>,
    key: String,
    value: Value,
) -> Result<PreparedRows> {
    let key: Arc<str> = Arc::from(tree::order(&key));
    let key_bytes = serde_json::to_vec(&key)?.len();
    let value_bytes = serde_json::to_vec(&value)?.len();
    let value = Arc::new(value);
    let rows = directories
        .into_iter()
        .map(|directory| {
            let directory = tree::order(&directory);
            // Exact JSON size of the Row struct without serializing the same
            // reference once for every membership. Field syntax occupies 30 bytes.
            let bytes = 30 + serde_json::to_vec(&directory)?.len() + key_bytes + value_bytes;
            Ok((
                Row {
                    directory,
                    key: key.clone(),
                    value: value.clone(),
                },
                bytes,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(PreparedRows(rows))
}
#[derive(Default)]
struct PendingRuns(VecDeque<(PathBuf, cq_core::compute::Job<u64>)>);
impl std::ops::Deref for PendingRuns {
    type Target = VecDeque<(PathBuf, cq_core::compute::Job<u64>)>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for PendingRuns {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for PendingRuns {
    fn drop(&mut self) {
        while let Some((_, job)) = self.0.pop_front() {
            let _ = job.join();
        }
    }
}
struct RunBuilder {
    // Declaration order matters: join writes before removing their TempDir.
    pending: PendingRuns,
    dir: tempfile::TempDir,
    rows: Vec<Row>,
    bytes: usize,
    run_bytes: usize,
    live_bytes: u64,
    disk_bytes: u64,
    serial: u64,
    runs: Vec<(usize, PathBuf)>,
    pub project: Option<String>,
    count: u64,
    prefix: String,
    shared_disk: Arc<SharedDisk>,
    shared_progress: Option<Arc<Mutex<(u64, cq_core::progress::Progress)>>>,
    asynchronous: bool,
    control: Option<cq_core::control::CaptureControl>,
}
impl RunBuilder {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            dir: tempfile::Builder::new()
                .prefix("initial-index-")
                .tempdir_in(root)?,
            rows: vec![],
            bytes: 0,
            run_bytes: RUN_BYTES,
            live_bytes: 0,
            disk_bytes: disk_budget(std::env::var("CQ_INDEX_SCRATCH_BYTES").ok().as_deref())?,
            serial: 0,
            runs: vec![],
            project: None,
            count: 0,
            prefix: String::new(),
            shared_disk: Arc::new(SharedDisk {
                used: AtomicU64::new(0),
                limit: disk_budget(std::env::var("CQ_INDEX_SCRATCH_BYTES").ok().as_deref())?,
            }),
            shared_progress: None,
            asynchronous: false,
            control: None,
            pending: PendingRuns::default(),
        })
    }
    #[cfg(test)]
    pub fn push(&mut self, directory: &str, key: &str, value: Value) -> Result<()> {
        let row = Row {
            directory: tree::order(directory),
            key: Arc::from(tree::order(key)),
            value: Arc::new(value),
        };
        let size = serde_json::to_vec(&row)?.len();
        if !self.rows.is_empty() && self.bytes + size > self.run_bytes {
            self.flush()?;
        }
        self.bytes += size;
        self.rows.push(row);
        self.count += 1;
        Ok(())
    }
    fn path(&mut self) -> PathBuf {
        self.serial += 1;
        self.dir.path().join(format!("{}.jsonl.lz4", self.serial))
    }
    fn flush(&mut self) -> Result<()> {
        self.collect_ready()?;
        if self.rows.is_empty() {
            return Ok(());
        }
        if self.asynchronous {
            let path = self.path();
            let output = path.clone();
            let mut rows = std::mem::take(&mut self.rows);
            let estimate = (self.bytes as u64)
                .saturating_mul(4)
                .saturating_add(8 * 1024 * 1024);
            let allowance = self.disk_bytes.saturating_sub(self.live_bytes);
            let disk = self.shared_disk.clone();
            let job = cq_core::compute::submit(estimate, move || {
                rows.sort_unstable_by(|a, b| (&a.directory, &a.key).cmp(&(&b.directory, &b.key)));
                let mut out = RunWriter::new(&output, allowance, disk)?;
                for row in rows {
                    out.push(&row)?;
                }
                out.finish()
            })?;
            self.bytes = 0;
            self.pending.push_back((path, job));
            return Ok(());
        }
        cq_core::compute::sort_by(&mut self.rows, |a, b| {
            (&a.directory, &a.key).cmp(&(&b.directory, &b.key))
        })?;
        let path = self.path();
        let mut out = RunWriter::new(
            &path,
            self.disk_bytes.saturating_sub(self.live_bytes),
            self.shared_disk.clone(),
        )?;
        for row in self.rows.drain(..) {
            out.push(&row)?;
        }
        self.live_bytes += out.finish()?;
        self.bytes = 0;
        self.runs.push((0, path));
        if self.prefix.is_empty() {
            self.compact_levels()?;
        }
        Ok(())
    }
    fn join_pending(&mut self) -> Result<()> {
        while let Some((path, job)) = self.pending.pop_front() {
            let bytes = job.join()?;
            self.live_bytes += bytes;
            self.runs.push((0, path));
        }
        Ok(())
    }
    fn collect_ready(&mut self) -> Result<()> {
        while let Some((path, job)) = self.pending.pop_front() {
            match job.try_join() {
                Ok(result) => {
                    self.live_bytes += result?;
                    self.runs.push((0, path));
                }
                Err(job) => {
                    self.pending.push_front((path, job));
                    break;
                }
            }
        }
        Ok(())
    }
    fn compact_levels(&mut self) -> Result<()> {
        let fan_in = fan_in()?;
        for level in 0..64 {
            loop {
                let selected = self
                    .runs
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.0 == level)
                    .map(|(i, _)| i)
                    .take(fan_in)
                    .collect::<Vec<_>>();
                if selected.len() < fan_in {
                    break;
                }
                self.merge_runs(selected, level + 1)?;
            }
        }
        Ok(())
    }
    fn merge_runs(&mut self, indices: Vec<usize>, level: usize) -> Result<()> {
        let paths = indices
            .iter()
            .map(|i| self.runs[*i].1.clone())
            .collect::<Vec<_>>();
        let path = self.path();
        let mut merged = Merge::open(&paths)?;
        let mut out = RunWriter::new(
            &path,
            self.disk_bytes.saturating_sub(self.live_bytes),
            self.shared_disk.clone(),
        )?;
        for row in &mut merged {
            if self.control.as_ref().is_some_and(|c| c.is_cancelled()) {
                return Err(Error::new("Cancelled", "directory merge paused"));
            }
            out.push(&row?)?;
        }
        let written = out.finish()?;
        drop(merged);
        self.live_bytes += written;
        for i in indices.into_iter().rev() {
            self.runs.remove(i);
        }
        for old in paths {
            let bytes = std::fs::metadata(&old)?.len();
            std::fs::remove_file(old)?;
            self.live_bytes -= bytes;
            self.shared_disk.release(bytes);
        }
        self.runs.push((level, path));
        Ok(())
    }
    fn build_with_count(mut self, o: &Objects) -> Result<(tree::Root, u64)> {
        let mut progress = super::limits::root(&o.root)
            .zip(self.project.as_deref())
            .filter(|_| self.shared_progress.is_none())
            .map(|(store, project)| {
                cq_core::progress::Progress::stage(
                    store,
                    project,
                    "sort",
                    "归并关系索引",
                    None,
                    "排序段",
                )
            });
        self.flush()?;
        self.join_pending()?;
        self.compact_levels()?;
        let fan_in = fan_in()?;
        while self.runs.len() > fan_in {
            self.merge_runs((0..fan_in).collect(), 64)?;
        }
        let paths = self.runs.iter().map(|r| r.1.clone()).collect::<Vec<_>>();
        let mut merged = Merge::open(&paths)?.peekable();
        let mut failed = false;
        let mut completed = 0;
        let mut reported = 0;
        if let Some(p) = &mut progress {
            p.detail(
                "bulk_tree",
                "批量构建关系目录",
                0,
                Some(self.count),
                "索引项",
                1,
            );
        }
        let directories = std::iter::from_fn(|| {
            if failed {
                return None;
            }
            let directory = match merged.peek()? {
                Ok(row) => row.directory.clone(),
                Err(_) => {
                    failed = true;
                    return Some(Err(merged.next().unwrap().err().unwrap()));
                }
            };
            let rows = std::iter::from_fn(|| {
                if self.control.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Some(Err(Error::new("Cancelled", "directory range paused")));
                }
                match merged.peek() {
                    Some(Ok(row)) if row.directory == directory => {
                        completed += 1;
                        if completed - reported >= 1024
                            && let Some(shared) = &self.shared_progress
                        {
                            let mut p = shared.lock().unwrap();
                            p.0 += completed - reported;
                            reported = completed;
                            let n = p.0;
                            p.1.count(n);
                        }
                        if let Some(p) = &mut progress {
                            p.count(completed);
                        }
                        Some(
                            merged
                                .next()
                                .unwrap()
                                .map(|r| (r.key.to_string(), Arc::unwrap_or_clone(r.value))),
                        )
                    }
                    Some(Err(_)) => Some(
                        merged
                            .next()
                            .unwrap()
                            .map(|r| (r.key.to_string(), Arc::unwrap_or_clone(r.value))),
                    ),
                    _ => None,
                }
            });
            let prefix = if key_partitioned(directory.get(65..).unwrap_or("")) {
                self.prefix.as_str()
            } else {
                ""
            };
            let result = tree::build_sorted_prefix(o, rows, prefix).and_then(|(root, count)| {
                Ok((directory, serde_json::to_value(Group { root, count })?))
            });
            if result.is_err() {
                failed = true;
            }
            Some(result)
        });
        let result = tree::build_sorted(o, directories)?;
        if let Some(shared) = &self.shared_progress {
            let mut p = shared.lock().unwrap();
            p.0 += completed - reported;
            let n = p.0;
            p.1.count(n);
        }
        Ok(result)
    }
    #[cfg(test)]
    fn build(self, o: &Objects) -> Result<tree::Root> {
        self.build_with_count(o).map(|(root, _)| root)
    }
}
impl Drop for RunBuilder {
    fn drop(&mut self) {
        while let Some((_, job)) = self.pending.pop_front() {
            let _ = job.join();
        }
        let bytes = std::fs::read_dir(self.dir.path())
            .into_iter()
            .flatten()
            .filter_map(|p| p.ok().and_then(|p| p.metadata().ok()))
            .map(|m| m.len())
            .sum();
        self.shared_disk.release(bytes);
    }
}
#[derive(Default, Debug, Serialize)]
pub(super) struct BuildStats {
    pub directory_inputs: u64,
    pub directories: u64,
    pub maximum_buffered_directories: usize,
}
const MERGE_DIRECTORIES: usize = 256;
pub(super) struct Builder {
    parts: Vec<RunBuilder>,
    pub project: Option<String>,
    count: u64,
    resume: Option<PathBuf>,
}
impl Builder {
    pub fn new(root: &Path) -> Result<Self> {
        let shared = Arc::new(SharedDisk {
            used: AtomicU64::new(0),
            limit: disk_budget(std::env::var("CQ_INDEX_SCRATCH_BYTES").ok().as_deref())?,
        });
        let parts = (0..16)
            .map(|n| {
                let mut p = RunBuilder::new(root)?;
                p.prefix = format!("{n:x}");
                p.shared_disk = shared.clone();
                p.asynchronous = match std::env::var("CQ_INDEX_RUN_FLUSH").ok().as_deref() {
                    Some("sync") => false,
                    None | Some("async") => true,
                    _ => {
                        return Err(Error::new(
                            "InvalidIndexBudget",
                            "CQ_INDEX_RUN_FLUSH must be sync or async",
                        ));
                    }
                };
                // Coarse run tasks overlap sorting/compression/writes with
                // continued payload admission on the coordinator.
                Ok(p)
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            parts,
            project: None,
            count: 0,
            resume: None,
        })
    }
    pub fn push(&mut self, directory: &str, key: &str, value: Value) -> Result<()> {
        self.push_many(vec![(directory.into(), key.into(), value)])
    }
    pub fn push_many(&mut self, rows: Vec<(String, String, Value)>) -> Result<()> {
        let rows = cq_core::compute::map_records(rows, |(directory, key, value)| {
            prepare_memberships(vec![directory], key, value)
        })?;
        self.push_prepared(rows)
    }
    pub fn push_prepared(&mut self, rows: Vec<PreparedRows>) -> Result<()> {
        let mut partitions: Vec<Vec<(Row, usize)>> =
            (0..self.parts.len()).map(|_| Vec::new()).collect();
        let mut count = 0;
        for (row, size) in rows.into_iter().flat_map(|r| r.0) {
            let partition: &str = if key_partitioned(row.directory.get(65..).unwrap_or("")) {
                &row.key
            } else {
                &row.directory
            };
            let index = (partition.as_bytes()[0] as char).to_digit(16).unwrap() as usize;
            partitions[index].push((row, size));
            count += 1;
        }
        // Independent bucket writers sort/compress/flush on the shared pool.
        // Rows within each partition keep their original order. This joins all
        // writes before releasing their temporary directories, including errors.
        let append = |(p, rows): (&mut RunBuilder, Vec<(Row, usize)>)| {
            for (row, size) in rows {
                if !p.rows.is_empty() && p.bytes + size > p.run_bytes {
                    p.flush()?;
                }
                p.bytes += size;
                p.rows.push(row);
                p.count += 1;
            }
            Ok(())
        };
        let parts = self
            .parts
            .iter_mut()
            .zip(partitions)
            .filter(|(_, rows)| !rows.is_empty())
            .collect::<Vec<_>>();
        // The optional queued-flush benchmark admits jobs at the coordinator;
        // it must not perform admission inside a pool worker.
        if parts.iter().any(|(p, _)| p.asynchronous) {
            for part in parts {
                append(part)?;
            }
        } else {
            cq_core::compute::map(parts, append)?;
        }
        self.count += count;
        Ok(())
    }
    pub(crate) fn cancel_with(&mut self, control: cq_core::control::CaptureControl) {
        for part in &mut self.parts {
            part.control = Some(control.clone());
        }
    }
    pub(crate) fn resume_in(&mut self, path: PathBuf) -> Result<()> {
        crate::atomic::create_directory(&path)?;
        self.resume = Some(path);
        Ok(())
    }
    pub fn build(self, o: &Objects) -> Result<tree::Root> {
        self.build_profiled(o).map(|(root, _)| root)
    }
    pub fn build_pair(
        mut facts: Self,
        mut groups: Self,
        o: &Objects,
    ) -> Result<(tree::Root, tree::Root)> {
        let project = facts.project.as_ref().or(groups.project.as_ref()).cloned();
        let progress = super::limits::root(&o.root)
            .zip(project.as_ref())
            .map(|(root, project)| {
                Mutex::new((
                    0,
                    cq_core::progress::Progress::stage_with_workers(
                        root,
                        project,
                        "encode.directories",
                        "并行构建事实与查询目录",
                        Some(2),
                        "索引",
                        cq_core::compute::workers().unwrap_or(1),
                    ),
                ))
            });
        if let Some(project) = project {
            facts.project = Some(format!("{project}:fact-directory"));
            groups.project = Some(format!("{project}:query-directories"));
        }
        let build = |b: Self| {
            let result = b.build(o);
            if result.is_ok()
                && let Some(p) = &progress
            {
                let mut p = p.lock().unwrap();
                p.0 += 1;
                let n = p.0;
                p.1.count(n);
            }
            result
        };
        // Only coordinators are separate: all sorting and hashing still use
        // the one bounded compute pool. Both builds join on every error path.
        std::thread::scope(|scope| {
            let fact = scope.spawn(|| build(facts));
            let groups = build(groups);
            let facts = fact
                .join()
                .map_err(|_| Error::new("WorkerFailed", "fact directory coordinator panicked"))?;
            Ok((facts?, groups?))
        })
    }
    fn build_profiled(mut self, o: &Objects) -> Result<(tree::Root, BuildStats)> {
        // Final buffers and all queued writes are settled by the coordinator.
        // Range workers never wait for jobs queued behind themselves.
        if self.parts.iter().any(|p| p.asynchronous) {
            for p in &mut self.parts {
                p.flush()?;
            }
        } else {
            self.parts = cq_core::compute::map(self.parts, |mut p| {
                if p.control.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(Error::new("Cancelled", "directory sorting paused"));
                }
                p.flush()?;
                Ok(p)
            })?;
        }
        let total = self.parts.iter().map(|p| p.pending.len() as u64).sum();
        let mut pending_progress = super::limits::root(&o.root)
            .zip(self.project.as_deref())
            .map(|(root, project)| {
                cq_core::progress::Progress::stage_with_workers(
                    root,
                    project,
                    "encode.runs",
                    "确认索引段写入",
                    Some(total),
                    "段",
                    cq_core::compute::workers().unwrap_or(1),
                )
            });
        let mut done = 0;
        for p in &mut self.parts {
            let n = p.pending.len() as u64;
            p.join_pending()?;
            done += n;
            if let Some(progress) = &mut pending_progress {
                progress.count(done);
            }
        }
        let sorting = super::limits::root(&o.root)
            .zip(self.project.as_deref())
            .map(|(root, project)| {
                Mutex::new((
                    0u64,
                    cq_core::progress::Progress::stage_with_workers(
                        root,
                        project,
                        "sort_ranges",
                        "并行排序与归并",
                        Some(16),
                        "范围",
                        cq_core::compute::workers().unwrap_or(1),
                    ),
                ))
            });
        self.parts = cq_core::compute::map(self.parts, |mut p| {
            p.flush()?;
            p.compact_levels()?;
            let fan_in = fan_in()?;
            while p.runs.len() > fan_in {
                p.merge_runs((0..fan_in).collect(), 64)?;
            }
            if let Some(progress) = &sorting {
                let mut progress = progress.lock().unwrap();
                progress.0 += 1;
                let n = progress.0;
                progress.1.count(n);
            }
            Ok(p)
        })?;
        let shared = super::limits::root(&o.root)
            .zip(self.project.as_deref())
            .map(|(root, project)| {
                let mut p = cq_core::progress::Progress::stage_with_workers(
                    root,
                    project,
                    "bulk_tree",
                    "并行构建索引目录",
                    Some(self.count),
                    "索引项",
                    cq_core::compute::workers().unwrap_or(1),
                );
                p.detail(
                    "bulk_tree",
                    "并行构建索引目录",
                    0,
                    Some(self.count),
                    "索引项",
                    cq_core::compute::workers().unwrap_or(1) as u64,
                );
                Arc::new(Mutex::new((0, p)))
            });
        for p in &mut self.parts {
            p.shared_progress = shared.clone();
        }
        let resume = self.resume.clone();
        let roots =
            cq_core::compute::map(self.parts.into_iter().enumerate().collect(), |(n, p)| {
                let checkpoint = resume.as_ref().map(|d| d.join(format!("part-{n}.json")));
                if let Some(path) = checkpoint.as_ref().filter(|p| p.exists()) {
                    let (root, count): (tree::Root, u64) =
                        serde_json::from_reader(File::open(path)?)?;
                    if let Some(progress) = &p.shared_progress {
                        let mut progress = progress.lock().unwrap();
                        progress.0 += p.count;
                        let done = progress.0;
                        progress.1.count(done);
                    }
                    return Ok((n, root, count));
                }
                let (root, count) = p.build_with_count(o)?;
                if let Some(path) = checkpoint {
                    o.sync_pending()?;
                    crate::atomic::immutable_write(&path, &serde_json::to_vec(&(&root, count))?)?;
                }
                Ok((n, root, count))
            })?;
        let total = roots.iter().map(|(_, _, n)| *n).sum();
        let mut progress = super::limits::root(&o.root)
            .zip(self.project.as_deref())
            .map(|(store, project)| {
                cq_core::progress::Progress::stage_with_workers(
                    store,
                    project,
                    "bulk_merge",
                    "合并分片目录",
                    Some(total),
                    "分片目录",
                    cq_core::compute::workers().unwrap_or(1),
                )
            });
        let mut merged =
            DirectoryMerge::new(o, roots.into_iter().map(|(n, r, _)| (n, r)).collect())?;
        let mut pending = VecDeque::new();
        let mut stats = BuildStats::default();
        let mut failed = false;
        let rows = std::iter::from_fn(|| {
            if failed {
                return None;
            }
            if pending.is_empty() {
                let wave = merged
                    .by_ref()
                    .take(MERGE_DIRECTORIES)
                    .collect::<Result<Vec<_>>>();
                let combined = wave.and_then(|wave| {
                    stats.maximum_buffered_directories =
                        stats.maximum_buffered_directories.max(wave.len());
                    stats.directories += wave.len() as u64;
                    cq_core::compute::map(wave, |(directory, parts)| {
                        if !key_partitioned(directory.get(65..).unwrap_or("")) {
                            if parts.len() != 1 {
                                return Err(Error::new(
                                    "InvalidSortedIndex",
                                    "whole directory spans multiple ranges",
                                ));
                            }
                            let (_, group) = parts.into_iter().next().unwrap();
                            return Ok((directory, serde_json::to_value(group)?));
                        }
                        let count = parts.iter().map(|(_, p)| p.count).sum::<u64>();
                        let root = if count <= 32 {
                            let mut values = std::collections::BTreeMap::new();
                            for (_, part) in parts {
                                if let Some(r) = part.root {
                                    match o.decode::<tree::Node>(
                                        &r,
                                        &mut cq_core::ports::facts::ReadStats::default(),
                                    )? {
                                        tree::Node::Leaf(v) => values.extend(v),
                                        _ => {
                                            return Err(Error::new(
                                                "InvalidSortedIndex",
                                                "small partition is not a leaf",
                                            ));
                                        }
                                    }
                                }
                            }
                            if count == 0 {
                                None
                            } else {
                                Some(o.json(&tree::Node::Leaf(values))?)
                            }
                        } else {
                            let children = parts
                                .into_iter()
                                .filter_map(|(n, p)| p.root.map(|r| (n, r)))
                                .collect();
                            Some(o.json(&tree::Node::Branch(children))?)
                        };
                        Ok((directory, serde_json::to_value(Group { root, count })?))
                    })
                });
                match combined {
                    Ok(rows) => pending.extend(rows),
                    Err(e) => {
                        failed = true;
                        return Some(Err(e));
                    }
                }
                stats.directory_inputs = merged.consumed;
                if let Some(progress) = &mut progress {
                    progress.count(merged.consumed);
                }
            }
            pending.pop_front().map(Ok)
        });
        let root = tree::build_sorted(o, rows)?.0;
        Ok((root, stats))
    }
}
struct DirectoryMerge<'a> {
    cursors: Vec<tree::Ordered<'a>>,
    parts: Vec<u8>,
    heads: Vec<Option<Value>>,
    heap: BinaryHeap<Reverse<(String, usize)>>,
    consumed: u64,
    failed: bool,
}
impl<'a> DirectoryMerge<'a> {
    fn new(o: &'a Objects, roots: Vec<(usize, tree::Root)>) -> Result<Self> {
        let mut m = Self {
            cursors: vec![],
            parts: vec![],
            heads: vec![],
            heap: BinaryHeap::new(),
            consumed: 0,
            failed: false,
        };
        for (part, root) in roots {
            m.cursors.push(tree::Ordered::new(o, &root));
            m.parts.push(part as u8);
            m.heads.push(None);
        }
        for i in 0..m.cursors.len() {
            m.advance(i)?;
        }
        Ok(m)
    }
    fn advance(&mut self, i: usize) -> Result<()> {
        if let Some(row) = self.cursors[i].next() {
            let (key, value) = row?;
            self.heap.push(Reverse((key, i)));
            self.heads[i] = Some(value);
        }
        Ok(())
    }
}
impl Iterator for DirectoryMerge<'_> {
    type Item = Result<(String, Vec<(u8, Group)>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let name = self.heap.peek()?.0.0.clone();
        let result = (|| {
            let mut parts = Vec::new();
            while self.heap.peek().is_some_and(|p| p.0.0 == name) {
                let Reverse((_, i)) = self.heap.pop().unwrap();
                let group = serde_json::from_value(self.heads[i].take().unwrap())?;
                parts.push((self.parts[i], group));
                self.consumed += 1;
                self.advance(i)?;
            }
            Ok((name, parts))
        })();
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }
}
struct Merge {
    readers: Vec<BufReader<lz4_flex::frame::FrameDecoder<File>>>,
    rows: Vec<Option<Row>>,
    heap: BinaryHeap<Reverse<(String, String, usize)>>,
    failed: bool,
    // Release the reservation after readers have closed, including open and
    // decode errors and early iterator drop.
    _descriptors: descriptor_budget::Lease<'static>,
}
impl Merge {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let descriptors = descriptor_budget::acquire(paths.len() + 1)?;
        let readers = paths
            .iter()
            .map(|p| {
                File::open(p)
                    .map(|f| BufReader::new(lz4_flex::frame::FrameDecoder::new(f)))
                    .map_err(|e| descriptor_budget::io(e, "open merge input", p))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut s = Self {
            rows: (0..readers.len()).map(|_| None).collect(),
            readers,
            heap: BinaryHeap::new(),
            failed: false,
            _descriptors: descriptors,
        };
        for i in 0..s.readers.len() {
            s.advance(i)?;
        }
        Ok(s)
    }
    fn advance(&mut self, i: usize) -> Result<()> {
        let mut line = String::new();
        if self.readers[i].read_line(&mut line)? == 0 {
            return Ok(());
        }
        let (directory, key, value): (String, String, Value) = serde_json::from_str(&line)?;
        let row = Row {
            directory: tree::order(&directory),
            key: Arc::from(tree::order(&key)),
            value: Arc::new(value),
        };
        self.heap
            .push(Reverse((row.directory.clone(), row.key.to_string(), i)));
        self.rows[i] = Some(row);
        Ok(())
    }
}
impl Iterator for Merge {
    type Item = Result<Row>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let Reverse((_, _, i)) = self.heap.pop()?;
        let row = self.rows[i].take().unwrap();
        if let Err(e) = self.advance(i) {
            self.failed = true;
            return Some(Err(e));
        }
        Some(Ok(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_merges_respect_os_descriptor_headroom() {
        const TEST: &str = "facts::bulk::tests::parallel_merges_respect_os_descriptor_headroom";
        if std::env::var_os("CQ_TEST_MERGE_FD_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--nocapture"])
                .env("CQ_TEST_MERGE_FD_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated descriptor regression:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            return;
        }
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // The test process alone changes its limit; parallel parent tests and
        // the user's shell retain their original configuration.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        limit.rlim_cur = limit.rlim_cur.min(1024);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let held = (0..256)
            .map(|_| File::open("/dev/null").unwrap())
            .collect::<Vec<_>>();
        let temp = tempfile::tempdir().unwrap();
        let disk = Arc::new(SharedDisk {
            used: AtomicU64::new(0),
            limit: 1024 * 1024,
        });
        let mut paths = Vec::new();
        for n in 0..32 {
            let path = temp.path().join(format!("{n}.lz4"));
            let mut writer = RunWriter::new(&path, 1024 * 1024, disk.clone()).unwrap();
            writer
                .push(&Row {
                    directory: tree::order("facts"),
                    key: Arc::from(tree::order(&format!("k{n}"))),
                    value: Arc::new(serde_json::json!(n)),
                })
                .unwrap();
            writer.finish().unwrap();
            paths.push(path);
        }
        let barrier = Arc::new(std::sync::Barrier::new(33));
        let result = std::thread::scope(|scope| {
            let handles = (0..32)
                .map(|_| {
                    let barrier = &barrier;
                    let paths = &paths;
                    scope.spawn(move || {
                        barrier.wait();
                        let merged = Merge::open(paths)?;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        let rows = merged.collect::<Result<Vec<_>>>()?;
                        assert_eq!(rows.len(), 32);
                        Ok::<_, Error>(())
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            let mut peak = 0;
            while handles.iter().any(|h| !h.is_finished()) {
                if let Ok(files) = std::fs::read_dir("/proc/self/fd") {
                    peak = peak.max(files.count());
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let results = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Result<Vec<_>>>();
            eprintln!(
                "descriptor stress: soft_limit={}, concurrent_merges=32, inputs_per_merge=32, unrelated_handles={}, peak_observed={peak}",
                limit.rlim_cur,
                held.len()
            );
            results
        });
        result.unwrap();
        assert_eq!(descriptor_budget::diagnostics()["merge_reserved"], 0);
        assert!(Merge::open(&[temp.path().join("missing")]).is_err());
        assert_eq!(descriptor_budget::diagnostics()["merge_reserved"], 0);
        let corrupt = temp.path().join("corrupt.lz4");
        std::fs::write(&corrupt, b"not a valid compressed run").unwrap();
        assert!(Merge::open(&[corrupt]).is_err());
        assert_eq!(descriptor_budget::diagnostics()["merge_reserved"], 0);
        let merged = Merge::open(&paths).unwrap();
        assert_eq!(descriptor_budget::diagnostics()["merge_reserved"], 33);
        drop(merged);
        assert_eq!(descriptor_budget::diagnostics()["merge_reserved"], 0);
    }
    #[test]
    fn abandoned_run_builder_joins_writes_before_removing_its_directory() {
        use std::sync::mpsc;
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let mut builder = RunBuilder::new(temp.path()).unwrap();
        let directory = builder.dir.path().to_path_buf();
        let output = directory.join("held-run");
        let worker_path = output.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (dropping_tx, dropping_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let job = cq_core::compute::submit(1024, move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            std::fs::write(worker_path, b"completed before cleanup")?;
            Ok(24)
        })
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        builder.pending.push_back((output, job));
        let thread = std::thread::spawn(move || {
            dropping_tx.send(()).unwrap();
            drop(builder);
            dropped_tx.send(()).unwrap();
        });
        dropping_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(directory.exists());
        assert!(matches!(
            dropped_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        assert!(!directory.exists());
    }
    #[test]
    fn shared_membership_rows_preserve_exact_json_size_and_values() {
        let value = serde_json::json!({"quoted":"a\"b","unicode":"路径","nested":[1,true,null]});
        let rows = prepare_memberships(
            vec!["all:entity".into(), "document_gram:\"\n路径".into()],
            "entity:标识\"".into(),
            value.clone(),
        )
        .unwrap();
        assert_eq!(rows.0.len(), 2);
        assert!(Arc::ptr_eq(&rows.0[0].0.value, &rows.0[1].0.value));
        assert!(Arc::ptr_eq(&rows.0[0].0.key, &rows.0[1].0.key));
        for (row, bytes) in rows.0 {
            assert_eq!(serde_json::to_vec(&row).unwrap().len(), bytes);
            assert_eq!(*row.value, value);
        }
    }
    #[test]
    fn performance_many_directories_stream_in_bounded_waves_with_the_exact_same_root() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut parallel = Builder::new(&o.root).unwrap();
        assert!(parallel.parts.iter().all(|part| part.asynchronous));
        let mut serial = RunBuilder::new(&o.root).unwrap();
        for i in 0..4097 {
            let key = format!("entity:{i}");
            for name in [format!("unique:{i}"), "fanout".to_owned()] {
                let value = serde_json::json!({"row":i});
                serial.push(&name, &key, value.clone()).unwrap();
                parallel.push(&name, &key, value).unwrap();
            }
        }
        let expected = serial.build(&o).unwrap();
        let (actual, stats) = parallel.build_profiled(&o).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(stats.directories, 4098);
        assert!(stats.maximum_buffered_directories <= MERGE_DIRECTORIES);
        assert_eq!(
            stats.directory_inputs, 4098,
            "ordinary directories must not be split into temporary per-key trees"
        );
        o.sync_pending().unwrap();
        let reopened = Objects::new(o.root.clone()).unwrap();
        let mut count = 0;
        tree::visit(&reopened, &actual, |_, _| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 4098);
    }
    #[test]
    fn parallel_hash_partitions_preserve_serial_merkle_root_and_group_counts() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut b = Builder::new(&o.root).unwrap();
        for p in &mut b.parts {
            p.run_bytes = 1024;
        }
        let mut expected =
            std::collections::BTreeMap::<String, std::collections::BTreeMap<String, Value>>::new();
        for (group, n) in [
            ("empty", 0),
            ("small", 1),
            ("boundary", 32),
            ("split", 33),
            ("large", 1200),
            ("all:small", 1),
            ("all:boundary", 32),
            ("all:split", 33),
            ("facts", 1200),
        ] {
            for i in (0..n).rev() {
                let key = format!("entity:{group}-{i}");
                let value = serde_json::json!({"n":i});
                b.push(group, &key, value.clone()).unwrap();
                expected.entry(group.into()).or_default().insert(key, value);
            }
        }
        let mut directories = vec![];
        for (name, values) in expected {
            let count = values.len() as u64;
            let root = tree::edit(
                &o,
                &None,
                values.into_iter().map(|(k, v)| (k, Some(v))).collect(),
                &mut cq_core::ports::facts::ReadStats::default(),
            )
            .unwrap();
            directories.push((
                name,
                Some(serde_json::to_value(Group { root, count }).unwrap()),
            ));
        }
        let serial = tree::edit(
            &o,
            &None,
            directories,
            &mut cq_core::ports::facts::ReadStats::default(),
        )
        .unwrap();
        let parallel = b.build(&o).unwrap();
        assert_eq!(parallel, serial);
        let mut duplicate = Builder::new(&o.root).unwrap();
        duplicate.push("g", "same", Value::Null).unwrap();
        duplicate.push("g", "same", Value::Null).unwrap();
        assert_eq!(
            duplicate.build(&o).unwrap_err().code(),
            "InvalidSortedIndex"
        );
    }
    #[test]
    fn queued_write_failure_never_returns_a_root_and_joins_before_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut b = Builder::new(&o.root).unwrap();
        let shared = Arc::new(SharedDisk {
            used: AtomicU64::new(0),
            limit: 1,
        });
        for p in &mut b.parts {
            p.shared_disk = shared.clone();
            p.asynchronous = true;
            p.run_bytes = 128;
        }
        let mut failure = None;
        for i in 0..100 {
            if let Err(e) = b.push("g", &format!("entity:{i}"), serde_json::json!({"n":i})) {
                failure = Some(e);
                break;
            }
        }
        let result = if let Some(e) = failure {
            drop(b);
            Err(e)
        } else {
            b.build(&o)
        };
        assert_eq!(result.unwrap_err().code(), "IndexBuildBudget");
        assert!(!std::fs::read_dir(&o.root).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("initial-index-")
        }));
    }
    use cq_core::ports::facts::ReadStats;
    use serde_json::json;
    use std::collections::BTreeMap;
    #[test]
    fn external_runs_produce_the_same_complete_merkle_roots_as_the_existing_map() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::new(temp.path().join("objects")).unwrap();
        let mut bulk = RunBuilder::new(&o.root).unwrap();
        bulk.run_bytes = 1024; // Exercise multiple fan-in levels with small data.
        let mut expected = BTreeMap::<String, Vec<(String, Option<Value>)>>::new();
        for i in (0..1400).rev() {
            for name in ["all:entity".to_owned(), format!("term:{}", i % 43)] {
                let key = format!("entity:f{i}");
                let value = json!({"digest":i,"offset":i*11});
                bulk.push(&name, &key, value.clone()).unwrap();
                expected.entry(name).or_default().push((key, Some(value)));
            }
        }
        let mut directories = vec![];
        for (name, rows) in expected {
            let count = rows.len() as u64;
            let root = tree::edit(&o, &None, rows, &mut ReadStats::default()).unwrap();
            directories.push((
                name,
                Some(serde_json::to_value(Group { root, count }).unwrap()),
            ));
        }
        let expected = tree::edit(&o, &None, directories, &mut ReadStats::default()).unwrap();
        let actual = bulk.build(&o).unwrap();
        assert_eq!(
            actual, expected,
            "membership counts, sorted keys and every reference must match"
        );
        assert!(!std::fs::read_dir(&o.root).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("initial-index-")
        }));
    }
    #[test]
    fn unordered_duplicate_and_corrupt_runs_never_produce_a_root() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::new(temp.path().join("objects")).unwrap();
        for keys in [vec!["b", "a"], vec!["a", "a"]] {
            let result =
                tree::build_sorted(&o, keys.into_iter().map(|k| Ok((k.to_owned(), json!(1)))));
            assert_eq!(result.unwrap_err().code(), "InvalidSortedIndex");
        }
        let mut b = RunBuilder::new(&o.root).unwrap();
        b.push("all:entity", "f0", json!(1)).unwrap();
        b.flush().unwrap();
        std::fs::write(&b.runs[0].1, b"truncated-json\n").unwrap();
        assert!(b.build(&o).is_err());
    }
    #[test]
    fn compressed_runs_keep_exact_live_byte_accounting_through_merges() {
        let temp = tempfile::tempdir().unwrap();
        let mut b = RunBuilder::new(temp.path()).unwrap();
        b.run_bytes = 512;
        for i in 0..140 {
            b.push(
                "all:entity",
                &format!("entity:f{i}"),
                json!({"repeated": "same-reference".repeat(32), "offset": i}),
            )
            .unwrap();
        }
        b.flush().unwrap();
        let actual: u64 = b
            .runs
            .iter()
            .map(|(_, p)| p.metadata().unwrap().len())
            .sum();
        assert_eq!(b.live_bytes, actual);
        assert!(b.runs.iter().any(|(level, _)| *level > 0));
        let count = Merge::open(&b.runs.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>())
            .unwrap()
            .inspect(|r| {
                r.as_ref().unwrap();
            })
            .count();
        assert_eq!(count, 140);
    }
    #[test]
    fn scratch_budget_includes_merge_output_while_inputs_still_exist() {
        let temp = tempfile::tempdir().unwrap();
        let mut b = RunBuilder::new(temp.path()).unwrap();
        for i in 0..2 {
            b.push("all:entity", &format!("entity:f{i}"), json!(i))
                .unwrap();
            b.flush().unwrap();
        }
        let before = b.live_bytes;
        b.disk_bytes = before + 1;
        let error = b.merge_runs(vec![0, 1], 1).unwrap_err();
        assert_eq!(error.code(), "IndexBuildBudget");
        let actual: u64 = std::fs::read_dir(b.dir.path())
            .unwrap()
            .map(|p| p.unwrap().metadata().unwrap().len())
            .sum();
        assert!(actual <= b.disk_bytes);
        assert_eq!(b.runs.len(), 2, "inputs must survive an incomplete merge");
    }
    #[test]
    fn truncated_compressed_run_cannot_return_a_root() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::new(temp.path().join("objects")).unwrap();
        let mut b = RunBuilder::new(&o.root).unwrap();
        b.push("all:entity", "f0", json!(1)).unwrap();
        b.flush().unwrap();
        let p = &b.runs[0].1;
        let size = p.metadata().unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_len(size - 3)
            .unwrap();
        assert!(b.build(&o).is_err());
    }
    #[test]
    #[ignore = "manual scratch-only replay; requires CQ_POSTING_SAMPLE and CQ_POSTING_REPORT"]
    fn retained_real_postings_measure_scratch_without_reindexing() {
        let input = std::env::var("CQ_POSTING_SAMPLE").unwrap();
        let report = std::env::var("CQ_POSTING_REPORT").unwrap();
        let rows: Vec<Value> = serde_json::from_reader(File::open(input).unwrap()).unwrap();
        let mut result = vec![];
        for family in ["entity", "body", "occurrence", "relation", "document"] {
            let temp = tempfile::tempdir().unwrap();
            let mut b = RunBuilder::new(temp.path()).unwrap();
            let mut count = 0;
            let started = std::time::Instant::now();
            for row in &rows {
                if row["family"].as_str().unwrap().to_lowercase() != family {
                    continue;
                }
                let value = &row["value"];
                let id = value["id"].as_str().unwrap();
                let reference = json!({"object":{"hash":cq_core::model::digest(format!("{family}-{}",count/64).as_bytes()),"bytes":4194304},"offset":(count%64)*4096,"length":serde_json::to_vec(value).unwrap().len(),"digest":cq_core::model::digest(&serde_json::to_vec(value).unwrap())});
                for group in cq_core::ports::facts::groups(family, value) {
                    b.push(&group, &format!("{family}:{id}"), reference.clone())
                        .unwrap();
                }
                count += 1;
            }
            b.flush().unwrap();
            let initial_bytes = b.live_bytes;
            let len = b.runs.len();
            b.merge_runs((0..len).collect(), 1).unwrap();
            result.push(json!({"family":family,"sample_records":count,"postings":b.count,"initial_compressed_bytes":initial_bytes,"merged_compressed_bytes":b.live_bytes,"seconds":started.elapsed().as_secs_f64()}));
        }
        std::fs::write(report, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    }
    #[test]
    #[ignore = "retained real posting replay: CQ_POSTING_SAMPLE and CQ_POSTING_REPORT"]
    fn retained_real_postings_build_the_same_serial_and_parallel_roots() {
        let input = std::env::var("CQ_POSTING_SAMPLE").unwrap();
        let report = std::env::var("CQ_POSTING_REPORT").unwrap();
        let rows: Vec<Value> = serde_json::from_reader(File::open(&input).unwrap()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut serial = RunBuilder::new(&o.root).unwrap();
        let mut parallel = Builder::new(&o.root).unwrap();
        let started = std::time::Instant::now();
        let mut postings = 0;
        for rows in rows.chunks(128) {
            let mut chunk = vec![];
            for row in rows {
                let family = row["family"].as_str().unwrap().to_lowercase();
                let value = &row["value"];
                let id = value["id"].as_str().unwrap();
                let reference = serde_json::json!({"digest":cq_core::model::digest(&serde_json::to_vec(value).unwrap()),"offset":0,"length":serde_json::to_vec(value).unwrap().len(),"object":{"hash":cq_core::model::digest(id.as_bytes()),"bytes":4194304}});
                for group in cq_core::ports::facts::groups_indexed(&family, value, true) {
                    chunk.push((group, format!("{family}:{id}"), reference.clone()));
                    postings += 1;
                }
            }
            for (g, k, v) in &chunk {
                serial.push(g, k, v.clone()).unwrap();
            }
            parallel.push_many(chunk).unwrap();
        }
        let feeding = started.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        let expected = serial.build(&o).unwrap();
        let serial_seconds = t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        let actual = parallel.build(&o).unwrap();
        let parallel_seconds = t.elapsed().as_secs_f64();
        assert_eq!(
            actual, expected,
            "all posting memberships, counts and Merkle pages must be identical"
        );
        o.sync_pending().unwrap();
        std::fs::write(report,serde_json::to_vec_pretty(&serde_json::json!({"input":input,"records":rows.len(),"postings":postings,"feeding_both_oracles_seconds":feeding,"serial_build_seconds":serial_seconds,"parallel_build_seconds":parallel_seconds,"workers":cq_core::compute::workers().unwrap(),"exact_root":actual,"passed":true,"scope":"real retained payload fields and actual indexed groups; synthesized page references; no semantic publication"})).unwrap()).unwrap();
    }
    #[test]
    #[ignore = "CQ_RUN_SAMPLE_LIST and CQ_RUN_REPORT: real retained multi-run pipeline"]
    fn retained_runs_validate_bounded_writer_pipeline_against_sync_oracle() {
        let manifest = std::env::var("CQ_RUN_SAMPLE_LIST").unwrap();
        let output = std::env::var("CQ_RUN_REPORT").unwrap();
        let samples: Vec<Value> =
            serde_json::from_reader(BufReader::new(File::open(&manifest).unwrap())).unwrap();
        let paths = samples
            .iter()
            .map(|s| PathBuf::from(s["path"].as_str().unwrap()))
            .collect::<Vec<_>>();
        let mut results = Vec::new();
        let mut root = None;
        for asynchronous in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let o = Objects::buffered(temp.path().join("objects")).unwrap();
            let mut b = Builder::new(&o.root).unwrap();
            for p in &mut b.parts {
                p.asynchronous = asynchronous;
            }
            let started = std::time::Instant::now();
            let mut count = 0;
            let mut batch = Vec::new();
            for row in Merge::open(&paths).unwrap() {
                let row = row.unwrap();
                let directory = row.directory.split_once(':').unwrap().1.to_owned();
                let key = row.key.split_once(':').unwrap().1.to_owned();
                batch.push((directory, key, Arc::unwrap_or_clone(row.value)));
                count += 1;
                if batch.len() == 4096 {
                    b.push_many(std::mem::take(&mut batch)).unwrap();
                }
            }
            b.push_many(batch).unwrap();
            let feed = started.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            let (actual, merge_stats) = b.build_profiled(&o).unwrap();
            o.sync_pending().unwrap();
            let build = t.elapsed().as_secs_f64();
            if asynchronous {
                assert_eq!(
                    actual, root,
                    "every real membership/ref/count and immutable page must match"
                );
            } else {
                root = actual.clone();
            }
            results.push(serde_json::json!({"asynchronous":asynchronous,"postings":count,"feed_seconds":feed,"finish_seconds":build,"total_seconds":feed+build,"root":actual,"merge":merge_stats,"objects":o.resource_stats()}));
        }
        std::fs::write(output,serde_json::to_vec_pretty(&serde_json::json!({"scope":"actual checksummed run files from the incomplete Brocade publication; sorted retained posting replay; separate temporary object stores; input cache may differ; compiler and graph import excluded","samples":paths.len(),"passed":true,"results":results})).unwrap()).unwrap();
    }
    #[test]
    fn scratch_limit_is_explicit_and_invalid_limits_fail_closed() {
        assert_eq!(disk_budget(None).unwrap(), DISK_BYTES);
        assert_eq!(
            disk_budget(Some("68719476736")).unwrap(),
            64 * 1024 * 1024 * 1024
        );
        for value in ["", "0", "-1", "unlimited", "1099511627777"] {
            assert_eq!(
                disk_budget(Some(value)).unwrap_err().code(),
                "InvalidIndexBudget"
            );
        }
    }
}
