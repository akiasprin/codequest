use cq_core::{
    Error, Result,
    model::*,
    semantic::records::{Record, Source, Store},
};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Mutex,
};
#[derive(Clone)]
struct Entry {
    offset: u64,
    bytes: u64,
    digest: String,
    sequence: u64,
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
    digest: String,
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
        data.file.as_file_mut().seek(SeekFrom::Start(e.offset))?;
        let mut b = vec![0; e.bytes as usize];
        data.file.as_file_mut().read_exact(&mut b)?;
        if digest(&b) != e.digest {
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
                    sequence,
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
            let hash = digest(&b);
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
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>> {
        let mut d = self.data.lock().unwrap();
        let e = d.entries.get(&format!("{family}:{id}")).cloned();
        e.map(|e| Self::read(&mut d, &e)).transpose()
    }
    fn visit(&self, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        use std::ops::Bound::{Excluded, Unbounded};
        let generation = self.data.lock().unwrap().sequence;
        let mut after = None::<String>;
        loop {
            let batch = {
                let d = self.data.lock().unwrap();
                d.entries
                    .range::<String, _>((after.as_ref().map_or(Unbounded, Excluded), Unbounded))
                    .take(128)
                    .map(|(k, e)| (k.clone(), e.clone()))
                    .collect::<Vec<_>>()
            };
            if batch.is_empty() {
                break;
            }
            for (k, e) in batch {
                after = Some(k);
                if e.sequence > generation {
                    continue;
                }
                let r = {
                    let mut d = self.data.lock().unwrap();
                    Self::read(&mut d, &e)?
                };
                f(r)?;
            }
        }
        Ok(())
    }
}
