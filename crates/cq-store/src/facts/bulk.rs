//! Initial directory construction sorts bounded runs on disk. Final group trees
//! are built once; the incremental writer retains its ordinary copy-on-write path.
use super::{objects::Objects, tree, writer::Group};
use cq_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};
const RUN_BYTES: usize = 8 * 1024 * 1024;
const FAN_IN: usize = 32;
const DISK_BYTES: u64 = 32 * 1024 * 1024 * 1024;
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
struct CappedFile {
    file: File,
    written: u64,
    limit: u64,
}
impl Write for CappedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(io::Error::other(ScratchBudget));
        }
        let n = self.file.write(bytes)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct RunWriter(BufWriter<lz4_flex::frame::FrameEncoder<CappedFile>>);
impl RunWriter {
    fn new(path: &Path, allowance: u64) -> Result<Self> {
        let file = CappedFile {
            file: File::create(path)?,
            written: 0,
            limit: allowance,
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
    key: String,
    value: Value,
}
pub(super) struct Builder {
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
}
impl Builder {
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
        })
    }
    pub fn push(&mut self, directory: &str, key: &str, value: Value) -> Result<()> {
        let row = Row {
            directory: tree::order(directory),
            key: tree::order(key),
            value,
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
        if self.rows.is_empty() {
            return Ok(());
        }
        self.rows
            .sort_unstable_by(|a, b| (&a.directory, &a.key).cmp(&(&b.directory, &b.key)));
        let path = self.path();
        let mut out = RunWriter::new(&path, self.disk_bytes.saturating_sub(self.live_bytes))?;
        for row in self.rows.drain(..) {
            out.push(&row)?;
        }
        self.live_bytes += out.finish()?;
        self.bytes = 0;
        self.runs.push((0, path));
        // Leveled merging bounds open files and avoids repeatedly rewriting a
        // growing master run for every new chunk.
        for level in 0..64 {
            let selected = self
                .runs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.0 == level)
                .map(|(i, _)| i)
                .take(FAN_IN)
                .collect::<Vec<_>>();
            if selected.len() < FAN_IN {
                break;
            }
            self.merge_runs(selected, level + 1)?;
        }
        Ok(())
    }
    fn merge_runs(&mut self, indices: Vec<usize>, level: usize) -> Result<()> {
        let paths = indices
            .iter()
            .map(|i| self.runs[*i].1.clone())
            .collect::<Vec<_>>();
        let path = self.path();
        let mut out = RunWriter::new(&path, self.disk_bytes.saturating_sub(self.live_bytes))?;
        for row in Merge::open(&paths)? {
            out.push(&row?)?;
        }
        self.live_bytes += out.finish()?;
        for i in indices.into_iter().rev() {
            self.runs.remove(i);
        }
        for old in paths {
            let bytes = std::fs::metadata(&old)?.len();
            std::fs::remove_file(old)?;
            self.live_bytes -= bytes;
        }
        self.runs.push((level, path));
        Ok(())
    }
    pub fn build(mut self, o: &Objects) -> Result<tree::Root> {
        let mut progress = super::limits::root(&o.root)
            .zip(self.project.as_deref())
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
        while self.runs.len() > FAN_IN {
            self.merge_runs((0..FAN_IN).collect(), 64)?;
        }
        let paths = self.runs.iter().map(|r| r.1.clone()).collect::<Vec<_>>();
        let mut merged = Merge::open(&paths)?.peekable();
        let mut failed = false;
        let mut completed = 0;
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
            let rows = std::iter::from_fn(|| match merged.peek() {
                Some(Ok(row)) if row.directory == directory => {
                    completed += 1;
                    if let Some(p) = &mut progress {
                        p.count(completed);
                    }
                    Some(merged.next().unwrap().map(|r| (r.key, r.value)))
                }
                Some(Err(_)) => Some(merged.next().unwrap().map(|r| (r.key, r.value))),
                _ => None,
            });
            let result = tree::build_sorted(o, rows).and_then(|(root, count)| {
                Ok((directory, serde_json::to_value(Group { root, count })?))
            });
            if result.is_err() {
                failed = true;
            }
            Some(result)
        });
        let (root, _) = tree::build_sorted(o, directories)?;
        Ok(root)
    }
}
struct Merge {
    readers: Vec<BufReader<lz4_flex::frame::FrameDecoder<File>>>,
    rows: Vec<Option<Row>>,
    heap: BinaryHeap<Reverse<(String, String, usize)>>,
    failed: bool,
}
impl Merge {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let readers = paths
            .iter()
            .map(|p| File::open(p).map(|f| BufReader::new(lz4_flex::frame::FrameDecoder::new(f))))
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut s = Self {
            rows: (0..readers.len()).map(|_| None).collect(),
            readers,
            heap: BinaryHeap::new(),
            failed: false,
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
            key: tree::order(&key),
            value,
        };
        self.heap
            .push(Reverse((row.directory.clone(), row.key.clone(), i)));
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
    use cq_core::ports::facts::ReadStats;
    use serde_json::json;
    use std::collections::BTreeMap;
    #[test]
    fn external_runs_produce_the_same_complete_merkle_roots_as_the_existing_map() {
        let temp = tempfile::tempdir().unwrap();
        let o = Objects::new(temp.path().join("objects")).unwrap();
        let mut bulk = Builder::new(&o.root).unwrap();
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
        let mut b = Builder::new(&o.root).unwrap();
        b.push("all:entity", "f0", json!(1)).unwrap();
        b.flush().unwrap();
        std::fs::write(&b.runs[0].1, b"truncated-json\n").unwrap();
        assert!(b.build(&o).is_err());
    }
    #[test]
    fn compressed_runs_keep_exact_live_byte_accounting_through_merges() {
        let temp = tempfile::tempdir().unwrap();
        let mut b = Builder::new(temp.path()).unwrap();
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
        let mut b = Builder::new(temp.path()).unwrap();
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
        let mut b = Builder::new(&o.root).unwrap();
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
            let mut b = Builder::new(temp.path()).unwrap();
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
