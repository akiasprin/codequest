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
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};
const RUN_BYTES: usize = 8 * 1024 * 1024;
const FAN_IN: usize = 32;
const DISK_BYTES: u64 = 32 * 1024 * 1024 * 1024;
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
    total: u64,
    serial: u64,
    runs: Vec<(usize, PathBuf)>,
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
            total: 0,
            serial: 0,
            runs: vec![],
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
        self.total += size as u64;
        if self.total > DISK_BYTES {
            return Err(Error::new(
                "IndexBuildBudget",
                "initial membership sort exceeds 32 GiB; no revision published",
            ));
        }
        self.bytes += size;
        self.rows.push(row);
        Ok(())
    }
    fn path(&mut self) -> PathBuf {
        self.serial += 1;
        self.dir.path().join(format!("{}.jsonl", self.serial))
    }
    fn flush(&mut self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        self.rows
            .sort_unstable_by(|a, b| (&a.directory, &a.key).cmp(&(&b.directory, &b.key)));
        let path = self.path();
        let mut out = BufWriter::new(File::create(&path)?);
        for row in self.rows.drain(..) {
            serde_json::to_writer(&mut out, &row)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
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
        let mut out = BufWriter::new(File::create(&path)?);
        for row in Merge::open(&paths)? {
            serde_json::to_writer(&mut out, &row?)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        drop(out);
        for i in indices.into_iter().rev() {
            self.runs.remove(i);
        }
        for old in paths {
            std::fs::remove_file(old)?;
        }
        self.runs.push((level, path));
        Ok(())
    }
    pub fn build(mut self, o: &Objects) -> Result<tree::Root> {
        self.flush()?;
        while self.runs.len() > FAN_IN {
            self.merge_runs((0..FAN_IN).collect(), 64)?;
        }
        let paths = self.runs.iter().map(|r| r.1.clone()).collect::<Vec<_>>();
        let mut merged = Merge::open(&paths)?.peekable();
        let mut failed = false;
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
    readers: Vec<BufReader<File>>,
    rows: Vec<Option<Row>>,
    heap: BinaryHeap<Reverse<(String, String, usize)>>,
    failed: bool,
}
impl Merge {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let readers = paths
            .iter()
            .map(|p| File::open(p).map(BufReader::new))
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
        let row: Row = serde_json::from_str(&line)?;
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
}
