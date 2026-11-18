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
use std::collections::{BTreeMap, BTreeSet};

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
    Ok(serde_json::from_slice(row)?)
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
    pub roots: Roots,
    pub stats: WriteStats,
    seen: BTreeSet<String>,
    pending: Vec<(String, String, Value, Vec<u8>)>,
    pending_bytes: usize,
    page_bytes: usize,
    page_rows: usize,
    read: ReadStats,
    audited: BTreeSet<String>,
    diagnostic_header: Option<SemanticSnapshot>,
    preserve_terms: bool,
    vector_model: Option<std::sync::Arc<cq_core::query::embeddings::TokenVectors>>,
    indexed: bool,
    initial_groups: Option<super::bulk::Builder>,
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
        self.validator.batch(key, n, records)?;
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
            self.writer.append(family, id, &r.value)?;
        }
        self.stats["payload_rows_submitted"] = json!(
            self.stats["payload_rows_submitted"].as_u64().unwrap_or(0) + records.len() as u64
        );
        Ok(())
    }
    fn end(&mut self, key: &ScopeKey, n: u64) -> Result<()> {
        self.validator.end(key, n)?;
        self.writer.flush()?;
        let (b, mut root) = self
            .current
            .take()
            .ok_or_else(|| Error::new("MissingScopeBegin", "end without begin"))?;
        if self.compact {
            let state_base = self.base.summary(key)?.unwrap().base_root;
            self.writer.stats.facts_reused += b.descriptor.rows;
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
            let f = reference(
                self.writer.o,
                &self.writer.roots,
                family,
                id,
                &mut self.writer.read,
            )?
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
        self.validator.finish()
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
            &base.groups,
            "all:entity_vector",
            &mut ReadStats::default(),
        )
        .is_ok_and(|v| v.is_some());
        Self {
            o,
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
            indexed,
            initial_groups: None,
        }
    }
    pub fn append<T: Serialize>(&mut self, family: &str, id: &str, value: &T) -> Result<()> {
        if family == "entity" && self.indexed {
            let value = serde_json::to_value(value)?;
            if let Ok(e) = serde_json::from_value::<Entity>(value) {
                let index =
                    cq_core::query::search::SearchIndex::with_model(&[], self.vector_model.clone());
                let terms = cq_core::query::search::terms(&e);
                let vector = index.prepare_vector(&terms[..terms.len().min(4096)]);
                let buckets = cq_core::query::search::vector_buckets(&vector);
                self.append("entity_vector", id, &json!({"entity":e,"vector":vector,"buckets":buckets,"model":index.model_name()}))?;
            }
        }
        let key = fact_key(family, id);
        if !self.seen.insert(key.clone()) {
            return Err(Error::new("DuplicateFact", key));
        }
        let bytes = serde_json::to_vec(value)?;
        self.stats.inventory_bytes += bytes.len() as u64;
        if bytes.len() > super::objects::MAX_OBJECT {
            let value = serde_json::from_slice(&bytes)?;
            return Err(super::limits::report(
                super::limits::root(&self.o.root),
                family,
                id,
                &value,
                bytes.len(),
                self.diagnostic_header.as_ref(),
            )?);
        }
        if let Some(r) = reference(self.o, &self.roots, family, id, &mut self.read)?
            && r.digest == digest(&bytes)
        {
            if self.audited.insert(r.object.hash.clone()) {
                self.o.audit(&r.object)?;
            }
            self.stats.facts_reused += 1;
            return Ok(());
        }
        let value = serde_json::from_slice(&bytes)?;
        if self.pending_bytes + bytes.len() > self.page_bytes
            || self.pending.len() >= self.page_rows
        {
            self.flush()?;
        }
        self.pending_bytes += bytes.len() + 1;
        self.pending.push((family.into(), id.into(), value, bytes));
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(self.pending_bytes);
        let mut offsets = vec![];
        for (_, _, _, row) in &self.pending {
            let offset = bytes.len();
            bytes.extend_from_slice(row);
            offsets.push((offset, row.len(), digest(row)));
            bytes.push(b'\n');
        }
        let object = self.o.put(&bytes)?;
        self.stats.fact_pages_prepared += 1;
        self.stats.fact_bytes_prepared += bytes.len() as u64;
        let rows = std::mem::take(&mut self.pending)
            .into_iter()
            .zip(offsets)
            .map(|((family, id, value, _), (offset, length, hash))| {
                (
                    family,
                    id,
                    value,
                    FactRef {
                        object: object.clone(),
                        offset: offset as u64,
                        length: length as u64,
                        digest: hash,
                    },
                )
            })
            .collect();
        self.pending_bytes = 0;
        self.commit_rows(rows)
    }
    fn commit_rows(&mut self, rows: Vec<(String, String, Value, FactRef)>) -> Result<()> {
        let mut memberships_by_group = GroupChanges::new();
        let mut facts = vec![];
        for (family, id, value, r) in rows {
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
            for g in cq_core::ports::facts::groups_indexed(&family, &value, self.indexed) {
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
    write(o, blobs, s, prior, None)
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
    )
}
fn write(
    o: &Objects,
    blobs: &crate::content::BlobStore,
    s: &SemanticSnapshot,
    prior: Option<&Revision>,
    prepared: Option<&crate::analysis::Prepared>,
) -> Result<Revision> {
    let mut w = Writer::new(o, prior.map(|r| r.roots.clone()).unwrap_or_default());
    if prior.is_none() {
        w.initial_groups = Some(super::bulk::Builder::new(&o.root)?);
    }
    w.vector_model = cq_core::query::embeddings::TokenVectors::configured()?;
    w.indexed = prior.is_none_or(|r| r.metadata["search_index"]["version"] == 1);
    let vector_model =
        cq_core::query::search::SearchIndex::with_model(&[], w.vector_model.clone()).model_name();
    if prior.is_some_and(|r| {
        r.metadata["search_index"]["version"] == 1
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
        prepared.map(|p| p.spool.statistics(s)).transpose()?
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
            let frozen = Source::freeze(o, &f.blob, std::fs::File::open(blobs.path(&f.blob)?)?)?;
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
    let vector_model =
        cq_core::query::search::SearchIndex::with_model(&[], w.vector_model.clone()).model_name();
    let (mut roots, stats) = if prepared.is_some() {
        w.finish_scoped()?
    } else {
        w.finish_inventory()?
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
    meta["search_index"] = json!({"version":if indexed{1}else{0},"vector_model":vector_model,"upgrade_required":!indexed,"ranking_window":1000});
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
