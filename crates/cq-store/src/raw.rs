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
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Entry {
    offset: u64,
    bytes: u64,
    digest: [u8; 32],
}
struct Data {
    file: tempfile::NamedTempFile,
    index: tempfile::NamedTempFile,
    payload_hash: Sha256,
    index_hash: Sha256,
    segments: Vec<Segment>,
    entries: BTreeMap<String, Entry>,
    sequence: u64,
    bytes: u64,
}
struct Encoded {
    key: String,
    bytes: Vec<u8>,
    digest: [u8; 32],
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Image {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SegmentImage {
    pub start: u64,
    pub payload: Image,
    pub index: Image,
}
struct Segment {
    start: u64,
    file: std::fs::File,
    image: SegmentImage,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub segments: Vec<SegmentImage>,
    pub bytes: u64,
    pub records: u64,
    pub sequence: u64,
}
impl Data {
    fn current_start(&self) -> u64 {
        self.segments
            .last()
            .map_or(0, |s| s.start + s.image.payload.bytes)
    }
    fn file_at(&self, offset: u64) -> Result<(&std::fs::File, u64)> {
        if offset >= self.current_start() {
            return Ok((self.file.as_file(), self.current_start()));
        }
        let i = self
            .segments
            .partition_point(|s| s.start <= offset)
            .saturating_sub(1);
        let segment = &self.segments[i];
        Ok((&segment.file, segment.start))
    }
}
fn hex(hash: Sha256) -> String {
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
fn retain(
    file: &tempfile::NamedTempFile,
    directory: &std::path::Path,
    hash: String,
) -> Result<Image> {
    file.as_file().sync_all()?;
    let path = directory.join(&hash);
    if !path.exists() {
        match std::fs::hard_link(file.path(), &path) {
            Ok(()) => (),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                let mut tmp = tempfile::NamedTempFile::new_in(directory)?;
                let mut input = file.as_file().try_clone()?;
                input.seek(SeekFrom::Start(0))?;
                std::io::copy(&mut input, &mut tmp)?;
                tmp.as_file().sync_all()?;
                tmp.persist_noclobber(&path).map_err(|e| e.error)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Image {
        path,
        bytes: file.as_file().metadata()?.len(),
        sha256: hash,
    })
}
fn verify_image(image: &Image, directory: &std::path::Path) -> Result<std::fs::File> {
    let path = image.path.canonicalize()?;
    if !path.starts_with(directory.canonicalize()?) || !path.is_file() {
        return Err(Error::new(
            "InvalidAnalysisCheckpoint",
            "journal artifact outside execution",
        ));
    }
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let n = std::io::copy(&mut file, &mut hash)?;
    if n != image.bytes || hex(hash) != image.sha256 {
        return Err(Error::new("DigestMismatch", "analysis checkpoint segment"));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}
pub struct Journal {
    root: PathBuf,
    progress: Mutex<Option<cq_core::progress::Progress>>,
    data: Mutex<Data>,
    execution: Mutex<Option<std::sync::Weak<crate::execution::Execution>>>,
}
impl Journal {
    pub fn attach_execution(&self, execution: &std::sync::Arc<crate::execution::Execution>) {
        *self.execution.lock().unwrap() = Some(std::sync::Arc::downgrade(execution));
    }
    /// Seal only the current append-only segment. Earlier stage snapshots keep
    /// referring to immutable files; no project-sized payload copy is needed.
    pub fn checkpoint(&self, directory: &std::path::Path) -> Result<Checkpoint> {
        crate::atomic::create_directory(directory)?;
        let mut d = self.data.lock().unwrap();
        if d.file.as_file().metadata()?.len() > 0 {
            let image = SegmentImage {
                start: d.current_start(),
                payload: retain(&d.file, directory, hex(d.payload_hash.clone()))?,
                index: retain(&d.index, directory, hex(d.index_hash.clone()))?,
            };
            let file = std::fs::File::open(&image.payload.path)?;
            d.segments.push(Segment {
                start: image.start,
                file,
                image,
            });
            d.file = tempfile::NamedTempFile::new_in(directory)?;
            d.index = tempfile::NamedTempFile::new_in(directory)?;
            d.payload_hash = Sha256::new();
            d.index_hash = Sha256::new();
        }
        std::fs::File::open(directory)?.sync_all()?;
        Ok(Checkpoint {
            version: 1,
            segments: d.segments.iter().map(|s| s.image.clone()).collect(),
            bytes: d.bytes,
            records: d.entries.len() as u64,
            sequence: d.sequence,
        })
    }
    /// Discard the unsealed tail, validate artifacts, and reconstruct only the
    /// compact locator map. Payloads stay in their original immutable segments.
    pub fn restore(&self, point: &Checkpoint, directory: &std::path::Path) -> Result<()> {
        self.restore_progress(point, directory, None)
    }
    pub(crate) fn restore_progress(
        &self,
        point: &Checkpoint,
        directory: &std::path::Path,
        project: Option<&str>,
    ) -> Result<()> {
        use std::io::{BufRead, BufReader};
        if point.version != 1 {
            return Err(Error::new("InvalidAnalysisCheckpoint", "journal version"));
        }
        let mut progress = project.map(|project| {
            cq_core::progress::Progress::stage(
                &self.root,
                project,
                "resume.records",
                "恢复事实存档",
                Some(point.records),
                "事实",
            )
        });
        let mut indexed = 0u64;
        let mut entries = BTreeMap::new();
        let mut segments = vec![];
        let mut offset = 0;
        for image in &point.segments {
            if image.start != offset {
                return Err(Error::new("InvalidAnalysisCheckpoint", "segment range"));
            }
            let file = verify_image(&image.payload, directory)?;
            let index = verify_image(&image.index, directory)?;
            for row in BufReader::new(index).lines() {
                let (key, entry): (String, Entry) = serde_json::from_str(&row?)?;
                if entry.offset < image.start
                    || entry
                        .offset
                        .checked_add(entry.bytes)
                        .is_none_or(|end| end > image.start + image.payload.bytes)
                {
                    return Err(Error::new("InvalidAnalysisCheckpoint", "locator range"));
                }
                entries.insert(key, entry);
                indexed += 1;
                if indexed.is_multiple_of(4096)
                    && let Some(p) = &mut progress
                {
                    p.count(entries.len() as u64);
                }
            }
            offset += image.payload.bytes;
            segments.push(Segment {
                start: image.start,
                file,
                image: image.clone(),
            });
        }
        if offset != point.bytes || entries.len() as u64 != point.records {
            return Err(Error::new("InvalidAnalysisCheckpoint", "journal counts"));
        }
        if let Some(p) = &mut progress {
            p.count(point.records);
        }
        *self.data.lock().unwrap() = Data {
            file: tempfile::NamedTempFile::new_in(directory)?,
            index: tempfile::NamedTempFile::new_in(directory)?,
            payload_hash: Sha256::new(),
            index_hash: Sha256::new(),
            segments,
            entries,
            sequence: point.sequence,
            bytes: point.bytes,
        };
        Ok(())
    }
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root,
            execution: Mutex::new(None),
            progress: Mutex::new(None),
            data: Mutex::new(Data {
                file: tempfile::NamedTempFile::new()?,
                index: tempfile::NamedTempFile::new()?,
                payload_hash: Sha256::new(),
                index_hash: Sha256::new(),
                segments: vec![],
                entries: BTreeMap::new(),
                sequence: 0,
                bytes: 0,
            }),
        })
    }
    fn read(data: &mut Data, e: &Entry) -> Result<Record> {
        let mut b = vec![0; e.bytes as usize];
        let (file, base) = data.file_at(e.offset)?;
        crate::disk_batch::read_at(file, &mut b, e.offset - base)?;
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
            d.payload_hash.update(&bytes);
            for (key, entry) in &entries {
                let row = serde_json::to_vec(&(key, entry))?;
                d.index.as_file_mut().write_all(&row)?;
                d.index.as_file_mut().write_all(b"\n")?;
                d.index_hash.update(&row);
                d.index_hash.update(b"\n");
            }
            d.bytes += bytes.len() as u64;
            d.sequence = sequence;
            d.entries.extend(entries);
        }
        Ok(())
    }
}
impl Store for Journal {
    fn is_cancelled(&self) -> bool {
        self.execution
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|e| e.upgrade())
            .is_some_and(|e| e.control().is_cancelled())
    }

