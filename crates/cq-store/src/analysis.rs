//! Storage-owned normalized-record spool. Only compact identities/offsets are
//! held in memory; producer payloads are delivered as actual sparse transfers.
use crate::{
    LocalStore,
    facts::{
        objects::Objects,
        tree::{self, Root},
        writer::{self, Group, Revision},
    },
};
use cq_core::{
    Error, Result,
    model::*,
    ports::facts::{FactRef, ReadStats},
    semantic::{
        normalized,
        production::{self, ScopeDescriptor},
        scope::*,
        transfer::{self, Base},
    },
};
use serde::ser::{Serialize, SerializeSeq, SerializeStruct, Serializer};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};
#[derive(Clone)]
struct Entry {
    offset: u64,
    length: u64,
    reference: ContentRef,
    scope: Arc<ScopeKey>,
    shard: Arc<str>,
}
pub struct Spool {
    diagnostic_header: SemanticSnapshot,
    diagnostic_root: Option<std::path::PathBuf>,
    file: Mutex<tempfile::NamedTempFile>,
    entries: BTreeMap<Arc<str>, Entry>,
    units: BTreeSet<String>,
    pub universe: String,
    pub sources: BTreeMap<String, Option<FileRevision>>,
    by_shard: BTreeMap<(Arc<str>, String), Vec<Arc<str>>>,
    by_scope: BTreeMap<ScopeKey, Vec<Arc<str>>>,
    scope_intern: BTreeMap<ScopeKey, Arc<ScopeKey>>,
    shard_intern: BTreeMap<String, Arc<str>>,
}
pub struct ScopeBase {
    objects: Objects,
    revision: Option<Revision>,
    _pin: Option<Box<dyn cq_core::ports::facts::FactReader>>,
}
impl ScopeBase {
    pub fn open(store: &LocalStore, project: &str, revision: Option<&str>) -> Result<Self> {
        let (r, pin) = if let Some(id) = revision {
            (
                Some(store.paged_manifest(project, id)?),
                Some(Box::new(store.open_facts(project, Some(id))?)
                    as Box<dyn cq_core::ports::facts::FactReader>),
            )
        } else {
            (None, None)
        };
        Ok(Self {
            objects: Objects::new(store.root.join("format-2/objects"))?,
            revision: r,
            _pin: pin,
        })
    }
    /// Embed the same store contract without a second IPC owner. The execution
    /// reader is retained as the base revision's GC/epoch protection.
    pub fn from_view(
        store: &LocalStore,
        view: Box<dyn cq_core::ports::facts::FactReader>,
    ) -> Result<Self> {
        let r = store.paged_manifest(view.project(), view.revision())?;
        Ok(Self {
            objects: Objects::new(store.root.join("format-2/objects"))?,
            revision: Some(r),
            _pin: Some(view),
        })
    }
    pub fn manifest(&self) -> Option<&Revision> {
        self.revision.as_ref()
    }
    pub fn stored(&self, key: &ScopeKey) -> Result<Option<Value>> {
        let Some(r) = &self.revision else {
            return Ok(None);
        };
        let id = digest(&serde_json::to_vec(key)?);
        writer::reference(
            &self.objects,
            &r.roots,
            "scope",
            &id,
            &mut ReadStats::default(),
        )?
        .map(|f| writer::decode(&self.objects, &f, &mut ReadStats::default()))
        .transpose()
    }
}
impl Base for ScopeBase {
    fn summary(&self, key: &ScopeKey) -> Result<Option<transfer::BaseSummary>> {
        let Some(v) = self.stored(key)? else {
            return Ok(None);
        };
        let Some(root) = v["state_base"]
            .as_str()
            .filter(|s| crate::facts::objects::valid_hash(s))
        else {
            return Ok(None);
        };
        if v.get("members").is_none() {
            return Ok(None);
        }
        Ok(Some(transfer::BaseSummary {
            descriptor: serde_json::from_value(v["descriptor"].clone())?,
            base_root: root.into(),
        }))
    }
    fn revision(&self) -> Option<&str> {
        self.revision.as_ref().map(|r| r.revision.as_str())
    }
    fn keys(&self) -> Result<Vec<ScopeKey>> {
        let Some(r) = &self.revision else {
            return Ok(vec![]);
        };
        let mut out = vec![];
        let group = tree::get(
            &self.objects,
            &r.roots.groups,
            "all:scope",
            &mut ReadStats::default(),
        )?
        .map(serde_json::from_value::<Group>)
        .transpose()?
        .unwrap_or_default();
        tree::visit(&self.objects, &group.root, |_, v| {
            let row = writer::decode(
                &self.objects,
                &serde_json::from_value(v.clone())?,
                &mut ReadStats::default(),
            )?;
            let descriptor: ScopeDescriptor = serde_json::from_value(row["descriptor"].clone())?;
            if descriptor.rows > 0 {
                out.push(descriptor.key);
            }
            Ok(())
        })?;
        Ok(out)
    }
    fn scope(&self, key: &ScopeKey) -> Result<Option<transfer::BaseScope>> {
        let Some(v) = self.stored(key)? else {
            return Ok(None);
        };
        if v.get("members").is_none() {
            return Ok(None);
        } // explicit full upgrade, never guess a sparse base
        let descriptor: ScopeDescriptor = serde_json::from_value(v["descriptor"].clone())?;
        let root: Root = serde_json::from_value(v["members"].clone())?;
        let mut records = BTreeMap::new();
        tree::visit(&self.objects, &root, |key, v| {
            let f: FactRef = serde_json::from_value(v.clone())?;
            let (family, id) = key
                .split_once(':')
                .ok_or_else(|| Error::new("InvalidFactKey", key))?;
            let actual = writer::reference(
                &self.objects,
                &self.revision.as_ref().unwrap().roots,
                family,
                id,
                &mut ReadStats::default(),
            )?
            .ok_or_else(|| Error::new("InvalidScopeBase", "member absent from base revision"))?;
            if serde_json::to_value(&f)? != serde_json::to_value(&actual)? {
                return Err(Error::new(
                    "InvalidScopeBase",
                    "scope member does not bind the revision fact",
                ));
            }
            records.insert(
                key.into(),
                ContentRef {
                    hash: f.digest,
                    bytes: f.length,
                },
            );
            Ok(())
        })?;
        if descriptor.digest != transfer::state_digest(&records)?
            || descriptor.rows != records.len() as u64
        {
            return Err(Error::new(
                "InvalidScopeBase",
                "scope descriptor does not cover its member tree",
            ));
        }
        Ok(Some(transfer::BaseScope {
            descriptor,
            records,
        }))
    }
}
pub struct Prepared {
    pub header: SemanticSnapshot,
    pub spool: Spool,
    pub base: ScopeBase,
    pub generation: Value,
}
pub struct Statistics {
    pub counts: Value,
    pub entity_kinds: BTreeMap<String, u64>,
    pub relation_kinds: BTreeMap<String, u64>,
    pub df: BTreeMap<String, u64>,
    pub terms: u64,
    pub contexts: BTreeMap<String, Value>,
}
fn fact_family(name: &str) -> Result<FactFamily> {
    Ok(match name {
        "entity" => FactFamily::Entity,
        "body" => FactFamily::Body,
        "occurrence" => FactFamily::Occurrence,
        "relation" => FactFamily::Relation,
        "document" => FactFamily::Document,
        _ => return Err(Error::new("InvalidFactFamily", name)),
    })
}
impl Spool {
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    pub fn statistics(&self, header: &SemanticSnapshot) -> Result<Statistics> {
        #[derive(Default)]
        struct ContextCounts {
            entities: u64,
            relations: u64,
            occurrences: u64,
            documents: u64,
            ek: BTreeMap<String, u64>,
            rk: BTreeMap<String, u64>,
        }
        let mut context_counts = (0..header.contexts.len())
            .map(|_| ContextCounts::default())
            .collect::<Vec<_>>();
        let selected = header
            .contexts
            .iter()
            .map(|c| c.units.iter().cloned().collect::<BTreeSet<_>>())
            .collect::<Vec<_>>();
        let mut contexts_by_unit = BTreeMap::<String, Vec<usize>>::new();
        for (i, units) in selected.iter().enumerate() {
            for unit in units {
                contexts_by_unit.entry(unit.clone()).or_default().push(i);
            }
        }
        let mut counts = self.counts();
        for (key, n) in [
            ("contexts", header.contexts.len()),
            ("units", header.units.len()),
            ("coverage", header.coverage.len()),
            ("gaps", header.gaps.len()),
        ] {
            counts[key] = json!(n);
        }
        let mut kinds = BTreeMap::new();
        let mut relations = BTreeMap::new();
        let mut df = BTreeMap::new();
        let mut terms = 0;
        let mut external = 0;
        let mut units = std::collections::HashMap::<String, String>::new();
        self.map_each(
            "entity",
            |_, v| {
                let e: Entity = serde_json::from_value(v)?;
                let words = cq_core::query::search::terms(&e);
                Ok((e.id, e.unit, format!("{:?}", e.kind), e.external, words))
            },
            |_, (id, unit, kind, is_external, words)| {
                *kinds.entry(kind.clone()).or_insert(0) += 1;
                external += u64::from(is_external);
                for i in contexts_by_unit.get(&unit).into_iter().flatten() {
                    let c = &mut context_counts[*i];
                    c.entities += 1;
                    *c.ek.entry(kind.clone()).or_default() += 1;
                }
                units.insert(id, unit);
                terms += words.len() as u64;
                for word in words.into_iter().collect::<BTreeSet<_>>() {
                    *df.entry(word).or_insert(0) += 1;
                }
                Ok(())
            },
        )?;
        self.each("relation", |_, v| {
            let kind: RelationKind = serde_json::from_value(v["kind"].clone())?;
            *relations.entry(kind.label().to_owned()).or_insert(0) += 1;
            if let (Some(from), Some(to)) = (
                v["from"].as_str().and_then(|id| units.get(id)),
                v["to"].as_str().and_then(|id| units.get(id)),
            ) {
                for i in contexts_by_unit.get(from).into_iter().flatten() {
                    if selected[*i].contains(to) {
                        let c = &mut context_counts[*i];
                        c.relations += 1;
                        *c.rk.entry(kind.label().into()).or_default() += 1;
                    }
                }
            }
            Ok(())
        })?;
        let mut unresolved = 0;
        self.each("occurrence", |_, v| {
            if v["kind"] == "Calls" && v["executable"]["state"] != "Resolved" {
                unresolved += 1;
            }
            if let Some(unit) = v["unit"].as_str() {
                for i in contexts_by_unit.get(unit).into_iter().flatten() {
                    context_counts[*i].occurrences += 1;
                }
            }
            Ok(())
        })?;
        self.each("document", |_, v| {
            if let Some(unit) = v["owner"].as_str().and_then(|id| units.get(id)) {
                for i in contexts_by_unit.get(unit).into_iter().flatten() {
                    context_counts[*i].documents += 1;
                }
            }
            Ok(())
        })?;
        counts["external"] = json!(external);
        counts["unresolved_calls"] = json!(unresolved);
        let contexts=header.contexts.iter().zip(context_counts).map(|(context,c)| (context.id.clone(),json!({"project":header.project,"counts":{"entities":c.entities,"relations":c.relations,"occurrences":c.occurrences,"documents":c.documents,"contexts":1},"entity_kinds":c.ek,"relation_kinds":c.rk}))).collect();
        Ok(Statistics {
            counts,
            entity_kinds: kinds,
            relation_kinds: relations,
            df,
            terms,
            contexts,
        })
    }
    pub fn new(header: &SemanticSnapshot) -> Result<Self> {
        Ok(Self {
            diagnostic_header: header.metadata(),
            diagnostic_root: None,
            file: Mutex::new(tempfile::NamedTempFile::new()?),
            entries: BTreeMap::new(),
            units: cq_core::semantic::identity::compilation_identities(
                &header.units,
                &header.root.display(),
            )?
            .units
            .into_values()
            .collect(),
            universe: String::new(),
            sources: BTreeMap::new(),
            by_shard: BTreeMap::new(),
            by_scope: BTreeMap::new(),
            scope_intern: BTreeMap::new(),
            shard_intern: BTreeMap::new(),
        })
    }
    fn encode(
        &self,
        unit: String,
        shard: String,
        record: normalized::Record,
    ) -> Result<(String, String, String, Vec<u8>, ContentRef)> {
        let family = record.family();
        let key = cq_core::ports::facts::fact_key(family, record.id());
        let bytes = serde_json::to_vec(&record.value()?)?;
        if bytes.len() > production::RECORD_BYTES {
            return Err(crate::facts::limits::report(
                self.diagnostic_root.as_deref(),
                family,
                record.id(),
                &record.value()?,
                bytes.len(),
                Some(&self.diagnostic_header),
            )?);
        }
        let reference = ContentRef {
            hash: digest(&bytes),
            bytes: bytes.len() as u64,
        };
        Ok((unit, shard, key, bytes, reference))
    }
    fn insert_batch(&mut self, records: Vec<(String, String, normalized::Record)>) -> Result<()> {
        let encoded =
            cq_core::compute::map(records, |(unit, shard, r)| self.encode(unit, shard, r))?;
        let file = self
            .file
            .get_mut()
            .map_err(|_| Error::new("SpoolPoisoned", "record spool poisoned"))?;
        let start = file.as_file_mut().seek(SeekFrom::End(0))?;
        let mut bytes = Vec::new();
        let mut pending = BTreeMap::<Arc<str>, Entry>::new();
        for (unit, shard, key, row, reference) in encoded {
            let unit = if self.units.contains(&unit) || unit == "external" {
                unit
            } else {
                "$project".into()
            };
            let scope = ScopeKey {
                context: digest(unit.as_bytes()),
                unit,
                family: fact_family(key.split(':').next().unwrap())?,
            };
            if let Some(old) = pending
                .get(key.as_str())
                .or_else(|| self.entries.get(key.as_str()))
            {
                if old.reference != reference
                    || old.scope.as_ref() != &scope
                    || old.shard.as_ref() != shard
                {
                    return Err(Error::new("ConflictingSemanticIdentity", key));
                }
                continue;
            }
            let scope = self
                .scope_intern
                .entry(scope.clone())
                .or_insert_with(|| Arc::new(scope))
                .clone();
            let shard = self
                .shard_intern
                .entry(shard.clone())
                .or_insert_with(|| Arc::from(shard))
                .clone();
            let key: Arc<str> = Arc::from(key);
            pending.insert(
                key,
                Entry {
                    offset: start + bytes.len() as u64,
                    length: row.len() as u64,
                    reference,
                    scope,
                    shard,
                },
            );
            bytes.extend_from_slice(&row);
        }
        file.as_file_mut().write_all(&bytes)?;
        for (key, e) in pending {
            self.by_scope
                .entry(e.scope.as_ref().clone())
                .or_default()
                .push(key.clone());
            self.entries.insert(key, e);
        }
        Ok(())
    }
    fn insert(&mut self, unit: &str, shard: &str, record: normalized::Record) -> Result<()> {
        self.insert_batch(vec![(unit.into(), shard.into(), record)])
    }
    pub fn read(&self, key: &str) -> Result<Value> {
        Ok(self.read_many(&[key])?.pop().unwrap())
    }
    fn read_many(&self, keys: &[&str]) -> Result<Vec<Value>> {
        let entries = keys
            .iter()
            .map(|k| {
                self.entries
                    .get(*k)
                    .ok_or_else(|| Error::new("MissingSpoolRecord", *k))
            })
            .collect::<Result<Vec<_>>>()?;
        let spans = entries
            .iter()
            .map(|e| crate::disk_batch::Span {
                offset: e.offset,
                bytes: e.length,
            })
            .collect::<Vec<_>>();
        let file = self
            .file
            .lock()
            .map_err(|_| Error::new("SpoolPoisoned", "record spool poisoned"))?
            .as_file()
            .try_clone()?;
        crate::disk_batch::decode(&file, &spans, |i, b| {
            if digest(b) != entries[i].reference.hash {
                return Err(Error::new("DigestMismatch", "normalized spool record"));
            }
            Ok(serde_json::from_slice(b)?)
        })
    }
    pub fn finish(
        &mut self,
        header: &mut SemanticSnapshot,
        projection: normalized::Projection,
        base: &dyn Base,
    ) -> Result<()> {
        self.universe = projection.universe;
        self.sources = projection.sources;
        let mut progress = self.diagnostic_root.as_ref().map(|root| {
            cq_core::progress::Progress::stage(
                root,
                &header.project,
                "prepare.directories",
                "整理分片成员目录",
                Some(self.entries.len() as u64),
                "事实",
            )
        });
        let mut completed = 0;
        for (key, e) in &self.entries {
            self.by_shard
                .entry((e.shard.clone(), key.split(':').next().unwrap().into()))
                .or_default()
                .push(key.clone());
            completed += 1;
            if let Some(p) = &mut progress {
                p.count(completed);
            }
        }
        let mut scopes = self.by_scope.keys().cloned().collect::<BTreeSet<_>>();
        for unit in self
            .units
            .iter()
            .map(String::as_str)
            .chain(["external", "$project"])
        {
            for family in [
                FactFamily::Entity,
                FactFamily::Body,
                FactFamily::Occurrence,
                FactFamily::Relation,
                FactFamily::Document,
            ] {
                scopes.insert(ScopeKey {
                    unit: unit.into(),
                    context: digest(unit.as_bytes()),
                    family,
                });
            }
        }
        let groups = std::mem::take(&mut self.by_scope)
            .into_iter()
            .collect::<Vec<_>>();
        self.by_scope = cq_core::compute::map(groups, |(k, mut v)| {
            cq_core::compute::sort_by(&mut v, |a, b| a.cmp(b))?;
            Ok((k, v))
        })?
        .into_iter()
        .collect();
        let p = header.production.as_mut().ok_or_else(|| {
            Error::new(
                "MissingProduction",
                "normalization omitted production metadata",
            )
        })?;
        p.retired.clear();
        for old in base.keys()? {
            if scopes.insert(old.clone()) {
                p.retired.push(old.clone());
                if !p.selection_complete {
                    p.failed_units.insert(
                        old.unit.clone(),
                        "previous scope not reverified by incomplete selection".into(),
                    );
                }
            }
        }
        let inputs = digest(&serde_json::to_vec(&(
            &header.working_tree_manifest,
            &header.analysis_inputs,
            &header.analysis_directories,
            header
                .providers
                .iter()
                .map(|p| (&p.name, &p.version, &p.binary_digest))
                .collect::<Vec<_>>(),
            &header.analysis_request,
        ))?);
        let scope_progress = self.diagnostic_root.as_ref().map(|root| {
            Mutex::new((
                0u64,
                cq_core::progress::Progress::stage(
                    root,
                    &header.project,
                    "prepare.scopes",
                    "校验范围与封存",
                    Some(scopes.len() as u64),
                    "范围",
                ),
            ))
        });
        let failed = &p.failed_units;
        p.scopes = cq_core::compute::map(scopes.into_iter().collect(), |key| {
            let rows = self.by_scope.get(&key).map(Vec::as_slice).unwrap_or(&[]);
            let completion = if failed.contains_key(&key.unit) {
                Completion::Failed
            } else {
                Completion::Complete
            };
            if completion != Completion::Complete && !rows.is_empty() {
                return Err(Error::new(
                    "InvalidScopeReceipt",
                    "failed scope emitted facts",
                ));
            }
            let descriptor = ScopeDescriptor {
                input_witness: digest(&serde_json::to_vec(&(&key, &inputs))?),
                digest: transfer::state_digest_sorted(
                    rows.iter()
                        .map(|k| (k.as_ref(), &self.entries[k].reference)),
                )?,
                rows: rows.len() as u64,
                key,
                completion,
            };
            if let Some(p) = &scope_progress {
                let mut p = p.lock().unwrap();
                p.0 += 1;
                let n = p.0;
                p.1.count(n);
            }
            Ok(descriptor)
        })?;
        if p.transport.contains_key("record_source") {
            p.native_boundary="compiler frontends retain language state; native export arrays are decoded record by record; source ownership frames and a disk journal replace the project semantic snapshot; identity/range catalogues remain global; publication consumes actual full/delta/reuse transfers".into();
        }
        header.seal()?;
        Ok(())
    }
    pub fn emit(
        &self,
        header: &SemanticSnapshot,
        base: &dyn Base,
        sink: &mut dyn transfer::Sink,
    ) -> Result<()> {
        for descriptor in &header.production.as_ref().unwrap().scopes {
            let begin = if let Some(begin) = transfer::unchanged_plan(descriptor, base)? {
                begin
            } else {
                let current = self
                    .by_scope
                    .get(&descriptor.key)
                    .into_iter()
                    .flatten()
                    .map(|k| (k.to_string(), self.entries[k].reference.clone()))
                    .collect::<BTreeMap<_, _>>();
                transfer::plan(descriptor, current, base)?
            };
            let keys = if descriptor.completion != Completion::Complete {
                vec![]
            } else {
                match &begin.receipt.output {
                    Output::Full(r) => r.keys().cloned().collect(),
                    Output::Delta { upserts, .. } => upserts.keys().cloned().collect(),
                    Output::Reuse { .. } => vec![],
                }
            };
            sink.begin(&begin)?;
            let mut at = 0;
            let mut sequence = 0;
            while at < keys.len() {
                let first = at;
                let mut bytes = 0;
                while at < keys.len() && at - first < production::BATCH_ROWS {
                    let length = self.entries[keys[at].as_str()].length;
                    if at > first && bytes + length > production::BATCH_BYTES as u64 {
                        break;
                    }
                    bytes += length;
                    at += 1;
                }
                let requested = keys[first..at]
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                let values = self.read_many(&requested)?;
                let batch = keys[first..at]
                    .iter()
                    .cloned()
                    .zip(values)
                    .map(|(key, value)| production::Record { key, value })
                    .collect::<Vec<_>>();
                sink.batch(&descriptor.key, sequence, &batch)?;
                sequence += 1;
            }
            sink.end(&descriptor.key, sequence)?;
        }
        sink.finish()
    }
    pub fn each(&self, family: &str, mut f: impl FnMut(&str, Value) -> Result<()>) -> Result<()> {
        self.map_each(family, |_, v| Ok(v), |k, v| f(k, v))
    }
    fn map_each<T: Send>(
        &self,
        family: &str,
        transform: impl Fn(&str, Value) -> Result<T> + Send + Sync,
        mut consume: impl FnMut(&str, T) -> Result<()>,
    ) -> Result<()> {
        use std::ops::Bound::{Included, Unbounded};
        let prefix = format!("{family}:");
        let mut entries = self
            .entries
            .range::<str, _>((Included(prefix.as_str()), Unbounded))
            .take_while(|(k, _)| k.starts_with(&prefix))
            .collect::<Vec<_>>();
        cq_core::compute::sort_by(&mut entries, |a, b| a.1.offset.cmp(&b.1.offset))?;
        let mut progress = self.diagnostic_root.as_ref().map(|root| {
            let phase = format!("statistics.{family}");
            let mut p = cq_core::progress::Progress::stage(
                root,
                &self.diagnostic_header.project,
                &phase,
                "统计事实与检索词",
                Some(entries.len() as u64),
                "事实",
            );
            p.detail(
                &phase,
                "统计事实与检索词",
                0,
                Some(entries.len() as u64),
                "事实",
                cq_core::compute::workers().unwrap_or(1) as u64,
            );
            p
        });
        let mut at = 0;
        while at < entries.len() {
            let first = at;
            let mut bytes = 0;
            while at < entries.len() && at - first < 4096 {
                if at > first && bytes + entries[at].1.length > crate::disk_batch::BATCH_BYTES {
                    break;
                }
                bytes += entries[at].1.length;
                at += 1;
            }
            let part = &entries[first..at];
            let spans = part
                .iter()
                .map(|(_, e)| crate::disk_batch::Span {
                    offset: e.offset,
                    bytes: e.length,
                })
                .collect::<Vec<_>>();
            let file = self
                .file
                .lock()
                .map_err(|_| Error::new("SpoolPoisoned", "record spool poisoned"))?
                .as_file()
                .try_clone()?;
            let values = crate::disk_batch::decode(&file, &spans, |i, b| {
                if digest(b) != part[i].1.reference.hash {
                    return Err(Error::new("DigestMismatch", "normalized spool record"));
                }
                transform(part[i].0, serde_json::from_slice(b)?)
            })?;
            for ((k, _), v) in part.iter().zip(values) {
                consume(k, v)?;
            }
            if let Some(p) = &mut progress {
                p.count(at as u64);
            }
        }
        Ok(())
    }
    pub fn counts(&self) -> Value {
        let mut counts = json!({"entities":0,"relations":0,"occurrences":0,"bodies":0,"documents":0,"external":0});
        for family in ["entity", "relation", "occurrence", "body", "document"] {
            let name = match family {
                "entity" => "entities",
                "body" => "bodies",
                _ => match family {
                    "relation" => "relations",
                    "occurrence" => "occurrences",
                    _ => "documents",
                },
            };
            counts[name] = json!(
                self.entries
                    .keys()
                    .filter(|k| k.starts_with(&format!("{family}:")))
                    .count()
            );
        }
        counts
    }
    pub fn entity_units(&self) -> Result<BTreeMap<String, String>> {
        let mut units = BTreeMap::new();
        self.each("entity", |_, v| {
            units.insert(
                v["id"].as_str().unwrap().into(),
                v["unit"].as_str().unwrap().into(),
            );
            Ok(())
        })?;
        Ok(units)
    }
    pub fn legacy_generation(&self, previous: Option<&Revision>) -> Result<Value> {
        struct Sequence<'a> {
            spool: &'a Spool,
            shard: &'a str,
            family: &'a str,
        }
        impl Serialize for Sequence<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                let mut seq = s.serialize_seq(None)?;
                let keys = self
                    .spool
                    .by_shard
                    .get(&(Arc::from(self.shard), self.family.into()))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let mut at = 0;
                while at < keys.len() {
                    let first = at;
                    let mut bytes = 0;
                    while at < keys.len() && at - first < 512 {
                        let length = self.spool.entries[&keys[at]].length;
                        if at > first && bytes + length > crate::disk_batch::BATCH_BYTES {
                            break;
                        }
                        bytes += length;
                        at += 1;
                    }
                    let requested = keys[first..at]
                        .iter()
                        .map(AsRef::as_ref)
                        .collect::<Vec<_>>();
                    for value in self
                        .spool
                        .read_many(&requested)
                        .map_err(serde::ser::Error::custom)?
                    {
                        seq.serialize_element(&value)?;
                    }
                }
                seq.end()
            }
        }
        struct Shard<'a> {
            spool: &'a Spool,
            key: &'a str,
            source: &'a Option<FileRevision>,
        }
        impl Serialize for Shard<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                let mut v = s.serialize_struct("Shard", 7)?;
                v.serialize_field("key", self.key)?;
                v.serialize_field("source", self.source)?;
                for (name, family) in [
                    ("entities", "entity"),
                    ("bodies", "body"),
                    ("occurrences", "occurrence"),
                    ("relations", "relation"),
                    ("documents", "document"),
                ] {
                    v.serialize_field(
                        name,
                        &Sequence {
                            spool: self.spool,
                            shard: self.key,
                            family,
                        },
                    )?;
                }
                v.end()
            }
        }
        use sha2::{Digest, Sha256};
        struct Hash(Sha256);
        impl Write for Hash {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.update(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut manifest = Vec::with_capacity(self.sources.len());
        let shards = self.sources.iter().collect::<Vec<_>>();
        let mut progress = self.diagnostic_root.as_ref().map(|root| {
            cq_core::progress::Progress::stage(
                root,
                &self.diagnostic_header.project,
                "prepare.shard_hashes",
                "封存事实分片",
                Some(shards.len() as u64),
                "分片",
            )
        });
        for wave in shards.chunks(cq_core::compute::workers()?.min(8)) {
            manifest.extend(cq_core::compute::map(wave.to_vec(), |(key, source)| {
                let mut h = Hash(Sha256::new());
                serde_json::to_writer(
                    &mut h,
                    &Shard {
                        spool: self,
                        key,
                        source,
                    },
                )?;
                Ok(json!({"key":key,"digest":format!("{:x}",h.0.finalize())}))
            })?);
            if let Some(p) = &mut progress {
                p.count(manifest.len() as u64);
            }
        }
        let generation = digest(&serde_json::to_vec(&(&self.universe, &manifest))?);
        let old = previous
            .map(|r| &r.metadata["semantic_generation"])
            .filter(|p| p["universe"] == self.universe);
        let old_rows = old
            .and_then(|p| p["manifest"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| {
                Some((
                    v["key"].as_str()?.to_owned(),
                    v["digest"].as_str()?.to_owned(),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let upserts = manifest
            .iter()
            .filter(|v| {
                old_rows.get(v["key"].as_str().unwrap()).map(String::as_str) != v["digest"].as_str()
            })
            .map(|v| v["key"].clone())
            .collect::<Vec<_>>();
        let deletes = old_rows
            .keys()
            .filter(|k| !self.sources.contains_key(*k))
            .collect::<Vec<_>>();
        Ok(
            json!({"generation":generation,"universe":self.universe,"manifest":manifest,"upserts":upserts,"deletes":deletes,"base_generation":old.map(|p|&p["generation"]),"method":"normalized shard hashes streamed from canonical records; no second full projection"}),
        )
    }
}
impl normalized::Sink for Spool {
    fn batch(&mut self, records: Vec<(String, String, normalized::Record)>) -> Result<()> {
        self.insert_batch(records)
    }
    fn push(&mut self, unit: &str, shard: &str, r: normalized::Record) -> Result<()> {
        self.insert(unit, shard, r)
    }
}
impl LocalStore {
    pub fn prepare_records(
        &self,
        mut header: SemanticSnapshot,
        source: &dyn cq_core::semantic::records::Source,
    ) -> Result<Prepared> {
        crate::facts::limits::reject_oversized_build_events(&self.root, &mut header)?;
        cq_core::semantic::record_projection::validate(&header, source, &self.content)?;
        if header.previous.is_some() && !self.paged_enabled(&header.project) {
            self.migrate_paged(&header.project)?;
        }
        let mut spool = Spool::new(&header)?;
        spool.diagnostic_root = Some(self.root.clone());
        let base = ScopeBase::open(self, &header.project, header.previous.as_deref())?;
        let projection =
            cq_core::semantic::record_projection::canonicalize(&mut header, source, &mut spool)?;
        spool.finish(&mut header, projection, &base)?;
        let generation = spool.legacy_generation(base.manifest())?;
        Ok(Prepared {
            header,
            spool,
            base,
            generation,
        })
    }
    pub fn prepare_analysis(&self, mut snapshot: SemanticSnapshot) -> Result<Prepared> {
        crate::facts::limits::reject_oversized_build_events(&self.root, &mut snapshot)?;
        if snapshot.previous.is_some() && !self.paged_enabled(&snapshot.project) {
            self.migrate_paged(&snapshot.project)?;
        }
        let mut spool = Spool::new(&snapshot)?;
        spool.diagnostic_root = Some(self.root.clone());
        let base = ScopeBase::open(self, &snapshot.project, snapshot.previous.as_deref())?;
        let projection = production::canonicalize_into(&mut snapshot, &mut spool)?;
        spool.finish(&mut snapshot, projection, &base)?;
        let generation = spool.legacy_generation(base.manifest())?;
        Ok(Prepared {
            header: snapshot,
            spool,
            base,
            generation,
        })
    }
}
