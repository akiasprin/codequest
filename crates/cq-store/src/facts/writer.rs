use super::{
    objects::Objects,
    source::Source,
    tree::{self, Root},
};
use cq_core::semantic::production::{self, Sink};
use cq_core::semantic::{
    scope::{Completion, Output, ScopeKey},
    transfer::{self, Base, Sink as TransferSink},
};
use cq_core::{
    Error, Result,
    model::*,
    ports::facts::{FactRef, ReadStats, fact_key},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Roots {
    pub facts: Root,
    pub groups: Root,
    pub sources: Root,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Group {
    pub root: Root,
    pub count: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WriteStats {
    pub inventory_bytes: u64,
    pub facts_changed: u64,
    pub facts_reused: u64,
    pub facts_deleted: u64,
    pub fact_pages_prepared: u64,
    pub fact_bytes_prepared: u64,
    pub identity_unmatched: Option<u64>,
    pub upserts: Root,
    pub deletes: Root,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Revision {
    pub format: u32,
    pub project: String,
    pub revision: String,
    pub parent: Option<String>,
    pub sequence: u64,
    pub roots: Roots,
    pub metadata: Value,
    pub changes: WriteStats,
    pub seal: String,
}
impl Revision {
    pub fn seal(&mut self) -> Result<()> {
        self.seal.clear();
        self.seal = digest(&serde_json::to_vec(self)?);
        Ok(())
    }
    pub fn verify(&self) -> Result<()> {
        let mut r = self.clone();
        r.seal()?;
        if self.format != 2 || self.seal != r.seal {
            return Err(Error::new("CorruptRevision", "format 2 sealed manifest"));
        }
        Ok(())
    }
}
pub fn reference(
    o: &Objects,
    roots: &Roots,
    family: &str,
    id: &str,
    s: &mut ReadStats,
) -> Result<Option<FactRef>> {
    tree::get(o, &roots.facts, &fact_key(family, id), s)?
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}
pub fn decode(o: &Objects, r: &FactRef, s: &mut ReadStats) -> Result<Value> {
    let b = o.read(&r.object, s)?;
    decode_bytes(&b, r)
}
pub(crate) fn decode_bytes(b: &[u8], r: &FactRef) -> Result<Value> {
    Ok(serde_json::from_slice(record_bytes(b, r)?)?)
}
pub(crate) fn record_bytes<'a>(b: &'a [u8], r: &FactRef) -> Result<&'a [u8]> {
    if b.len() as u64 != r.object.bytes {
        return Err(Error::new("DigestMismatch", "object length"));
    }
    let end = r
        .offset
        .checked_add(r.length)
        .ok_or_else(|| Error::new("InvalidFactRef", "offset overflow"))?;
    if end > r.object.bytes {
        return Err(Error::new("InvalidFactRef", "page bounds"));
    }
    let row = b
        .get(r.offset as usize..end as usize)
        .ok_or_else(|| Error::new("InvalidFactRef", "page bounds"))?;
    if digest(row) != r.digest {
        return Err(Error::new("DigestMismatch", "fact record"));
    }
    Ok(row)
}
struct PreparedRow {
    family: String,
    id: String,
    value: Value,
    bytes: Vec<u8>,
    digest: String,
    groups: Vec<String>,
}
impl PreparedRow {
    fn new(family: &str, id: &str, value: Value, indexed: bool) -> Result<Self> {
        let bytes = serde_json::to_vec(&value)?;
        let groups = if bytes.len() <= super::objects::MAX_OBJECT {
            cq_core::ports::facts::groups_indexed(family, &value, indexed)
        } else {
            Vec::new()
        };
        let hash = digest(&bytes);
        Ok(Self {
            family: family.into(),
            id: id.into(),
            value,
            bytes,
            digest: hash,
            groups,
        })
    }
}
type IndexedRow = (String, String, Value, FactRef, Vec<String>);
enum EncodingInput {
    Record(production::Record),
    BeginScope,
    EndScope(Box<transfer::Begin>, u64),
}
enum Encoded {
    Rows(Vec<PreparedRow>),
    BeginScope,
    EndScope(Box<transfer::Begin>, u64),
}
type ClosingScope = (Box<transfer::Begin>, BTreeMap<String, FactRef>, u64);
#[derive(Default)]
struct ScopeJobs(VecDeque<cq_core::compute::Job<PreparedRow>>);
impl Drop for ScopeJobs {
    fn drop(&mut self) {
        while let Some(job) = self.0.pop_front() {
            let _ = job.join();
        }
    }
}
type GroupChanges = BTreeMap<String, BTreeMap<String, Option<Value>>>;
fn memberships(
    o: &Objects,
    roots: &mut Roots,
    changes: GroupChanges,
    s: &mut ReadStats,
) -> Result<()> {
    let mut directories = vec![];
    for (name, changes) in changes {
        let mut group: Group = tree::get(o, &roots.groups, &name, s)?
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        for (key, value) in &changes {
            let exists = tree::get(o, &group.root, key, s)?.is_some();
            if value.is_some() && !exists {
                group.count += 1;
            } else if value.is_none() && exists {
                group.count -= 1;
            }
        }
        group.root = tree::edit(o, &group.root, changes.into_iter().collect(), s)?;
        directories.push((
            name,
            if group.count == 0 {
                None
            } else {
                Some(serde_json::to_value(group)?)
            },
        ));
    }
    roots.groups = tree::edit(o, &roots.groups, directories, s)?;
    Ok(())
}
/// Incremental sink: pages are sealed before references enter the manifest.
/// An unchanged canonical record retains its exact page and offset.
pub struct Writer<'a> {
    o: &'a Objects,
    control: Option<cq_core::control::CaptureControl>,
    pub roots: Roots,
    pub stats: WriteStats,
    seen: BTreeSet<String>,
    pending: Vec<PreparedRow>,
    pending_bytes: usize,
    page_bytes: usize,
    page_rows: usize,
    read: ReadStats,
    audited: BTreeSet<String>,
    diagnostic_header: Option<SemanticSnapshot>,
    preserve_terms: bool,
    vector_model: Option<std::sync::Arc<cq_core::query::embeddings::TokenVectors>>,
    vector_enabled: bool,
    indexed: bool,
    initial_groups: Option<super::bulk::Builder>,
    initial_facts: Option<super::bulk::Builder>,
    scope_facts: Option<BTreeMap<String, FactRef>>,
    progress: Option<cq_core::progress::Progress>,
    completed: u64,
    encoding_queue: Vec<EncodingInput>,
    encoding_batches: usize,
    encoding_jobs: VecDeque<cq_core::compute::Job<Vec<Encoded>>>,
    encoding_maximum_pending: usize,
    encoding_submitted: u64,
    encoding_pending_limit: usize,
    closing_scope: Option<ClosingScope>,
    scope_jobs: ScopeJobs,
    scope_jobs_submitted: u64,
}
struct NativeSink<'a, 'b> {
    writer: &'a mut Writer<'b>,
    validator: production::Validator,
}
struct ScopedSink<'a, 'b> {
    writer: &'a mut Writer<'b>,
    base: &'a crate::analysis::ScopeBase,
    validator: transfer::Validator<'a>,
    current: Option<(transfer::Begin, Root)>,
    compact: bool,
    aggregate: Option<super::statistics::Incremental>,
    spool: &'a crate::analysis::Spool,
    stats: Value,
}
impl ScopedSink<'_, '_> {
    fn statistics_target(&self, family: &str, row: &Value, old: bool) -> Result<Option<String>> {
        if self.aggregate.is_none() || family != "relation" {
            return Ok(None);
        }
        let Some(id) = row["to"].as_str() else {
            return Ok(None);
        };
        let value = if old {
            let Some(base) = self.base.manifest() else {
                return Ok(None);
            };
            reference(
                self.writer.o,
                &base.roots,
                "entity",
                id,
                &mut ReadStats::default(),
            )?
            .map(|f| decode(self.writer.o, &f, &mut ReadStats::default()))
            .transpose()?
        } else {
            let key = format!("entity:{id}");
            self.spool
                .contains(&key)
                .then(|| self.spool.read(&key))
                .transpose()?
        };
        Ok(value.and_then(|v| v["unit"].as_str().map(str::to_owned)))
    }
}
impl TransferSink for ScopedSink<'_, '_> {
    fn begin(&mut self, b: &transfer::Begin) -> Result<()> {
        self.validator.begin(b)?;
        if self.writer.initial_facts.is_some()
            && self.aggregate.is_none()
            && self.base.manifest().is_none()
            && b.descriptor.completion == Completion::Complete
            && matches!(b.receipt.output, Output::Full(_))
        {
            self.writer.encoding_queue.push(EncodingInput::BeginScope);
            self.current = Some((b.clone(), None));
            self.compact = false;
            self.stats["full_scopes"] = json!(self.stats["full_scopes"].as_u64().unwrap_or(0) + 1);
            return Ok(());
        }
        if self.writer.initial_facts.is_some() {
            self.writer.flush()?;
            self.writer.scope_facts = Some(BTreeMap::new());
        }
        self.compact = self
            .base
            .summary(&b.descriptor.key)?
            .is_some_and(|old| transfer::compact_match(b, &old));
        if self.compact {
            let root = self
                .base
                .stored(&b.descriptor.key)?
                .map(|v| serde_json::from_value(v["members"].clone()))
                .transpose()?
                .flatten();
            self.current = Some((b.clone(), root));
            let mode = if matches!(b.receipt.output, Output::Reuse { .. }) {
                "reused_scopes"
            } else {
                "delta_scopes"
            };
            self.stats[mode] = json!(self.stats[mode].as_u64().unwrap_or(0) + 1);
            return Ok(());
        }
        let old = self
            .base
            .scope(&b.descriptor.key)?
            .map(|s| s.records)
            .unwrap_or_default();
        let keep = match &b.receipt.output {
            Output::Full(r) => r.keys().cloned().collect::<BTreeSet<_>>(),
            Output::Delta { deletes, .. } => old
                .keys()
                .filter(|k| !deletes.contains(*k))
                .cloned()
                .collect(),
            Output::Reuse { .. } => old.keys().cloned().collect(),
        };
        let remove = old
            .keys()
            .filter(|key| b.descriptor.completion != Completion::Complete || !keep.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        if self.aggregate.is_some() {
            for key in &remove {
                let (family, id) = key.split_once(':').unwrap();
                if let Some(f) = reference(
                    self.writer.o,
                    &self.writer.roots,
                    family,
                    id,
                    &mut self.writer.read,
                )? {
                    let old = decode(self.writer.o, &f, &mut self.writer.read)?;
                    let target = self.statistics_target(family, &old, true)?;
                    if let Some(stats) = self.aggregate.as_mut() {
                        stats.change(
                            family,
                            &old,
                            &b.descriptor.key.unit,
                            target.as_deref(),
                            -1,
                        )?;
                    }
                }
            }
        }
        self.writer.delete_many(&remove)?;
        let root = self
            .base
            .stored(&b.descriptor.key)?
            .and_then(|v| v.get("members").cloned())
            .map(serde_json::from_value)
            .transpose()?
            .flatten();
        self.current = Some((b.clone(), root));
        let mode = match &b.receipt.output {
            Output::Full(_) => "full_scopes",
            Output::Delta { .. } => "delta_scopes",
            Output::Reuse { .. } => "reused_scopes",
        };
        self.stats[mode] = json!(self.stats[mode].as_u64().unwrap_or(0) + 1);
        Ok(())
    }
    fn batch(&mut self, key: &ScopeKey, n: u64, records: &[production::Record]) -> Result<()> {
        #[cfg(test)]
        let validation_at = std::time::Instant::now();
        self.validator.batch(key, n, records)?;
        #[cfg(test)]
        {
            let seconds = validation_at.elapsed().as_secs_f64();
            self.stats["batch_validation_seconds"] = json!(
                self.stats["batch_validation_seconds"]
                    .as_f64()
                    .unwrap_or(0.)
                    + seconds
            );
        }
        for r in records {
            let (family, id) = r
                .key
                .split_once(':')
                .ok_or_else(|| Error::new("InvalidFactKey", &r.key))?;
            if self.aggregate.is_some() {
                if let Some(f) = reference(
                    self.writer.o,
                    &self.writer.roots,
                    family,
                    id,
                    &mut self.writer.read,
                )? {
                    let old = decode(self.writer.o, &f, &mut self.writer.read)?;
                    let target = self.statistics_target(family, &old, true)?;
                    if let Some(stats) = self.aggregate.as_mut() {
                        stats.change(family, &old, &key.unit, target.as_deref(), -1)?;
                    }
                }
                let target = self.statistics_target(family, &r.value, false)?;
                if let Some(stats) = self.aggregate.as_mut() {
                    stats.change(family, &r.value, &key.unit, target.as_deref(), 1)?;
                }
            }
            if self.aggregate.is_some() {
                self.writer.append(family, id, &r.value)?;
            }
        }
        if self.aggregate.is_none() {
            self.writer.append_batch(records)?;
        }
        self.stats["payload_rows_submitted"] = json!(
            self.stats["payload_rows_submitted"].as_u64().unwrap_or(0) + records.len() as u64
        );
        Ok(())
    }
    fn batch_owned(
        &mut self,
        key: &ScopeKey,
        n: u64,
        records: Vec<production::Record>,
    ) -> Result<()> {
        if self.aggregate.is_some() {
            return self.batch(key, n, &records);
        }
        #[cfg(test)]
        let validation_at = std::time::Instant::now();
        self.validator.batch(key, n, &records)?;
        #[cfg(test)]
        {
            let seconds = validation_at.elapsed().as_secs_f64();
            self.stats["batch_validation_seconds"] = json!(
                self.stats["batch_validation_seconds"]
                    .as_f64()
                    .unwrap_or(0.)
                    + seconds
            );
        }
        let count = records.len() as u64;
        self.writer.append_owned_batch(records)?;
        self.stats["payload_rows_submitted"] =
            json!(self.stats["payload_rows_submitted"].as_u64().unwrap_or(0) + count);
        Ok(())
    }
    fn batch_wave(
        &mut self,
        key: &ScopeKey,
        n: u64,
        batches: Vec<Vec<production::Record>>,
    ) -> Result<()> {
        if self.aggregate.is_some() {
            for (i, batch) in batches.into_iter().enumerate() {
                self.batch_owned(key, n + i as u64, batch)?;
            }
            return Ok(());
        }
        #[cfg(test)]
        let at = std::time::Instant::now();
        self.validator.validate_wave(key, n, &batches)?;
        #[cfg(test)]
        {
            self.stats["batch_validation_seconds"] = json!(
                self.stats["batch_validation_seconds"]
                    .as_f64()
                    .unwrap_or(0.)
                    + at.elapsed().as_secs_f64()
            );
        }
        let count = batches.iter().map(Vec::len).sum::<usize>() as u64;
        for batch in batches {
            self.writer.append_owned_batch(batch)?;
        }
        self.stats["payload_rows_submitted"] =
            json!(self.stats["payload_rows_submitted"].as_u64().unwrap_or(0) + count);
        Ok(())
    }
    fn end(&mut self, key: &ScopeKey, n: u64) -> Result<()> {
        #[cfg(test)]
        let closing_at = std::time::Instant::now();
        self.validator.end(key, n)?;
        #[cfg(test)]
        {
            let seconds = closing_at.elapsed().as_secs_f64();
            self.stats["close_validation_seconds"] = json!(
                self.stats["close_validation_seconds"]
                    .as_f64()
                    .unwrap_or(0.)
                    + seconds
            );
        }
        #[cfg(test)]
        let flush_at = std::time::Instant::now();
        if self.writer.initial_facts.is_some()
            && self.aggregate.is_none()
            && self.base.manifest().is_none()
            && self.current.as_ref().is_some_and(|(b, _)| {
                b.descriptor.completion == Completion::Complete
                    && matches!(b.receipt.output, Output::Full(_))
            })
        {
            let (b, _) = self.current.take().unwrap();
            self.writer
                .encoding_queue
                .push(EncodingInput::EndScope(Box::new(b), n));
            if self.writer.encoding_queue.len() >= 1024
                || self
                    .writer
                    .encoding_queue
                    .iter()
                    .filter(|r| matches!(r, EncodingInput::EndScope(..)))
                    .count()
                    >= 16
            {
                self.writer.schedule_encoding()?;
                self.writer.collect_ready_encoding()?;
            }
            return Ok(());
        }
        self.writer.flush()?;
        #[cfg(test)]
        {
            let seconds = flush_at.elapsed().as_secs_f64();
            self.stats["scope_flush_seconds"] =
                json!(self.stats["scope_flush_seconds"].as_f64().unwrap_or(0.) + seconds);
        }
        let staged = self.writer.scope_facts.take();
        let (b, mut root) = self
            .current
            .take()
            .ok_or_else(|| Error::new("MissingScopeBegin", "end without begin"))?;
        if self.compact {
            let state_base = self.base.summary(key)?.unwrap().base_root;
            self.writer.stats.facts_reused += b.descriptor.rows;
            self.writer.completed += b.descriptor.rows;
            if let Some(p) = &mut self.writer.progress {
                p.count(self.writer.completed);
            }
            return self.writer.append("scope",&b.descriptor.id()?,&json!({"descriptor":b.descriptor,"mode":if matches!(b.receipt.output,Output::Reuse{..}){"Reuse"}else{"Delta"},"base_revision":b.base_revision,"receipt_digest":b.receipt.digest,"members":root,"state_base":state_base,"batches":n}));
        }
        let old = self.base.scope(key)?.map(|s| s.records).unwrap_or_default();
        let mut next = match &b.receipt.output {
            Output::Full(r) => r.clone(),
            Output::Delta {
                upserts, deletes, ..
            } => {
                let mut r = old.clone();
                for k in deletes {
                    r.remove(k);
                }
                r.extend(upserts.clone());
                r
            }
            Output::Reuse { .. } => old.clone(),
        };
        if b.descriptor.completion != Completion::Complete {
            next.clear();
        }
        if !matches!(b.receipt.output, Output::Full(_)) {
            self.writer.stats.facts_reused +=
                next.iter().filter(|(k, r)| old.get(*k) == Some(*r)).count() as u64;
        }
        let mut edits = vec![];
        let full = matches!(b.receipt.output, Output::Full(_));
        if full {
            root = None;
        } else {
            for k in old.keys().filter(|k| !next.contains_key(*k)) {
                edits.push((k.clone(), None));
            }
        }
        for (k, r) in &next {
            if !full && old.get(k) == Some(r) {
                continue;
            }
            let (family, id) = k
                .split_once(':')
                .ok_or_else(|| Error::new("InvalidFactKey", k))?;
            let f = if let Some(staged) = &staged {
                staged.get(k).cloned()
            } else {
                reference(
                    self.writer.o,
                    &self.writer.roots,
                    family,
                    id,
                    &mut self.writer.read,
                )?
            }
            .ok_or_else(|| Error::new("MissingTransferredFact", k))?;
            if f.digest != r.hash || f.length != r.bytes {
                return Err(Error::new(
                    "ScopeResultMismatch",
                    "stored payload differs from transfer",
                ));
            }
            edits.push((k.clone(), Some(serde_json::to_value(f)?)));
        }
        root = tree::edit(self.writer.o, &root, edits, &mut self.writer.read)?;
        let mode = if b.descriptor.completion != Completion::Complete {
            "Unavailable"
        } else {
            match &b.receipt.output {
                Output::Full(_) => "Full",
                Output::Delta { .. } => "Delta",
                Output::Reuse { .. } => "Reuse",
            }
        };
        self.writer.append("scope",&b.descriptor.id()?,&json!({"descriptor":b.descriptor,"mode":mode,"base_revision":b.base_revision,"receipt_digest":b.receipt.digest,"members":root,"state_base":transfer::base_root(key,&next)?,"batches":n}))?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.validator.finish()?;
        self.writer.prepare_queued()
    }
}
impl Sink for NativeSink<'_, '_> {
    fn begin(&mut self, scope: &production::ScopeDescriptor) -> Result<()> {
        self.validator.begin(scope)
    }
    fn batch(
        &mut self,
        scope: &cq_core::semantic::scope::ScopeKey,
        sequence: u64,
        records: &[production::Record],
    ) -> Result<()> {
        self.validator.batch(scope, sequence, records)?;
        for row in records {
            let (family, id) = row
                .key
                .split_once(':')
                .ok_or_else(|| Error::new("InvalidFactKey", &row.key))?;
            self.writer.append(family, id, &row.value)?;
        }
        Ok(())
    }
    fn end(&mut self, scope: &production::ScopeDescriptor, batches: u64) -> Result<()> {
        self.validator.end(scope, batches)?;
        let id = scope.id()?;
        let mode = if scope.completion != cq_core::semantic::scope::Completion::Complete {
            "Unavailable"
        } else {
            "Full"
        };
        self.writer.append("scope",&id,&json!({"descriptor":scope,"mode":mode,"transport":"compatibility-full-snapshot","batches":batches}))
    }
    fn finish(&mut self) -> Result<()> {
        self.validator.finish()
    }
}
impl<'a> Writer<'a> {
    pub fn delete(&mut self, key: &str) -> Result<()> {
        self.delete_many(&[key.to_owned()])
    }
    /// Explicit deletions stay explicit, but affected tree paths are persisted
    /// once per bounded batch rather than once per key and membership group.
    pub fn delete_many(&mut self, keys: &[String]) -> Result<()> {
        if keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
            return Err(Error::new(
                "DuplicateFactDelete",
                "delete batch repeats a key",
            ));
        }
        let mut expanded = keys.iter().cloned().collect::<BTreeSet<_>>();
        for key in keys {
            if let Some(id) = key.strip_prefix("entity:")
                && reference(self.o, &self.roots, "entity_vector", id, &mut self.read)?.is_some()
            {
                expanded.insert(fact_key("entity_vector", id));
            }
        }
        let expanded = expanded.into_iter().collect::<Vec<_>>();
        for batch in expanded.chunks(production::BATCH_ROWS) {
            let mut changes = GroupChanges::new();
            let mut deletes = vec![];
            let mut removals = vec![];
            for key in batch {
                let (family, id) = key
                    .split_once(':')
                    .ok_or_else(|| Error::new("InvalidFactKey", key))?;
                let old = reference(self.o, &self.roots, family, id, &mut self.read)?
                    .ok_or_else(|| Error::new("DeleteUnknownFact", key))?;
                let value = decode(self.o, &old, &mut self.read)?;
                for group in cq_core::ports::facts::groups_indexed(family, &value, self.indexed) {
                    changes.entry(group).or_default().insert(key.clone(), None);
                }
                deletes.push((key.clone(), Some(serde_json::to_value(old)?)));
                removals.push((key.clone(), None));
            }
            memberships(self.o, &mut self.roots, changes, &mut self.read)?;
            self.roots.facts = tree::edit(self.o, &self.roots.facts, removals, &mut self.read)?;
            self.stats.deletes = tree::edit(self.o, &self.stats.deletes, deletes, &mut self.read)?;
            self.stats.facts_deleted += batch.len() as u64;
        }
        Ok(())
    }
    pub fn new(o: &'a Objects, base: Roots) -> Self {
        let initial = base.facts.is_none();
        let indexed = tree::get(
            o,
            &base.facts,
            "search_stats:corpus",
            &mut ReadStats::default(),
        )
        .is_ok_and(|v| v.is_some());
        Self {
            o,
            control: None,
            roots: base,
            stats: WriteStats::default(),
            seen: BTreeSet::new(),
            pending: vec![],
            pending_bytes: 0,
            page_bytes: if initial {
                4 * 1024 * 1024
            } else {
                1024 * 1024
            },
            page_rows: if initial { 4096 } else { 256 },
            read: ReadStats::default(),
            audited: BTreeSet::new(),
            diagnostic_header: None,
            preserve_terms: false,
            vector_model: None,
            vector_enabled: cq_core::query::embeddings::enabled(),
            indexed,
            initial_groups: None,
            initial_facts: None,
            scope_facts: None,
            progress: None,
            completed: 0,
            encoding_queue: Vec::new(),
            encoding_batches: 0,
            encoding_jobs: VecDeque::new(),
            encoding_maximum_pending: 0,
            encoding_submitted: 0,
            encoding_pending_limit: 4,
            closing_scope: None,
            scope_jobs: ScopeJobs::default(),
            scope_jobs_submitted: 0,
        }
    }
    /// The first streamed inventory uses the same bulk path as production
    /// preparation; `new` remains available for copy-on-write edits and replay.
    pub fn initial(o: &'a Objects) -> Result<Self> {
        let mut writer = Self::new(o, Roots::default());
        writer.encoding_pending_limit =
            std::env::var("CQ_ENCODING_PENDING_BATCHES").map_or(Ok(4), |v| {
                v.parse::<usize>().ok().filter(|n| *n <= 8).ok_or_else(|| {
                    Error::new(
                        "InvalidEncodingBudget",
                        "CQ_ENCODING_PENDING_BATCHES must be 0..8",
                    )
                })
            })?;
        writer.initial_facts = Some(super::bulk::Builder::new(&o.root)?);
        writer.initial_groups = Some(super::bulk::Builder::new(&o.root)?);
        Ok(writer)
    }
    fn prepare_row(
        family: &str,
        id: &str,
        value: Value,
        indexed: bool,
        vector_enabled: bool,
        model: Option<std::sync::Arc<cq_core::query::embeddings::TokenVectors>>,
    ) -> Result<Vec<PreparedRow>> {
        let mut rows = Vec::with_capacity(2);
        if family == "entity"
            && indexed
            && vector_enabled
            && let Ok(e) = serde_json::from_value::<Entity>(value.clone())
        {
            let index = cq_core::query::search::SearchIndex::with_model(&[], model);
            let terms = cq_core::query::search::terms(&e);
            let vector = index.prepare_vector(&terms[..terms.len().min(4096)]);
            let buckets = cq_core::query::search::vector_buckets(&vector);
            let v =
                json!({"entity":e,"vector":vector,"buckets":buckets,"model":index.model_name()});
            rows.push(PreparedRow::new("entity_vector", id, v, indexed)?);
        }
        rows.push(PreparedRow::new(family, id, value, indexed)?);
        Ok(rows)
    }
    fn append_batch(&mut self, records: &[production::Record]) -> Result<()> {
        self.append_owned_batch(records.to_vec())
    }
    fn append_owned_batch(&mut self, records: Vec<production::Record>) -> Result<()> {
        if self.initial_facts.is_some() {
            self.encoding_queue
                .extend(records.into_iter().map(EncodingInput::Record));
            self.encoding_batches += 1;
            // Producers/validators retain the 128-row transfer protocol. Only
            // independent encoding is coalesced, bounded by eight input batches
            // (each protocol batch is byte-limited). Ordered scope markers
            // preserve page boundaries without draining at each input scope.
            if self.encoding_batches >= 8 || self.encoding_queue.len() >= 1024 {
                self.schedule_encoding()?;
                self.collect_ready_encoding()?;
            }
            return Ok(());
        }
        let indexed = self.indexed;
        let model = self.vector_model.clone();
        let vector_enabled = self.vector_enabled;
        let prepared = cq_core::compute::map(records, |r| {
            let (family, id) = r
                .key
                .split_once(':')
                .ok_or_else(|| Error::new("InvalidFactKey", &r.key))?;
            Self::prepare_row(family, id, r.value, indexed, vector_enabled, model.clone())
        })?;
        for rows in prepared {
            self.append_prepared(rows)?;
        }
        Ok(())
    }
    fn prepare_queued(&mut self) -> Result<()> {
        self.schedule_encoding()?;
        while let Some(job) = self.encoding_jobs.pop_front() {
            self.apply_encoded(job.join()?)?;
        }
        if let Some((b, staged, n)) = self.closing_scope.take() {
            let row = Self::scope_row(self.o, b, staged, n, self.indexed)?;
            self.append_prepared(vec![row])?;
        }
        let total = self.scope_jobs.0.len() as u64;
        for completed in 0..total {
            if let Some(p) = &mut self.progress {
                p.task(
                    "encode.scope_seals",
                    "封存范围成员与校验摘要",
                    completed,
                    Some(total),
                    "范围",
                );
            }
            self.join_scope()?;
        }
        if total > 0
            && let Some(p) = &mut self.progress
        {
            p.count(total);
        }
        Ok(())
    }
    fn scope_row(
        objects: &Objects,
        b: Box<transfer::Begin>,
        staged: BTreeMap<String, FactRef>,
        n: u64,
        indexed: bool,
    ) -> Result<PreparedRow> {
        let Output::Full(next) = &b.receipt.output else {
            return Err(Error::new("InvalidInitialScope", "full scope required"));
        };
        let mut edits = Vec::with_capacity(next.len());
        for (key, reference) in next {
            let stored = staged
                .get(key)
                .ok_or_else(|| Error::new("MissingTransferredFact", key))?;
            if stored.digest != reference.hash || stored.length != reference.bytes {
                return Err(Error::new(
                    "ScopeResultMismatch",
                    "stored payload differs from transfer",
                ));
            }
            edits.push((key.clone(), Some(serde_json::to_value(stored)?)));
        }
        let members = tree::edit(objects, &None, edits, &mut ReadStats::default())?;
        let state_base = transfer::base_root(&b.descriptor.key, next)?;
        let id = b.descriptor.id()?;
        let value = json!({"descriptor":b.descriptor,"mode":"Full","base_revision":b.base_revision,"receipt_digest":b.receipt.digest,
            "members":members,"state_base":state_base,"batches":n});
        PreparedRow::new("scope", &id, value, indexed)
    }
    fn queue_scope(&mut self, closing: ClosingScope) -> Result<()> {
        if self.scope_jobs.0.len() >= 4 {
            self.join_scope()?;
        }
        let objects = self.o.clone();
        let indexed = self.indexed;
        let estimate = (closing.1.len() as u64)
            .saturating_mul(2048)
            .max(1024 * 1024);
        self.scope_jobs
            .0
            .push_back(cq_core::compute::submit(estimate, move || {
                Self::scope_row(&objects, closing.0, closing.1, closing.2, indexed)
            })?);
        self.scope_jobs_submitted += 1;
        Ok(())
    }
    fn join_scope(&mut self) -> Result<()> {
        let Some(job) = self.scope_jobs.0.pop_front() else {
            return Ok(());
        };
        let row = job.join()?;
        let key = fact_key(&row.family, &row.id);
        if !self.seen.insert(key.clone()) {
            return Err(Error::new("DuplicateFact", key));
        }
        if row.bytes.len() > super::objects::MAX_OBJECT {
            return Err(super::limits::report(
                super::limits::root(&self.o.root),
                &row.family,
                &row.id,
                &row.value,
                row.bytes.len(),
                self.diagnostic_header.as_ref(),
            )?);
        }
        let length = row.bytes.len() as u64;
        let mut bytes = row.bytes;
        bytes.push(b'\n');
        let object = self.o.put(&bytes)?;
        self.stats.inventory_bytes += length;
        self.stats.fact_pages_prepared += 1;
        self.stats.fact_bytes_prepared += bytes.len() as u64;
        let reference = FactRef {
            object,
            offset: 0,
            length,
            digest: row.digest,
        };
        let current = self.scope_facts.take();
        let result = self.commit_rows(vec![(row.family, row.id, row.value, reference, row.groups)]);
        self.scope_facts = current;
        result
    }
    fn apply_encoded(&mut self, rows: Vec<Encoded>) -> Result<()> {
        for row in rows {
            match row {
                Encoded::Rows(rows) => self.append_prepared(rows)?,
                Encoded::BeginScope => {
                    if let Some(closing) = self.closing_scope.take() {
                        self.queue_scope(closing)?;
                    }
                    self.flush_page()?;
                    self.scope_facts = Some(BTreeMap::new());
                }
                Encoded::EndScope(b, n) => {
                    self.flush_page()?;
                    let staged = self.scope_facts.take().ok_or_else(|| {
                        Error::new("MissingScopeMembers", "ordered initial scope")
                    })?;
                    if self.closing_scope.replace((b, staged, n)).is_some() {
                        return Err(Error::new(
                            "InvalidInitialScope",
                            "overlapping scope completion",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    fn collect_ready_encoding(&mut self) -> Result<()> {
        while let Some(job) = self.encoding_jobs.pop_front() {
            match job.try_join() {
                Ok(rows) => self.apply_encoded(rows?)?,
                Err(job) => {
                    self.encoding_jobs.push_front(job);
                    break;
                }
            }
        }
        Ok(())
    }
    fn schedule_encoding(&mut self) -> Result<()> {
        if self.encoding_queue.is_empty() {
            return Ok(());
        }
        // Results retain payloads and membership names, so both submitted work
        // and completed-but-unconsumed work have an explicit four-wave bound.
        if self.encoding_pending_limit > 0
            && self.encoding_jobs.len() >= self.encoding_pending_limit
        {
            let job = self.encoding_jobs.pop_front().unwrap();
            self.apply_encoded(job.join()?)?;
        }
        let rows = std::mem::take(&mut self.encoding_queue);
        self.encoding_batches = 0;
        let indexed = self.indexed;
        let model = self.vector_model.clone();
        let vector_enabled = self.vector_enabled;
        fn heap(v: &Value) -> u64 {
            std::mem::size_of::<Value>() as u64
                + match v {
                    Value::String(s) => s.capacity() as u64,
                    Value::Array(a) => a.iter().map(heap).sum(),
                    Value::Object(m) => m
                        .iter()
                        .map(|(k, v)| k.capacity() as u64 + 48 + heap(v))
                        .sum(),
                    _ => 0,
                }
        }
        let dimension = model.as_ref().map_or(0, |m| m.dimension as u64);
        let estimate = rows
            .iter()
            .map(|r| match r {
                EncodingInput::Record(r) => {
                    heap(&r.value).saturating_mul(8)
                        + if r.key.starts_with("entity:") {
                            dimension.saturating_mul(256)
                        } else {
                            0
                        }
                }
                EncodingInput::BeginScope => 64,
                EncodingInput::EndScope(b, _) => b.descriptor.rows.saturating_mul(512),
            })
            .fold(0u64, u64::saturating_add)
            .max(8 * 1024 * 1024);
        let encode = move || {
            cq_core::compute::map_records(rows, |r| match r {
                EncodingInput::BeginScope => Ok(Encoded::BeginScope),
                EncodingInput::EndScope(b, n) => Ok(Encoded::EndScope(b, n)),
                EncodingInput::Record(r) => {
                    let (family, id) = r
                        .key
                        .split_once(':')
                        .ok_or_else(|| Error::new("InvalidFactKey", &r.key))?;
                    Self::prepare_row(family, id, r.value, indexed, vector_enabled, model.clone())
                        .map(Encoded::Rows)
                }
            })
        };
        if self.encoding_pending_limit == 0 {
            return self.apply_encoded(encode()?);
        }
        let job = cq_core::compute::submit(estimate, encode)?;
        self.encoding_jobs.push_back(job);
        self.encoding_submitted += 1;
        self.encoding_maximum_pending = self.encoding_maximum_pending.max(self.encoding_jobs.len());
        Ok(())
    }
    pub fn append<T: Serialize>(&mut self, family: &str, id: &str, value: &T) -> Result<()> {
        self.prepare_queued()?;
        let bytes = serde_json::to_vec(value)?;
        let mut prepared = Self::prepare_row(
            family,
            id,
            serde_json::to_value(value)?,
            self.indexed,
            self.vector_enabled,
            self.vector_model.clone(),
        )?;
        let row = prepared.last_mut().unwrap();
        row.digest = digest(&bytes);
        row.bytes = bytes;
        self.append_prepared(prepared)
    }
    fn append_prepared(&mut self, rows: Vec<PreparedRow>) -> Result<()> {
        for row in rows {
            let PreparedRow {
                family,
                id,
                value,
                bytes,
                digest: hash,
                groups,
            } = row;
            if ["entity", "body", "occurrence", "relation", "document"].contains(&family.as_str()) {
                self.completed += 1;
                if let Some(p) = &mut self.progress {
                    p.count(self.completed);
                }
            }
            let key = fact_key(&family, &id);
            if !self.seen.insert(key.clone()) {
                return Err(Error::new("DuplicateFact", key));
            }
            self.stats.inventory_bytes += bytes.len() as u64;
            if bytes.len() > super::objects::MAX_OBJECT {
                return Err(super::limits::report(
                    super::limits::root(&self.o.root),
                    &family,
                    &id,
                    &value,
                    bytes.len(),
                    self.diagnostic_header.as_ref(),
                )?);
            }
            if self.initial_facts.is_none()
                && let Some(r) = reference(self.o, &self.roots, &family, &id, &mut self.read)?
                && r.digest == hash
            {
                if self.audited.insert(r.object.hash.clone()) {
                    self.o.audit(&r.object)?;
                }
                self.stats.facts_reused += 1;
                continue;
            }
            if self.pending_bytes + bytes.len() > self.page_bytes
                || self.pending.len() >= self.page_rows
            {
                self.flush_page()?;
            }
            self.pending_bytes += bytes.len() + 1;
            self.pending.push(PreparedRow {
                family,
                id,
                value,
                bytes,
                digest: hash,
                groups,
            });
        }
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if self.control.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(Error::new("Cancelled", "encoding paused"));
        }
        self.prepare_queued()?;
        self.flush_page()
    }
    fn flush_page(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(self.pending_bytes);
        let mut offsets = vec![];
        for row in &self.pending {
            let offset = bytes.len();
            bytes.extend_from_slice(&row.bytes);
            offsets.push((offset, row.bytes.len()));
            bytes.push(b'\n');
        }
        let object = self.o.put(&bytes)?;
        self.stats.fact_pages_prepared += 1;
        self.stats.fact_bytes_prepared += bytes.len() as u64;
        let rows = std::mem::take(&mut self.pending)
            .into_iter()
            .zip(offsets)
            .map(|(row, (offset, length))| {
                (
                    row.family,
                    row.id,
                    row.value,
                    FactRef {
                        object: object.clone(),
                        offset: offset as u64,
                        length: length as u64,
                        digest: row.digest,
                    },
                    row.groups,
                )
            })
            .collect();
        self.pending_bytes = 0;
        self.commit_rows(rows)
    }
    fn commit_rows(&mut self, rows: Vec<IndexedRow>) -> Result<()> {
        if let Some(initial_facts) = &mut self.initial_facts {
            let groups = self
                .initial_groups
                .as_mut()
                .ok_or_else(|| Error::new("InvalidInitialWriter", "missing group builder"))?;
            let mut input = rows.into_iter();
            loop {
                let batch = input.by_ref().take(1024).collect::<Vec<_>>();
                if batch.is_empty() {
                    break;
                }
                let batch =
                    cq_core::compute::map_records(batch, |(family, id, _value, r, names)| {
                        let key = fact_key(&family, &id);
                        let reference = serde_json::to_value(&r)?;
                        let postings = super::bulk::prepare_memberships(
                            names,
                            key.clone(),
                            reference.clone(),
                        )?;
                        Ok((key, reference, postings, r))
                    })?;
                let mut facts = Vec::with_capacity(batch.len());
                let mut postings = Vec::new();
                for (key, reference, names, r) in batch {
                    facts.push(("facts".into(), key.clone(), reference));
                    if let Some(scope) = &mut self.scope_facts {
                        scope.insert(key, r);
                    }
                    postings.push(names);
                    self.stats.facts_changed += 1;
                }
                initial_facts.push_many(facts)?;
                groups.push_prepared(postings)?;
            }
            self.pending_bytes = 0;
            return Ok(());
        }
        let mut memberships_by_group = GroupChanges::new();
        let mut facts = vec![];
        for (family, id, _value, r, names) in rows {
            let key = fact_key(&family, &id);
            if self.initial_groups.is_none()
                && let Some(old) = reference(self.o, &self.roots, &family, &id, &mut self.read)?
            {
                let previous = decode(self.o, &old, &mut self.read)?;
                for g in cq_core::ports::facts::groups_indexed(&family, &previous, self.indexed) {
                    memberships_by_group
                        .entry(g)
                        .or_default()
                        .insert(key.clone(), None);
                }
            }
            let value_ref = serde_json::to_value(&r)?;
            facts.push((key.clone(), Some(value_ref.clone())));
            for g in names {
                memberships_by_group
                    .entry(g)
                    .or_default()
                    .insert(key.clone(), Some(value_ref.clone()));
            }
            self.stats.facts_changed += 1;
        }
        self.roots.facts = tree::edit(self.o, &self.roots.facts, facts.clone(), &mut self.read)?;
        self.stats.upserts = if self.initial_groups.is_some() {
            self.roots.facts.clone()
        } else {
            tree::edit(self.o, &self.stats.upserts, facts, &mut self.read)?
        };
        if let Some(initial) = &mut self.initial_groups {
            for (name, changes) in memberships_by_group {
                for (key, value) in changes {
                    let value = value.ok_or_else(|| {
                        Error::new("InitialIndexConflict", "initial build attempted a deletion")
                    })?;
                    initial.push(&name, &key, value)?;
                }
            }
        } else {
            memberships(
                self.o,
                &mut self.roots,
                memberships_by_group,
                &mut self.read,
            )?;
        }
        self.pending_bytes = 0;
        Ok(())
    }
    /// Called only after the producer's complete inventory has been validated.
    /// Sparse/delta producers must use explicit deletes rather than this method.
    pub fn finish_inventory(self) -> Result<(Roots, WriteStats)> {
        self.finish(false)
    }
    fn finish_scoped(self) -> Result<(Roots, WriteStats)> {
        self.finish(true)
    }
    fn finish(mut self, preserve_semantic: bool) -> Result<(Roots, WriteStats)> {
        self.flush()?;
        if let Some(facts) = self.initial_facts.take() {
            let groups = self
                .initial_groups
                .take()
                .ok_or_else(|| Error::new("InvalidInitialWriter", "missing group builder"))?;
            let (directories, groups) = super::bulk::Builder::build_pair(facts, groups, self.o)?;
            let group: Group = tree::get(self.o, &directories, "facts", &mut self.read)?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            self.roots.facts = group.root;
            self.stats.upserts = self.roots.facts.clone();
            self.roots.groups = groups;
            return Ok((self.roots, self.stats));
        }
        if let Some(initial) = self.initial_groups.take() {
            self.roots.groups = initial.build(self.o)?;
            return Ok((self.roots, self.stats));
        }
        if preserve_semantic {
            for family in [
                "scope",
                "unit",
                "context",
                "build",
                "runtime_trace",
                "order",
                "context_stats",
                "provenance",
                "file",
                "providers",
                "coverage",
                "gaps",
                "analysis_inputs",
                "analysis_directories",
                "build_dependencies",
                "working_tree",
                "term_df",
                "search_stats",
                "entity_vector",
                "metadata",
                "observation_unit",
                "observation_provenance",
            ] {
                if family == "entity_vector" || family == "term_df" && self.preserve_terms {
                    continue;
                }
                let group: Group = tree::get(
                    self.o,
                    &self.roots.groups,
                    &format!("all:{family}"),
                    &mut self.read,
                )?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
                let mut after = None;
                loop {
                    let page =
                        tree::scan(self.o, &group.root, after.as_deref(), 128, &mut self.read)?;
                    let keys = page
                        .rows
                        .into_iter()
                        .map(|(_, k, _)| k)
                        .filter(|k| !self.seen.contains(k))
                        .collect::<Vec<_>>();
                    self.delete_many(&keys)?;
                    if page.complete {
                        break;
                    }
                    after = page.after;
                }
            }
            return Ok((self.roots, self.stats));
        }
        let mut after = None;
        let original = self.roots.facts.clone();
        loop {
            let p = tree::scan(self.o, &original, after.as_deref(), 256, &mut self.read)?;
            let mut membership_changes = GroupChanges::new();
            let mut deletes = vec![];
            let mut removals = vec![];
            for (_, key, v) in p.rows {
                if self.seen.contains(&key) {
                    continue;
                }
                let (family, _) = key
                    .split_once(':')
                    .ok_or_else(|| Error::new("InvalidFactKey", &key))?;
                if preserve_semantic
                    && ["entity", "body", "occurrence", "relation", "document"].contains(&family)
                {
                    continue;
                }
                let r = serde_json::from_value(v.clone())?;
                let value = decode(self.o, &r, &mut self.read)?;
                for g in cq_core::ports::facts::groups_indexed(family, &value, self.indexed) {
                    membership_changes
                        .entry(g)
                        .or_default()
                        .insert(key.clone(), None);
                }
                deletes.push((key.clone(), Some(v)));
                removals.push((key, None));
                self.stats.facts_deleted += 1;
            }
            memberships(self.o, &mut self.roots, membership_changes, &mut self.read)?;
            self.stats.deletes = tree::edit(self.o, &self.stats.deletes, deletes, &mut self.read)?;
            self.roots.facts = tree::edit(self.o, &self.roots.facts, removals, &mut self.read)?;
            if p.complete {
                break;
            }
            after = p.after;
        }
        Ok((self.roots, self.stats))
    }
}

/// Compatibility adapter consumes records independently; it never serializes a
/// complete SemanticSnapshot. Native producer streaming is a separate boundary.
pub fn snapshot(
    o: &Objects,
    blobs: &crate::content::BlobStore,
    s: &SemanticSnapshot,
    prior: Option<&Revision>,
) -> Result<Revision> {
    write(o, blobs, s, prior, None, None)
}
pub fn scoped(
    o: &Objects,
    blobs: &crate::content::BlobStore,
    prepared: &crate::analysis::Prepared,
) -> Result<Revision> {
    write(
        o,
        blobs,
        &prepared.header,
        prepared.base.manifest(),
        Some(prepared),
        None,
    )
}
pub(crate) fn scoped_tracked(
    o: &Objects,
    blobs: &crate::content::BlobStore,
    prepared: &crate::analysis::Prepared,
    execution: &crate::execution::Execution,
) -> Result<Revision> {
    write(
        o,
        blobs,
        &prepared.header,
        prepared.base.manifest(),
        Some(prepared),
        Some(execution),
    )
}
#[derive(Serialize, Deserialize)]
struct EncodedState {
    roots: Roots,
    stats: WriteStats,
    seen: BTreeSet<String>,
    preserve_terms: bool,
    indexed: bool,
    facts: Option<std::path::PathBuf>,
    groups: Option<std::path::PathBuf>,
    source_root: Root,
    statistics: Option<crate::analysis::Statistics>,
    transfer_stats: Value,
    vector_model: String,
}
pub(crate) fn resume_directories(
    o: &Objects,
    e: &crate::execution::Execution,
    header: &SemanticSnapshot,
    template: &Revision,
) -> Result<Revision> {
    use cq_core::execution::Stage;
    let mut header = header.clone();
    let encoded = e.start(Stage::Encode, &mut header, None)?;
    if !encoded.reused {
        return Err(Error::new("StageDependencyIncomplete", "encoded input"));
    }
    let entry = e.start(Stage::Directories, &mut header, None)?;
    let (roots, changes) = if entry.reused {
        serde_json::from_reader(std::fs::File::open(
            entry.cursor["roots"]
                .as_str()
                .ok_or_else(|| Error::new("MissingStageArtifact", "directories"))?,
        )?)?
    } else {
        let saved: EncodedState =
            serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(
                encoded.cursor["writer"]
                    .as_str()
                    .ok_or_else(|| Error::new("MissingStageArtifact", "encoded writer"))?,
            )?))?;
        let mut w = Writer::new(o, saved.roots);
        w.stats = saved.stats;
        w.seen = saved.seen;
        w.preserve_terms = saved.preserve_terms;
        w.indexed = saved.indexed;
        w.initial_facts = saved
            .facts
            .map(|p| super::bulk::Builder::restore(&o.root, &p))
            .transpose()?;
        w.initial_groups = saved
            .groups
            .map(|p| super::bulk::Builder::restore(&o.root, &p))
            .transpose()?;
        w.control = Some(e.control());
        let directory = e.stage_file(Stage::Directories, "ranges")?;
        if let Some(b) = &mut w.initial_facts {
            b.cancel_with(e.control());
            b.resume_in(directory.join("facts"))?;
        }
        if let Some(b) = &mut w.initial_groups {
            b.cancel_with(e.control());
            b.resume_in(directory.join("groups"))?;
        }
        let (mut roots, changes) = w.finish_scoped()?;
        roots.sources = saved.source_root;
        o.sync_pending()?;
        let path = e.stage_file(Stage::Directories, "roots.json")?;
        crate::atomic::immutable_write(&path, &serde_json::to_vec(&(&roots, &changes))?)?;
        e.finish(Stage::Directories, &header, None, json!({"roots":path}))?;
        (roots, changes)
    };
    let mut revision = template.clone();
    revision.roots = roots;
    revision.changes = changes;
    revision.seal()?;
    Ok(revision)
}
fn write(
    o: &Objects,
    blobs: &crate::content::BlobStore,
    s: &SemanticSnapshot,
    prior: Option<&Revision>,
    prepared: Option<&crate::analysis::Prepared>,
    execution: Option<&crate::execution::Execution>,
) -> Result<Revision> {
    use cq_core::execution::Stage;
    let mut header = s.clone();
    let saved_statistics = if let Some(e) = execution {
        let stage = e.start(Stage::Statistics, &mut header, None)?;
        if stage.reused {
            serde_json::from_reader(std::fs::File::open(
                stage.cursor["statistics"]
                    .as_str()
                    .ok_or_else(|| Error::new("MissingStageArtifact", "statistics"))?,
            )?)?
        } else {
            let stats = if prior.is_none() {
                prepared.map(|p| p.spool.statistics(s)).transpose()?
            } else {
                None
            };
            let path = e.stage_file(Stage::Statistics, "statistics.json")?;
            let mut writer = std::io::BufWriter::new(std::fs::File::create(&path)?);
            serde_json::to_writer(&mut writer, &stats)?;
            std::io::Write::flush(&mut writer)?;
            writer.get_ref().sync_all()?;
            e.finish(Stage::Statistics, s, None, json!({"statistics":path}))?;
            stats
        }
    } else {
        prepared.map(|p| p.spool.statistics(s)).transpose()?
    };
    let entry = execution
        .map(|e| e.start(Stage::Encode, &mut header, None))
        .transpose()?;
    let (mut w, statistics, transfer_stats, source_root, vector_model) = if let Some(entry) =
        entry.as_ref().filter(|v| v.reused)
    {
        let saved: EncodedState =
            serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(
                entry.cursor["writer"]
                    .as_str()
                    .ok_or_else(|| Error::new("MissingStageArtifact", "encoded writer"))?,
            )?))?;
        let mut writer = Writer::new(o, saved.roots);
        writer.stats = saved.stats;
        writer.seen = saved.seen;
        writer.preserve_terms = saved.preserve_terms;
        writer.indexed = saved.indexed;
        writer.initial_facts = saved
            .facts
            .map(|p| super::bulk::Builder::restore(&o.root, &p))
            .transpose()?;
        writer.initial_groups = saved
            .groups
            .map(|p| super::bulk::Builder::restore(&o.root, &p))
            .transpose()?;
        (
            writer,
            saved.statistics,
            saved.transfer_stats,
            saved.source_root,
            saved.vector_model,
        )
    } else {
        let mut w = if prior.is_none() && prepared.is_some() {
            Writer::initial(o)?
        } else {
            Writer::new(o, prior.map(|r| r.roots.clone()).unwrap_or_default())
        };
        w.control = execution.map(|e| e.control());
        let count=prepared.map(|p|p.spool.counts()).unwrap_or_else(||json!({"entities":s.entities.len(),"bodies":s.bodies.len(),"relations":s.relations.len(),"occurrences":s.occurrences.len(),"documents":s.documents.len()}));
        let total = [
            "entities",
            "bodies",
            "relations",
            "occurrences",
            "documents",
        ]
        .into_iter()
        .map(|k| count[k].as_u64().unwrap_or(0))
        .sum();
        if let Some(store) = super::limits::root(&o.root) {
            w.progress = Some(cq_core::progress::Progress::stage_with_workers(
                store,
                &s.project,
                "encode",
                "编码事实分片",
                Some(total),
                "事实",
                cq_core::compute::workers()?,
            ));
        }
        if prior.is_none() {
            if w.initial_groups.is_none() {
                w.initial_groups = Some(super::bulk::Builder::new(&o.root)?);
            }
            if let Some(b) = &mut w.initial_groups {
                b.project = Some(s.project.clone());
            }
            if prepared.is_some() {
                w.initial_facts.as_mut().unwrap().project = Some(s.project.clone());
            }
        }
        w.vector_model = cq_core::query::embeddings::TokenVectors::configured()?;
        w.indexed = prior.is_none_or(|r| r.metadata["search_index"]["version"] == 1);
        let vector_model = if w.vector_enabled {
            cq_core::query::search::SearchIndex::with_model(&[], w.vector_model.clone())
                .model_name()
        } else {
            "disabled".to_owned()
        };
        if prior.is_some_and(|r| {
            r.metadata["search_index"]["version"] == 1
                && w.vector_enabled
                && r.metadata["search_index"]["vector_model"] != vector_model
        }) {
            return Err(Error::new(
                "VectorIndexModelMismatch",
                "keep the original token model for incremental publication, or index into a fresh project namespace",
            ));
        }
        w.diagnostic_header = Some(s.metadata());
        let mut aggregate = if let Some(p) = prepared {
            super::statistics::Incremental::new(o, s, prior, &p.spool.universe)?
        } else {
            None
        };
        let incremental_statistics = aggregate.is_some();
        w.preserve_terms = incremental_statistics;
        let mut statistics = if aggregate.is_none() {
            saved_statistics
        } else {
            None
        };
        let mut transfer_stats = Value::Null;
        if let Some(p) = prepared
            && prior.is_some_and(|r| r.metadata["scope_protocol"] != 2)
        {
            let original = w.roots.facts.clone();
            let mut deletes = vec![];
            tree::visit(o, &original, |key, _| {
                if key.split_once(':').is_some_and(|(f, _)| {
                    ["entity", "body", "occurrence", "relation", "document"].contains(&f)
                }) && !p.spool.contains(key)
                {
                    deletes.push(key.to_owned());
                }
                Ok(())
            })?;
            w.delete_many(&deletes)?;
        }

        macro_rules! records {
            ($field:ident,$family:literal) => {
                for item in &s.$field {
                    w.append($family, &item.id, item)?;
                }
            };
        }
        if let Some(p) = &s.production {
            if let Some(prepared) = prepared {
                let mut sink = ScopedSink {
                    writer: &mut w,
                    base: &prepared.base,
                    validator: transfer::Validator::new(p, &prepared.base)?,
                    current: None,
                    compact: false,
                    aggregate: aggregate.take(),
                    spool: &prepared.spool,
                    stats: json!({"full_scopes":0,"delta_scopes":0,"reused_scopes":0,"payload_rows_submitted":0}),
                };
                prepared.spool.emit(s, &prepared.base, &mut sink)?;
                if let Some(stats) = sink.aggregate.take() {
                    statistics = Some(stats.finish(o, prior.unwrap())?);
                }
                transfer_stats = sink.stats;
            } else {
                let mut sink = NativeSink {
                    validator: production::Validator::new(p)?,
                    writer: &mut w,
                };
                production::emit(s, &mut sink)?;
            }
            w.stats.identity_unmatched = Some(p.identity_unmatched);
            let mut metadata = p.clone();
            metadata.scopes.clear();
            metadata.unit_observations.clear();
            metadata.provenance_observations.clear();
            w.append("metadata", "production", &metadata)?;
            for (i, v) in p.unit_observations.iter().enumerate() {
                w.append("observation_unit", &i.to_string(), v)?;
            }
            for (i, v) in p.provenance_observations.iter().enumerate() {
                w.append("observation_provenance", &i.to_string(), v)?;
            }
        } else {
            records!(entities, "entity");
            records!(relations, "relation");
            records!(occurrences, "occurrence");
            records!(bodies, "body");
            records!(documents, "document");
        }
        records!(runtime_traces, "runtime_trace");
        records!(units, "unit");
        records!(contexts, "context");
        records!(builds, "build");
        macro_rules! order {
            ($field:ident,$family:literal) => {
                w.append(
                    "order",
                    $family,
                    &s.$field.iter().map(|v| &v.id).collect::<Vec<_>>(),
                )?;
            };
        }
        order!(units, "unit");
        order!(contexts, "context");
        order!(builds, "build");
        order!(runtime_traces, "runtime_trace");
        for context in &s.contexts {
            if let Some(stats) = &statistics {
                w.append("context_stats", &context.id, &stats.contexts[&context.id])?;
                continue;
            }
            let units = context
                .units
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let entities = s
                .entities
                .iter()
                .filter(|e| units.contains(e.unit.as_str()))
                .collect::<Vec<_>>();
            let ids = entities
                .iter()
                .map(|e| e.id.as_str())
                .collect::<BTreeSet<_>>();
            let edges = s
                .relations
                .iter()
                .filter(|r| ids.contains(r.from.as_str()) && ids.contains(r.to.as_str()))
                .collect::<Vec<_>>();
            let mut kinds = BTreeMap::<String, usize>::new();
            for e in &entities {
                *kinds.entry(e.kind.label()).or_default() += 1;
            }
            let mut relation_kinds = BTreeMap::<String, usize>::new();
            for r in &edges {
                *relation_kinds.entry(r.kind.label().into()).or_default() += 1;
            }
            w.append("context_stats",&context.id,&json!({"project":s.project,"counts":{"entities":entities.len(),"relations":edges.len(),"occurrences":s.occurrences.iter().filter(|o|units.contains(o.unit.as_str())).count(),"documents":s.documents.iter().filter(|d|d.owner.as_ref().is_some_and(|id|ids.contains(id.as_str()))).count(),"contexts":1},"entity_kinds":kinds,"relation_kinds":relation_kinds}))?;
        }
        for (id, p) in &s.provenance_records {
            w.append("provenance", &id.0, p)?;
        }
        let mut source_root = prior.and_then(|r| r.roots.sources.clone());
        let mut source_seen = BTreeSet::new();
        for f in &s.sources.files {
            w.append("file", &f.id, f)?;
            source_seen.insert(f.id.clone());
            if tree::get(o, &source_root, &f.id, &mut ReadStats::default())?.is_none() {
                let frozen =
                    Source::freeze(o, &f.blob, std::fs::File::open(blobs.path(&f.blob)?)?)?;
                source_root = tree::set(
                    o,
                    &source_root,
                    &f.id,
                    Some(serde_json::to_value(frozen)?),
                    &mut ReadStats::default(),
                )?;
            }
        }
        let initial = source_root.clone();
        tree::visit(o, &initial, |key, _| {
            if !source_seen.contains(key) {
                source_root = tree::set(o, &source_root, key, None, &mut ReadStats::default())?;
            }
            Ok(())
        })?;
        // Large metadata collections remain pageable records, including observation
        // bindings. A new build ID does not get injected into unchanged fact pages.
        macro_rules! indexed {
            ($field:ident) => {
                for (i, v) in s.$field.iter().enumerate() {
                    w.append(stringify!($field), &i.to_string(), v)?;
                }
            };
        }
        indexed!(providers);
        indexed!(coverage);
        indexed!(gaps);
        indexed!(analysis_inputs);
        indexed!(analysis_directories);
        indexed!(build_dependencies);
        for (k, v) in &s.working_tree_manifest {
            w.append("working_tree", k, v)?;
        }
        let mut df = BTreeMap::<String, u64>::new();
        let mut term_total = 0_u64;
        if let Some(stats) = &statistics {
            df = stats.df.clone();
            term_total = stats.terms;
        }
        for e in &s.entities {
            let terms = cq_core::query::search::terms(e);
            term_total += terms.len() as u64;
            for t in terms.into_iter().collect::<BTreeSet<_>>() {
                *df.entry(t).or_default() += 1;
            }
        }
        for (term, n) in df {
            if incremental_statistics && n == 0 {
                w.delete(&fact_key("term_df", &term))?;
            } else {
                w.append("term_df", &term, &n)?;
            }
        }
        w.append(
        "search_stats",
        "corpus",
        &json!({"documents":statistics.as_ref().map(|s|s.counts["entities"].as_u64().unwrap_or(0)).unwrap_or(s.entities.len()as u64),"terms":term_total}),
    )?;
        let mut source_meta = s.sources.clone();
        source_meta.files.clear();
        w.append("metadata", "source_snapshot", &source_meta)?;
        w.append("metadata", "analysis_request", &s.analysis_request)?;
        let vector_model = if w.vector_enabled {
            cq_core::query::search::SearchIndex::with_model(&[], w.vector_model.clone())
                .model_name()
        } else {
            "disabled".to_owned()
        };
        if let Some(e) = execution {
            w.flush()?;
            o.sync_pending()?;
            let path = e.stage_file(Stage::Encode, "writer.json")?;
            let facts = w
                .initial_facts
                .as_mut()
                .map(|b| {
                    let p = path.with_extension("facts");
                    b.retain(&p)?;
                    Ok::<_, Error>(p)
                })
                .transpose()?;
            let groups = w
                .initial_groups
                .as_mut()
                .map(|b| {
                    let p = path.with_extension("groups");
                    b.retain(&p)?;
                    Ok::<_, Error>(p)
                })
                .transpose()?;
            let saved = EncodedState {
                roots: w.roots.clone(),
                stats: w.stats.clone(),
                seen: w.seen.clone(),
                preserve_terms: w.preserve_terms,
                indexed: w.indexed,
                facts,
                groups,
                source_root: source_root.clone(),
                statistics,
                transfer_stats,
                vector_model: vector_model.to_owned(),
            };
            let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
            serde_json::to_writer(&mut out, &saved)?;
            std::io::Write::flush(&mut out)?;
            out.get_ref().sync_all()?;
            e.finish(Stage::Encode, s, None, json!({"writer":path}))?;
            (
                w,
                saved.statistics,
                saved.transfer_stats,
                source_root,
                saved.vector_model,
            )
        } else {
            (
                w,
                statistics,
                transfer_stats,
                source_root,
                vector_model.to_owned(),
            )
        }
    };
    w.control = execution.map(|e| e.control());
    let entry = execution
        .map(|e| e.start(Stage::Directories, &mut header, None))
        .transpose()?;
    if let Some(e) = execution {
        let directory = e.stage_file(Stage::Directories, "ranges")?;
        if let Some(b) = &mut w.initial_facts {
            b.cancel_with(e.control());
            b.resume_in(directory.join("facts"))?;
        }
        if let Some(b) = &mut w.initial_groups {
            b.cancel_with(e.control());
            b.resume_in(directory.join("groups"))?;
        }
    }
    let (mut roots, stats) = if let Some(entry) = entry.as_ref().filter(|v| v.reused) {
        serde_json::from_reader(std::fs::File::open(
            entry.cursor["roots"]
                .as_str()
                .ok_or_else(|| Error::new("MissingStageArtifact", "directories"))?,
        )?)?
    } else {
        let (roots, stats) = if prepared.is_some() {
            w.finish_scoped()?
        } else {
            w.finish_inventory()?
        };
        o.sync_pending()?;
        if let Some(e) = execution {
            let path = e.stage_file(Stage::Directories, "roots.json")?;
            let pinned = Roots {
                sources: source_root.clone(),
                ..roots.clone()
            };
            crate::atomic::immutable_write(&path, &serde_json::to_vec(&(&pinned, &stats))?)?;
            e.finish(Stage::Directories, s, None, json!({"roots":path}))?;
        }
        (roots, stats)
    };
    roots.sources = source_root;
    let mut kinds = BTreeMap::<String, usize>::new();
    for e in &s.entities {
        *kinds.entry(e.kind.label()).or_default() += 1;
    }
    let mut relations = BTreeMap::<String, usize>::new();
    for r in &s.relations {
        *relations.entry(r.kind.label().into()).or_default() += 1;
    }
    let mut meta = json!({"project":s.project,"revision":s.revision,"schema_version":s.schema_version,"root":s.root,"created_ms":s.created_ms,"previous":s.previous,"counts":{"entities":s.entities.len(),"relations":s.relations.len(),"occurrences":s.occurrences.len(),"bodies":s.bodies.len(),"documents":s.documents.len(),"contexts":s.contexts.len(),"units":s.units.len(),"external":s.entities.iter().filter(|e|e.external).count(),"coverage":s.coverage.len(),"gaps":s.gaps.len()},"entity_kinds":kinds,"relation_kinds":relations});
    let indexed = prior.is_none_or(|r| r.metadata["search_index"]["version"] == 1);
    meta["search_index"] = json!({"version":if indexed{1}else{0},"vector_model":vector_model,"upgrade_required":!indexed,"ranking_window":1000,"vector_enabled":cq_core::query::embeddings::enabled()});
    let rejections = s
        .builds
        .iter()
        .flat_map(|b| {
            b.rejected_fields
                .iter()
                .map(move |r| json!({"family":"build","id":b.id,"rejection":r}))
        })
        .collect::<Vec<_>>();
    if !rejections.is_empty() {
        meta["storage_rejections"] = json!(rejections);
        meta["completion"] = json!("complete_with_warnings");
    }
    if let Some(stats) = &statistics {
        meta["counts"] = stats.counts.clone();
        meta["entity_kinds"] = json!(stats.entity_kinds);
        meta["relation_kinds"] = json!(stats.relation_kinds);
        meta["scope_protocol"] = json!(2);
        meta["transfer"] = transfer_stats;
    }
    if let Some(p) = &s.production {
        meta["production"] = json!({"version":p.version,"partial":p.partial,"selection_complete":p.selection_complete,"scopes":p.scopes.len(),"identity_unmatched":p.identity_unmatched,"stage_ms":p.stage_ms,"native_boundary":p.native_boundary});
    }
    if let Some(p) = prepared {
        meta["semantic_generation"] = p.generation.clone();
    } else if s.entities.len() + s.relations.len() + s.occurrences.len() <= 20_000 {
        let generation =
            cq_core::semantic::generation::project(s, prior.map_or(1, |r| r.sequence + 1))?;
        let previous = prior.and_then(|r| r.metadata.get("semantic_generation"));
        let compatible = previous.filter(|p| p["universe"] == generation.universe);
        let old = compatible
            .and_then(|p| p["manifest"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        Some((
                            v["key"].as_str()?.to_owned(),
                            v["digest"].as_str()?.to_owned(),
                        ))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let upserts = generation
            .manifest
            .iter()
            .filter(|m| old.get(&m.key) != Some(&m.digest))
            .map(|m| &m.key)
            .collect::<Vec<_>>();
        let deletes = old
            .keys()
            .filter(|k| !generation.shards.contains_key(*k))
            .collect::<Vec<_>>();
        meta["semantic_generation"] = json!({"generation":generation.generation,"universe":generation.universe,"manifest":generation.manifest,"upserts":upserts,"deletes":deletes,"base_generation":compatible.and_then(|p|p.get("generation")),"method":"canonical generation v1; computed at publication for bounded small snapshots; physical write counts remain separate"});
    } else {
        meta["semantic_generation"] = json!({"state":"explicit_generation_job_required","reason":"canonical generation v1 exceeds the 20000-record precomputation budget"});
    }
    let mut r = Revision {
        format: 2,
        project: s.project.clone(),
        revision: s.revision.clone(),
        parent: prior.map(|r| r.revision.clone()),
        sequence: prior.map_or(1, |r| r.sequence + 1),
        roots,
        metadata: meta,
        changes: stats,
        seal: String::new(),
    };
    r.seal()?;
    Ok(r)
}

pub fn export(o: &Objects, r: &Revision) -> Result<SemanticSnapshot> {
    let mut orderings = BTreeMap::<String, Vec<String>>::new();
    let mut scopes = vec![];
    let mut observed_units = BTreeMap::new();
    let mut observed_provenance = BTreeMap::new();
    let mut s = SemanticSnapshot::empty(
        r.project.clone(),
        serde_json::from_value(r.metadata["root"].clone())?,
    );
    s.schema_version = r.metadata["schema_version"].as_u64().unwrap_or(2) as u32;
    s.revision = r.revision.clone();
    s.previous = r.metadata["previous"].as_str().map(str::to_owned);
    s.created_ms = r.metadata["created_ms"].as_u64().unwrap_or(0);
    tree::visit(o, &r.roots.facts, |key, v| {
        let (family, id) = key
            .split_once(':')
            .ok_or_else(|| Error::new("InvalidFactKey", key))?;
        let f: FactRef = serde_json::from_value(v.clone())?;
        let v = decode(o, &f, &mut ReadStats::default())?;
        macro_rules! push {
            ($field:ident) => {
                s.$field.push(serde_json::from_value(v)?)
            };
        }
        match family {
            "scope" => scopes.push(serde_json::from_value::<production::ScopeDescriptor>(
                v["descriptor"].clone(),
            )?),
            "observation_unit" => {
                observed_units.insert(
                    id.parse::<usize>()
                        .map_err(|_| Error::new("InvalidFactKey", key))?,
                    serde_json::from_value::<production::UnitObservation>(v)?,
                );
            }
            "observation_provenance" => {
                observed_provenance.insert(
                    id.parse::<usize>()
                        .map_err(|_| Error::new("InvalidFactKey", key))?,
                    serde_json::from_value::<production::ProvenanceObservation>(v)?,
                );
            }
            "entity" => push!(entities),
            "relation" => push!(relations),
            "occurrence" => push!(occurrences),
            "body" => push!(bodies),
            "document" => push!(documents),
            "runtime_trace" => push!(runtime_traces),
            "unit" => push!(units),
            "context" => push!(contexts),
            "build" => push!(builds),
            "file" => s.sources.files.push(serde_json::from_value(v)?),
            "provenance" => {
                s.provenance_records
                    .insert(ProvenanceId(id.into()), serde_json::from_value(v)?);
            }
            "providers" => push!(providers),
            "coverage" => push!(coverage),
            "gaps" => push!(gaps),
            "analysis_inputs" => push!(analysis_inputs),
            "analysis_directories" => push!(analysis_directories),
            "build_dependencies" => push!(build_dependencies),
            "working_tree" => {
                s.working_tree_manifest
                    .insert(id.into(), serde_json::from_value(v)?);
            }
            "term_df" | "search_stats" | "context_stats" | "entity_vector" => {}
            "order" => {
                orderings.insert(id.into(), serde_json::from_value(v)?);
            }
            "metadata" if id == "analysis_request" => {
                s.analysis_request = serde_json::from_value(v)?;
            }
            "metadata" if id == "production" => {
                s.production = Some(serde_json::from_value(v)?);
            }
            "metadata" if id == "source_snapshot" => {
                let files = std::mem::take(&mut s.sources.files);
                s.sources = serde_json::from_value(v)?;
                s.sources.files = files;
            }
            _ => return Err(Error::new("UnknownFactFamily", family)),
        }
        Ok(())
    })?;
    if let Some(p) = &mut s.production {
        scopes.sort_by(|a, b| a.key.cmp(&b.key));
        p.scopes = scopes;
        p.unit_observations = observed_units.into_values().collect();
        p.provenance_observations = observed_provenance.into_values().collect();
    }
    // Metadata indexed by ordinal must retain original ordering, not trie order.
    macro_rules! reload {
        ($field:ident) => {{
            let mut values = vec![];
            tree::visit(o, &r.roots.facts, |key, v| {
                if let Some(i) = key.strip_prefix(concat!(stringify!($field), ":")) {
                    values.push((
                        i.parse::<usize>()
                            .map_err(|_| Error::new("InvalidFactKey", key))?,
                        decode(
                            o,
                            &serde_json::from_value(v.clone())?,
                            &mut ReadStats::default(),
                        )?,
                    ));
                }
                Ok(())
            })?;
            values.sort_by_key(|(i, _)| *i);
            s.$field = values
                .into_iter()
                .map(|(_, v)| serde_json::from_value(v).map_err(Into::into))
                .collect::<Result<_>>()?;
        }};
    }
    reload!(providers);
    reload!(coverage);
    reload!(gaps);
    reload!(analysis_inputs);
    reload!(analysis_directories);
    reload!(build_dependencies);
    s.entities.sort_by(|a, b| a.id.cmp(&b.id));
    s.relations.sort_by(|a, b| a.id.cmp(&b.id));
    s.occurrences.sort_by(|a, b| a.id.cmp(&b.id));
    s.bodies.sort_by(|a, b| a.id.cmp(&b.id));
    s.documents.sort_by(|a, b| a.id.cmp(&b.id));
    s.sources
        .files
        .sort_by(|a, b| a.logical_path.cmp(&b.logical_path));
    macro_rules! reorder {
        ($field:ident,$family:literal) => {
            if let Some(ids) = orderings.get($family) {
                let positions = ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (id.as_str(), i))
                    .collect::<BTreeMap<_, _>>();
                s.$field
                    .sort_by_key(|v| positions.get(v.id.as_str()).copied().unwrap_or(usize::MAX));
            }
        };
    }
    reorder!(units, "unit");
    reorder!(contexts, "context");
    reorder!(builds, "build");
    reorder!(runtime_traces, "runtime_trace");
    Ok(s)
}

#[cfg(test)]
mod performance_tests {
    use super::*;
    #[test]
    #[ignore = "manual retained payload publication profile: CQ_RAW_SAMPLE, CQ_WRITER_REPORT"]
    fn retained_payloads_profile_the_complete_initial_writer() {
        let input = std::env::var("CQ_RAW_SAMPLE").unwrap();
        let count: usize = std::env::var("CQ_RAW_COUNT").unwrap().parse().unwrap();
        let report = std::env::var("CQ_WRITER_REPORT").unwrap();
        let reader =
            std::io::BufReader::with_capacity(8 * 1024 * 1024, std::fs::File::open(input).unwrap());
        let mut unique = BTreeMap::new();
        for row in serde_json::Deserializer::from_reader(reader)
            .into_iter::<cq_core::semantic::records::Record>()
            .take(count)
        {
            let row = row.unwrap();
            unique.insert(fact_key(row.family(), row.id()), row.value().unwrap());
        }
        let temp = tempfile::tempdir().unwrap();
        let objects = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut writer = Writer::initial(&objects).unwrap();
        writer.indexed = true;
        writer.vector_model = cq_core::query::embeddings::TokenVectors::configured().unwrap();
        let embedding_model_configured = writer.vector_model.is_some();
        let started = std::time::Instant::now();
        let mut input = unique.iter();
        loop {
            let batch = input
                .by_ref()
                .take(production::BATCH_ROWS)
                .map(|(key, value)| production::Record {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            writer.append_batch(&batch).unwrap();
        }
        writer.flush().unwrap();
        let feed = started.elapsed().as_secs_f64();
        let (roots, stats) = writer.finish_inventory().unwrap();
        let finish = started.elapsed().as_secs_f64() - feed;
        objects.sync_pending().unwrap();
        let duration = started.elapsed().as_secs_f64();
        let validation_started = std::time::Instant::now();
        let mut actual = 0;
        let mut pages = BTreeMap::<String, Vec<(String, FactRef)>>::new();
        tree::visit(&objects, &roots.facts, |key, r| {
            let r: FactRef = serde_json::from_value(r.clone())?;
            pages
                .entry(r.object.hash.clone())
                .or_default()
                .push((key.into(), r));
            Ok(())
        })
        .unwrap();
        let verified_payload_pages = pages.len();
        for rows in pages.into_values() {
            let bytes = objects.audit_bytes(&rows[0].1.object).unwrap();
            for (key, r) in rows {
                let value = decode_bytes(&bytes, &r).unwrap();
                if let Some(expected) = unique.get(&key) {
                    assert_eq!(value, *expected);
                    actual += 1;
                }
            }
        }
        assert_eq!(actual, unique.len());
        std::fs::write(
            report,
            serde_json::to_vec_pretty(&json!({
                "passed":true,"records":unique.len(),"workers":cq_core::compute::workers().unwrap(),
                "feed_seconds":feed,"finish_seconds":finish,"durable_seconds":duration,
                "roots":roots,"stats":stats,
                "objects":objects.resource_stats(),
                "embedding_model_configured":embedding_model_configured,"validation_seconds":validation_started.elapsed().as_secs_f64(),"verified_payload_pages":verified_payload_pages,
                "scope":"retained raw payloads, initial fact pages, configured embeddings and search memberships, directory merge, durable pack sync, every input payload checked in physical page order; excludes compiler, canonical identity, scope transfers and graph publication"
            })).unwrap(),
        ).unwrap();
    }
}

#[cfg(test)]
mod scoped_encoding_performance_tests {
    use super::*;
    use crate::analysis::encoding_reproduction::{load, progress};
    use std::time::Instant;

    fn closing_fixture(objects: &Objects, name: &str) -> ClosingScope {
        use cq_core::semantic::scope::{ContentRef, FactFamily, Receipt};
        let key = fact_key("body", name);
        let value = json!({"id":name,"owner":name});
        let bytes = serde_json::to_vec(&value).unwrap();
        let hash = digest(&bytes);
        let reference = FactRef {
            object: objects.put(&bytes).unwrap(),
            offset: 0,
            length: bytes.len() as u64,
            digest: hash.clone(),
        };
        let records = BTreeMap::from([(
            key.clone(),
            ContentRef {
                hash,
                bytes: bytes.len() as u64,
            },
        )]);
        let scope = ScopeKey {
            unit: name.into(),
            context: digest(name.as_bytes()),
            family: FactFamily::Body,
        };
        let descriptor = production::ScopeDescriptor {
            key: scope.clone(),
            input_witness: digest(b"input"),
            completion: Completion::Complete,
            rows: 1,
            digest: transfer::state_digest(&records).unwrap(),
        };
        let receipt = Receipt::new(scope, Completion::Complete, Output::Full(records)).unwrap();
        (
            Box::new(transfer::Begin {
                descriptor,
                base_revision: None,
                receipt,
            }),
            BTreeMap::from([(key, reference)]),
            1,
        )
    }

    #[test]
    fn completed_scope_jobs_preserve_the_current_member_map_and_durable_references() {
        let temp = tempfile::tempdir().unwrap();
        let objects = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut writer = Writer::initial(&objects).unwrap();
        let closing = closing_fixture(&objects, "earlier");
        let id = closing.0.descriptor.id().unwrap();
        let expected = closing.1.values().next().unwrap().clone();
        writer.queue_scope(closing).unwrap();
        writer.scope_facts = Some(BTreeMap::from([("body:later".into(), expected.clone())]));
        writer.join_scope().unwrap();
        assert_eq!(
            writer
                .scope_facts
                .as_ref()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["body:later"]
        );
        let (roots, stats) = writer.finish_inventory().unwrap();
        assert_eq!(stats.facts_changed, 1);
        objects.sync_pending().unwrap();
        let reopened = Objects::new(objects.root.clone()).unwrap();
        let r = reference(&reopened, &roots, "scope", &id, &mut ReadStats::default())
            .unwrap()
            .unwrap();
        let v = decode(&reopened, &r, &mut ReadStats::default()).unwrap();
        let members: Root = serde_json::from_value(v["members"].clone()).unwrap();
        assert_eq!(
            tree::get(
                &reopened,
                &members,
                "body:earlier",
                &mut ReadStats::default()
            )
            .unwrap(),
            Some(serde_json::to_value(expected).unwrap())
        );
    }

    #[test]
    fn failed_scope_seal_cannot_return_an_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let objects = Objects::buffered(temp.path().join("objects")).unwrap();
        for missing in [false, true] {
            let mut writer = Writer::initial(&objects).unwrap();
            let mut closing =
                closing_fixture(&objects, if missing { "missing" } else { "corrupt" });
            if missing {
                closing.1.clear();
            } else {
                closing.1.values_mut().next().unwrap().digest = digest(b"wrong");
            }
            writer.queue_scope(closing).unwrap();
            assert_eq!(
                writer.finish_inventory().unwrap_err().code(),
                if missing {
                    "MissingTransferredFact"
                } else {
                    "ScopeResultMismatch"
                }
            );
        }
    }

    #[test]
    fn encoding_queue_preserves_serial_pages_roots_and_tail_batches() {
        let temp = tempfile::tempdir().unwrap();
        let objects = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut asynchronous = Writer::initial(&objects).unwrap();
        let mut serial = Writer::initial(&objects).unwrap();
        serial.encoding_pending_limit = 0;
        let rows=(0..5217).map(|i|production::Record{key:format!("body:{i}"),
            value:json!({"id":i.to_string(),"owner":format!("f{i}"),"text":"unicode 路径\n\"quoted\""})}).collect::<Vec<_>>();
        for batch in rows.chunks(production::BATCH_ROWS) {
            asynchronous.append_owned_batch(batch.to_vec()).unwrap();
            serial.append_owned_batch(batch.to_vec()).unwrap();
            assert!(asynchronous.encoding_jobs.len() <= asynchronous.encoding_pending_limit);
        }
        assert!(asynchronous.encoding_submitted > 0);
        let (actual, stats) = asynchronous.finish_inventory().unwrap();
        let (expected, oracle_stats) = serial.finish_inventory().unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(stats.fact_pages_prepared, oracle_stats.fact_pages_prepared);
        assert_eq!(stats.inventory_bytes, oracle_stats.inventory_bytes);
        assert_eq!(stats.facts_changed, 5217);
        objects.sync_pending().unwrap();
        let reopened = Objects::new(objects.root.clone()).unwrap();
        let tail = reference(
            &reopened,
            &actual,
            "body",
            "5216",
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            decode(&reopened, &tail, &mut ReadStats::default()).unwrap(),
            rows.last().unwrap().value
        );
    }

    #[test]
    fn asynchronous_record_limit_failure_is_returned_before_inventory_publication() {
        let temp = tempfile::tempdir().unwrap();
        let objects = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut writer = Writer::initial(&objects).unwrap();
        writer.append_owned_batch(vec![production::Record{key:"document:too-large".into(),
            value:json!({"id":"too-large","text":"x".repeat(super::super::objects::MAX_OBJECT)})}]).unwrap();
        assert_eq!(
            writer.finish_inventory().unwrap_err().code(),
            "OversizedFact"
        );
    }

    fn cpu_seconds() -> f64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // getrusage initializes the entire output on success.
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
            0
        );
        let usage = unsafe { usage.assume_init() };
        usage.ru_utime.tv_sec as f64
            + usage.ru_utime.tv_usec as f64 / 1e6
            + usage.ru_stime.tv_sec as f64
            + usage.ru_stime.tv_usec as f64 / 1e6
    }

    struct Timed<S> {
        inner: S,
        begin_seconds: f64,
        batch_seconds: f64,
        end_seconds: f64,
        batches: u64,
        completed: u64,
        total: u64,
        tick: Instant,
    }
    impl<S: TransferSink> TransferSink for Timed<S> {
        fn begin(&mut self, begin: &transfer::Begin) -> Result<()> {
            let at = Instant::now();
            let result = self.inner.begin(begin);
            self.begin_seconds += at.elapsed().as_secs_f64();
            result
        }
        fn batch(
            &mut self,
            key: &ScopeKey,
            sequence: u64,
            records: &[production::Record],
        ) -> Result<()> {
            let at = Instant::now();
            let result = self.inner.batch(key, sequence, records);
            self.batch_seconds += at.elapsed().as_secs_f64();
            result?;
            self.batches += 1;
            self.completed += records.len() as u64;
            if self.tick.elapsed().as_millis() >= 500 {
                progress("encoding.scope_transfer", self.completed, self.total);
                self.tick = Instant::now();
            }
            Ok(())
        }
        fn batch_wave(
            &mut self,
            key: &ScopeKey,
            sequence: u64,
            batches: Vec<Vec<production::Record>>,
        ) -> Result<()> {
            let count = batches.iter().map(Vec::len).sum::<usize>() as u64;
            let n = batches.len() as u64;
            let at = Instant::now();
            let result = self.inner.batch_wave(key, sequence, batches);
            self.batch_seconds += at.elapsed().as_secs_f64();
            result?;
            self.batches += n;
            self.completed += count;
            if self.tick.elapsed().as_millis() >= 500 {
                progress("encoding.scope_transfer", self.completed, self.total);
                self.tick = Instant::now();
            }
            Ok(())
        }
        fn end(&mut self, key: &ScopeKey, batches: u64) -> Result<()> {
            let at = Instant::now();
            let result = self.inner.end(key, batches);
            self.end_seconds += at.elapsed().as_secs_f64();
            result
        }
        fn batch_owned(
            &mut self,
            key: &ScopeKey,
            sequence: u64,
            records: Vec<production::Record>,
        ) -> Result<()> {
            let count = records.len() as u64;
            let at = Instant::now();
            let result = self.inner.batch_owned(key, sequence, records);
            self.batch_seconds += at.elapsed().as_secs_f64();
            result?;
            self.batches += 1;
            self.completed += count;
            if self.tick.elapsed().as_millis() >= 500 {
                progress("encoding.scope_transfer", self.completed, self.total);
                self.tick = Instant::now();
            }
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.inner.finish()
        }
    }

    #[test]
    #[ignore = "manual real scoped encoding replay: CQ_ENCODING_FIXTURE, CQ_ENCODING_REPORT, CQ_TOKEN_VECTORS"]
    fn retained_scopes_reproduce_production_encoding() {
        let setup = Instant::now();
        let (fixture, header, spool) = load();
        let temp = tempfile::tempdir().unwrap();
        let local = crate::LocalStore::open(&temp.path().join("store")).unwrap();
        let base = crate::analysis::ScopeBase::open(&local, &header.project, None).unwrap();
        let objects = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut writer = Writer::initial(&objects).unwrap();
        writer.indexed = true;
        writer.vector_model = cq_core::query::embeddings::TokenVectors::configured().unwrap();
        assert_eq!(writer.vector_model.is_some(), writer.vector_enabled);
        let setup_seconds = setup.elapsed().as_secs_f64();
        let cpu = cpu_seconds();
        let started = Instant::now();
        progress("encoding.scope_transfer", 0, fixture.records.len() as u64);
        let sink = ScopedSink {
            writer: &mut writer,
            base: &base,
            validator: transfer::Validator::new(header.production.as_ref().unwrap(), &base)
                .unwrap(),
            current: None,
            compact: false,
            aggregate: None,
            spool: &spool,
            stats: json!({"full_scopes":0,"delta_scopes":0,"reused_scopes":0,"payload_rows_submitted":0}),
        };
        let mut timing = Timed {
            inner: sink,
            begin_seconds: 0.,
            batch_seconds: 0.,
            end_seconds: 0.,
            batches: 0,
            completed: 0,
            total: fixture.records.len() as u64,
            tick: Instant::now(),
        };
        spool.emit(&header, &base, &mut timing).unwrap();
        assert_eq!(timing.completed, fixture.records.len() as u64);
        let feed = started.elapsed().as_secs_f64();
        let feed_cpu = cpu_seconds() - cpu;
        let encoding_pipeline = json!({"submitted":timing.inner.writer.encoding_submitted,
            "maximum_pending":timing.inner.writer.encoding_maximum_pending,
            "pending_after_transfer":timing.inner.writer.encoding_jobs.len(),
            "pending_limit":timing.inner.writer.encoding_pending_limit});
        assert!(timing.inner.writer.encoding_jobs.is_empty());
        let stages = json!({"begin_seconds":timing.begin_seconds,"batch_seconds":timing.batch_seconds,
            "scope_end_seconds":timing.end_seconds,"spool_plan_read_decode_seconds":(feed-timing.begin_seconds-timing.batch_seconds-timing.end_seconds).max(0.),
            "batches":timing.batches,"transfer":timing.inner.stats});
        drop(timing);
        progress("encoding.directory_finalize", 0, 1);
        let (roots, stats) = writer.finish_inventory().unwrap();
        let finish = started.elapsed().as_secs_f64() - feed;
        progress("encoding.durable_sync", 0, 1);
        objects.sync_pending().unwrap();
        let durable = started.elapsed().as_secs_f64();
        let total_cpu = cpu_seconds() - cpu;
        progress(
            "encoding.verify_every_record",
            0,
            fixture.records.len() as u64,
        );
        let validation = Instant::now();
        let expected = fixture
            .records
            .iter()
            .map(|r| (r.key.as_str(), &r.reference))
            .collect::<BTreeMap<_, _>>();
        let mut found = BTreeSet::new();
        let mut actual_counts = BTreeMap::<String, u64>::new();
        let mut pages = BTreeMap::<String, Vec<FactRef>>::new();
        tree::visit(&objects, &roots.facts, |key, value| {
            let reference: FactRef = serde_json::from_value(value.clone())?;
            if let Some(expected) = expected.get(key) {
                assert_eq!(reference.digest, expected.hash, "{key}");
                assert_eq!(reference.length, expected.bytes, "{key}");
                assert!(found.insert(key.to_owned()));
            }
            *actual_counts
                .entry(key.split_once(':').unwrap().0.into())
                .or_default() += 1;
            pages
                .entry(reference.object.hash.clone())
                .or_default()
                .push(reference);
            Ok(())
        })
        .unwrap();
        assert_eq!(found.len(), fixture.records.len());
        for (family, count) in &fixture.counts {
            assert_eq!(actual_counts[family], *count);
        }
        assert_eq!(
            actual_counts.get("entity_vector").copied().unwrap_or(0),
            if cq_core::query::embeddings::enabled() {
                fixture.counts["entity"]
            } else {
                0
            }
        );
        assert_eq!(actual_counts["scope"], fixture.scopes.len() as u64);
        let verified_payload_pages = pages.len();
        for refs in pages.into_values() {
            let bytes = objects.audit_bytes(&refs[0].object).unwrap();
            for reference in refs {
                record_bytes(&bytes, &reference).unwrap();
            }
        }
        let report = json!({"passed":true,"records":fixture.records.len(),"scopes":fixture.scopes.len(),
            "counts":fixture.counts,"workers":cq_core::compute::workers().unwrap(),"setup_seconds":setup_seconds,
            "feed_seconds":feed,"feed_cpu_seconds":feed_cpu,"feed_average_cpu_cores":feed_cpu/feed,
            "feed_records_per_second":fixture.records.len() as f64/feed,"timings":stages,"encoding_pipeline":encoding_pipeline,
            "finish_seconds":finish,"durable_seconds":durable,"durable_cpu_seconds":total_cpu,
            "roots":roots,"stats":stats,"objects":objects.resource_stats(),"actual_counts":actual_counts,
            "verified_payload_pages":verified_payload_pages,"validation_seconds":validation.elapsed().as_secs_f64(),
            "fixture":std::env::var("CQ_ENCODING_FIXTURE").unwrap(),"ownership_revision":fixture.ownership_revision,
            "selection":fixture.selection,"namespace_rebindings":fixture.namespace_rebindings,
            "scope":"original complete Brocade scopes, exact payload offsets in the current run's retained canonical spool, production Spool::emit and ScopedSink, vectors and all index memberships, scope sealing, full directory finalization and durable sync; fixture setup and validation excluded; smaller selection does not reproduce full-project metadata/cache pressure"});
        std::fs::write(
            std::env::var("CQ_ENCODING_REPORT").unwrap(),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        progress(
            "encoding.complete",
            fixture.records.len() as u64,
            fixture.records.len() as u64,
        );
    }
}