    fn normalization_cursor(&self) -> Result<serde_json::Value> {
        Ok(self
            .execution
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|e| e.upgrade())
            .map(|e| e.cursor(cq_core::execution::Stage::Normalize))
            .unwrap_or(serde_json::Value::Null))
    }
    fn checkpoint_normalization(
        &self,
        header: &SemanticSnapshot,
        done: u64,
        total: u64,
        peak: u64,
    ) -> Result<()> {
        if let Some(e) = self
            .execution
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|e| e.upgrade())
        {
            e.checkpoint(
                cq_core::execution::Stage::Normalize,
                header,
                Some(self),
                serde_json::json!({"frames":done,"peak":peak}),
                done,
                total,
                false,
            )?;
        }
        Ok(())
    }

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
        if Store::is_cancelled(self) {
            return Err(Error::new("Cancelled", "record write paused"));
        }
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
        let (files, mut entries) = {
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
            let mut files = d
                .segments
                .iter()
                .map(|s| Ok((s.start, s.file.try_clone()?)))
                .collect::<Result<Vec<_>>>()?;
            files.push((d.current_start(), d.file.as_file().try_clone()?));
            (files, entries)
        };
        // Freeze live locators, not payloads. A callback may append/replace
        // records without changing this visit's generation or file offsets.
        cq_core::compute::sort_by(&mut entries, |a, b| a.offset.cmp(&b.offset))?;
        let mut at = 0;
        while at < entries.len() {
            if Store::is_cancelled(self) {
                return Err(Error::new("Cancelled", "record scan paused"));
            }
            let first = at;
            let file_index = files
                .partition_point(|(start, _)| *start <= entries[at].offset)
                .saturating_sub(1);
            let (base, file) = &files[file_index];
            let end = files
                .get(file_index + 1)
                .map(|(start, _)| *start)
                .unwrap_or(u64::MAX);
            let mut bytes = 0;
            while at < entries.len() && at - first < 4096 && entries[at].offset < end {
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
                    offset: e.offset - base,
                    bytes: e.bytes,
                })
                .collect::<Vec<_>>();
            let rows = crate::disk_batch::decode(file, &spans, |i, b| {
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
