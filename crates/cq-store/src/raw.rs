use cq_core::{
    Error, Result,
    model::*,
    semantic::records::{Record, Source, Store},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Mutex,
};
#[derive(Clone, Copy)]
struct Entry {
    offset: u64,
    bytes: u64,
    digest: [u8; 32],
}
struct Data {
    file: tempfile::NamedTempFile,
    entries: BTreeMap<String, Entry>,
    sequence: u64,
    bytes: u64,
}
struct Encoded {
    key: String,
    bytes: Vec<u8>,
    digest: [u8; 32],
}
pub struct Journal {
    root: PathBuf,
    progress: Mutex<Option<cq_core::progress::Progress>>,
    data: Mutex<Data>,
}
impl Journal {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root,
            progress: Mutex::new(None),
            data: Mutex::new(Data {
                file: tempfile::NamedTempFile::new()?,
                entries: BTreeMap::new(),
                sequence: 0,
                bytes: 0,
            }),
        })
    }
    fn read(data: &mut Data, e: &Entry) -> Result<Record> {
        let mut b = vec![0; e.bytes as usize];
        crate::disk_batch::read_at(data.file.as_file(), &mut b, e.offset)?;
        if <[u8; 32]>::from(Sha256::digest(&b)) != e.digest {
            return Err(Error::new("DigestMismatch", "analysis record journal"));
        }
        Ok(serde_json::from_slice(&b)?)
    }
    pub fn stats(&self) -> serde_json::Value {
        let d = self.data.lock().unwrap();
        serde_json::json!({"records":d.entries.len(),"journal_bytes":d.bytes,"payload_storage":"disk","project_semantic_snapshot":false})
    }
    fn append(&self, records: Vec<Encoded>, replace: bool) -> Result<()> {
        let mut d = self.data.lock().unwrap();
        let mut bytes = Vec::new();
        let mut entries = BTreeMap::<String, Entry>::new();
        let mut sequence = d.sequence;
        for record in records {
            if let Some(old) = entries
                .get(&record.key)
                .or_else(|| d.entries.get(&record.key))
            {
                if old.digest == record.digest {
                    continue;
                }
                if !replace {
                    return Err(Error::new("ConflictingAnalysisRecord", record.key));
                }
            }
            if d.bytes + bytes.len() as u64 + record.bytes.len() as u64 > 16 * 1024 * 1024 * 1024 {
                return Err(Error::new(
                    "AnalysisJournalBudget",
                    "16 GiB record journal limit",
                ));
            }
            sequence += 1;
            entries.insert(
                record.key,
                Entry {
                    offset: d.bytes + bytes.len() as u64,
                    bytes: record.bytes.len() as u64,
                    digest: record.digest,
                },
            );
            bytes.extend_from_slice(&record.bytes);
        }
        if !bytes.is_empty() {
            d.file.as_file_mut().seek(SeekFrom::End(0))?;
            d.file.as_file_mut().write_all(&bytes)?;
            d.bytes += bytes.len() as u64;
            d.sequence = sequence;
            d.entries.extend(entries);
        }
        Ok(())
    }
}
impl Store for Journal {
    fn task_progress(
        &self,
        project: &str,
        phase: &str,
        label: &str,
        done: u64,
        total: Option<u64>,
        unit: &str,
    ) {
        let mut p = self.progress.lock().unwrap();
        let p = p.get_or_insert_with(|| {
            cq_core::progress::Progress::stage(&self.root, project, phase, label, total, unit)
        });
        p.task(phase, label, done, total, unit);
    }
    fn progress(&self, project: &str, completed: u64, total: u64) {
        self.progress_workers(project, completed, total, 1);
    }
    fn progress_workers(&self, project: &str, completed: u64, total: u64, workers: usize) {
        let mut p = self.progress.lock().unwrap();
        if completed == 0 || p.is_none() {
            *p = Some(cq_core::progress::Progress::stage(
                &self.root,
                project,
                "normalize",
                "还原符号与引用",
                Some(total),
                "文件",
            ));
            if let Some(p) = p.as_mut() {
                p.detail(
                    "normalize",
                    "还原符号与引用",
                    0,
                    Some(total),
                    "文件",
                    workers as u64,
                );
            }
        }
        if let Some(p) = p.as_mut() {
            p.count(completed);
        }
    }
    fn put(&self, header: &SemanticSnapshot, r: Record, replace: bool) -> Result<()> {
        self.put_batch(header, vec![r], replace)
    }
    fn progress_pipeline(
        &self,
        project: &str,
        completed: u64,
        total: u64,
        pipeline: &cq_core::progress::Pipeline,
    ) {
        let mut p = self.progress.lock().unwrap();
        let p = p.get_or_insert_with(|| {
            cq_core::progress::Progress::stage(
                &self.root,
                project,
                "normalize",
                "还原符号与引用",
                Some(total),
                "文件",
            )
        });
        p.pipeline(completed, pipeline);
    }
    fn put_batch(
        &self,
        header: &SemanticSnapshot,
        records: Vec<Record>,
        replace: bool,
    ) -> Result<()> {
        let mut chunk = Vec::new();
        let mut chunk_bytes = 0;
        for r in records {
            let key = format!("{}:{}", r.family(), r.id());
            let value = r.value()?;
            let size = serde_json::to_vec(&value)?.len();
            if size > crate::facts::objects::MAX_OBJECT {
                return Err(crate::facts::limits::report(
                    Some(&self.root),
                    r.family(),
                    r.id(),
                    &value,
                    size,
                    Some(header),
                )?);
            }
            let b = serde_json::to_vec(&r)?;
            let hash = Sha256::digest(&b).into();
            if !chunk.is_empty() && chunk_bytes + b.len() > 1024 * 1024 {
                self.append(std::mem::take(&mut chunk), replace)?;
                chunk_bytes = 0;
            }
            chunk_bytes += b.len();
            chunk.push(Encoded {
                key,
                bytes: b,
                digest: hash,
            });
        }
        self.append(chunk, replace)?;
        Ok(())
    }
}
impl Source for Journal {
    fn phase_progress(
        &self,
        project: &str,
        phase: &str,
        label: &str,
        done: u64,
        total: Option<u64>,
        workers: usize,
    ) {
        let mut progress = self.progress.lock().unwrap();
        let p = progress.get_or_insert_with(|| {
            cq_core::progress::Progress::stage(&self.root, project, phase, label, total, "事实")
        });
        if done == 0 {
            p.detail(phase, label, done, total, "事实", workers as u64);
        } else {
            p.count(done);
        }
    }
    fn visit_batches(
        &self,
        family: Option<&str>,
        visitor: &mut dyn FnMut(Vec<Record>) -> Result<()>,
    ) -> Result<()> {
        let (file, mut entries) = {
            let d = self.data.lock().unwrap();
            let prefix = family.map(|f| format!("{f}:"));
            let entries = if let Some(prefix) = prefix {
                d.entries
                    .range(prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&prefix))
                    .map(|(_, e)| *e)
                    .collect::<Vec<_>>()
            } else {
                d.entries.values().copied().collect()
            };
            (d.file.as_file().try_clone()?, entries)
        };
        // Freeze live locators, not payloads. A callback may append/replace
        // records without changing this visit's generation or file offsets.
        cq_core::compute::sort_by(&mut entries, |a, b| a.offset.cmp(&b.offset))?;
        let mut at = 0;
        while at < entries.len() {
            let first = at;
            let mut bytes = 0;
            while at < entries.len() && at - first < 4096 {
                let length = entries[at].bytes;
                if at > first && bytes + length > crate::disk_batch::BATCH_BYTES {
                    break;
                }
                bytes += length;
                at += 1;
            }
            let batch = &entries[first..at];
            let spans = batch
                .iter()
                .map(|e| crate::disk_batch::Span {
                    offset: e.offset,
                    bytes: e.bytes,
                })
                .collect::<Vec<_>>();
            let rows = crate::disk_batch::decode(&file, &spans, |i, b| {
                if <[u8; 32]>::from(Sha256::digest(b)) != batch[i].digest {
                    return Err(Error::new("DigestMismatch", "analysis record journal"));
                }
                Ok(serde_json::from_slice(b)?)
            })?;
            visitor(rows)?;
        }
        Ok(())
    }
    fn family_len(&self, family: &str) -> Result<Option<u64>> {
        let prefix = format!("{family}:");
        let d = self.data.lock().unwrap();
        Ok(Some(
            d.entries
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
                .count() as u64,
        ))
    }
    fn visit_family(&self, family: &str, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        self.visit_records(Some(family), f)
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>> {
        let mut d = self.data.lock().unwrap();
        let e = d.entries.get(&format!("{family}:{id}")).cloned();
        e.map(|e| Self::read(&mut d, &e)).transpose()
    }
    fn visit(&self, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        self.visit_records(None, f)
    }
}
impl Journal {
    fn visit_records(
        &self,
        family: Option<&str>,
        f: &mut dyn FnMut(Record) -> Result<()>,
    ) -> Result<()> {
        self.visit_batches(family, &mut |rows| {
            for row in rows {
                f(row)?;
            }
            Ok(())
        })
    }
}
