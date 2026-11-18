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
pub struct Journal {
    root: PathBuf,
    data: Mutex<Data>,
}
impl Journal {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root,
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
}
impl Store for Journal {
    fn put(&self, header: &SemanticSnapshot, r: Record, replace: bool) -> Result<()> {
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
        let mut d = self.data.lock().unwrap();
        if let Some(old) = d.entries.get(&key) {
            if old.digest == hash {
                return Ok(());
            }
            if !replace {
                return Err(Error::new("ConflictingAnalysisRecord", key));
            }
        }
        if d.bytes + b.len() as u64 > 16 * 1024 * 1024 * 1024 {
            return Err(Error::new(
                "AnalysisJournalBudget",
                "16 GiB record journal limit",
            ));
        }
        let offset = d.file.as_file_mut().seek(SeekFrom::End(0))?;
        d.file.as_file_mut().write_all(&b)?;
        d.sequence += 1;
        d.bytes += b.len() as u64;
        let sequence = d.sequence;
        d.entries.insert(
            key,
            Entry {
                offset,
                bytes: b.len() as u64,
                digest: hash,
                sequence,
            },
        );
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
