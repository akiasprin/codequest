//! Compiler observations and reusable semantic payloads have separate identities.
//! Native frontends still own their project-wide semantic databases. This module
//! bounds the normalized publication stream; it does not claim streaming parsing.
use super::scope::{Completion, FactFamily, ScopeKey};
use crate::{Error, Result, model::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const BATCH_ROWS: usize = 128;
pub const BATCH_BYTES: usize = 1024 * 1024;
pub const RECORD_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnitObservation {
    pub unit: String,
    pub observed: BuildUnit,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvenanceObservation {
    pub canonical: ProvenanceId,
    pub observed: Provenance,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeDescriptor {
    pub key: ScopeKey,
    pub input_witness: String,
    pub completion: Completion,
    pub rows: u64,
    pub digest: String,
}
impl ScopeDescriptor {
    pub fn id(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(&self.key)?))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Production {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub transport: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub streamed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<ScopeKey>,
    pub version: u32,
    pub partial: bool,
    pub selection_complete: bool,
    pub failed_units: BTreeMap<String, String>,
    pub unit_observations: Vec<UnitObservation>,
    pub provenance_observations: Vec<ProvenanceObservation>,
    pub scopes: Vec<ScopeDescriptor>,
    pub identity_unmatched: u64,
    pub stage_ms: BTreeMap<String, u64>,
    pub native_boundary: String,
}
impl Default for Production {
    fn default() -> Self {
        Self { transport:BTreeMap::new(), retired:vec![], streamed:false, version:1, partial:false, selection_complete:true, failed_units:BTreeMap::new(), unit_observations:vec![], provenance_observations:vec![], scopes:vec![], identity_unmatched:0, stage_ms:BTreeMap::new(), native_boundary:"native frontends and cross-unit normalization retain project-wide state; publication batches are bounded".into() }
    }
}
pub fn failed(s: &mut SemanticSnapshot, unit: &str, reason: &str) {
    let p = s.production.get_or_insert_with(Default::default);
    p.partial = true;
    p.failed_units.insert(unit.into(), reason.into());
    s.gaps.push(Gap::new("ScopeAnalysisFailed", unit, reason));
}
fn rebound(v: &mut String, units: &BTreeMap<String, String>) {
    if let Some(id) = units.get(v) {
        *v = id.clone();
    }
}
/// Canonical payloads retain source/configuration coordinates; raw run bindings
/// remain available as independently pageable observation records.
fn is_false(value: &bool) -> bool {
    !*value
}
pub fn canonicalize(s: &mut SemanticSnapshot) -> Result<()> {
    canonicalize_impl(s, None)?;
    refresh_descriptors(s)
}
pub fn canonicalize_into(
    s: &mut SemanticSnapshot,
    sink: &mut dyn super::normalized::Sink,
) -> Result<super::normalized::Projection> {
    canonicalize_impl(s, Some(sink))?
        .ok_or_else(|| Error::new("MissingProjection", "streamed projection did not finish"))
}
fn canonicalize_impl(
    s: &mut SemanticSnapshot,
    output: Option<&mut dyn super::normalized::Sink>,
) -> Result<Option<super::normalized::Projection>> {
    let started = std::time::Instant::now();
    let root = s.root.display();
    let contexts = super::identity::compilation_identities(&s.units, &root)?;
    contexts.report(s);
    for entity in &mut s.entities {
        contexts.mark(entity);
    }
    let units = contexts.units.into_iter().collect::<BTreeMap<_, _>>();
    let mut p = s.production.take().unwrap_or_default();
    p.unit_observations = s
        .units
        .iter()
        .map(|u| UnitObservation {
            unit: units[&u.id].clone(),
            observed: u.clone(),
        })
        .collect();
    p.provenance_observations = s
        .provenance_records
        .values()
        .map(|observed| {
            let mut canonical = observed.clone();
            canonical.build_run = None;
            Ok(ProvenanceObservation {
                canonical: ProvenanceId(digest(&serde_json::to_vec(&canonical)?)),
                observed: observed.clone(),
            })
        })
        .collect::<Result<_>>()?;
    p.identity_unmatched = s
        .entities
        .iter()
        .filter(|e| {
            e.attributes.contains_key("identity_scope_reason")
                || (e.source.is_none()
                    && e.stable_handle
                        .as_ref()
                        .is_some_and(|h| h.starts_with("cqg1:")))
        })
        .count() as u64;
    let streamed = output.is_some();
    if streamed && p.partial {
        prune_failed(s, &p.failed_units);
    }
    let projection = if let Some(sink) = output {
        let projection = super::generation::project_into(s, sink)?;
        s.entities.clear();
        s.bodies.clear();
        s.occurrences.clear();
        s.relations.clear();
        s.documents.clear();
        s.provenance_records = projection.provenance.clone();
        Some(projection)
    } else {
        let g = super::generation::project(s, 1)?;
        s.entities.clear();
        s.bodies.clear();
        s.occurrences.clear();
        s.relations.clear();
        s.documents.clear();
        for shard in g.shards.into_values() {
            s.entities.extend(shard.entities);
            s.bodies.extend(shard.bodies);
            s.occurrences.extend(shard.occurrences);
            s.relations.extend(shard.relations);
            s.documents.extend(shard.documents);
        }
        s.provenance_records = g.provenance;
        None
    };
    for u in &mut s.units {
        rebound(&mut u.id, &units);
    }
    for c in &mut s.contexts {
        rebound(&mut c.entry_unit, &units);
        c.id = digest(c.entry_unit.as_bytes());
        for id in &mut c.units {
            rebound(id, &units);
        }
        c.units.sort();
        c.units.dedup();
    }
    for d in &mut s.build_dependencies {
        rebound(&mut d.consumer, &units);
        if let Observed::Value(id) = &mut d.producer {
            rebound(id, &units);
        }
    }
    p.failed_units = p
        .failed_units
        .into_iter()
        .map(|(mut id, reason)| {
            rebound(&mut id, &units);
            (id, reason)
        })
        .collect();
    for gap in &mut s.gaps {
        if gap.code == "ScopeAnalysisFailed" {
            rebound(&mut gap.scope, &units);
        }
    }
    if p.partial && !streamed {
        prune_failed(s, &p.failed_units);
    }
    p.stage_ms.insert(
        "canonical_payloads".into(),
        started.elapsed().as_millis() as u64,
    );
    p.streamed = streamed;
    s.production = Some(p);
    Ok(projection)
}
/// Re-seal normalized output after adding a separately attributed evidence layer.
/// This preserves original compiler observation bindings.
pub fn refresh_descriptors(s: &mut SemanticSnapshot) -> Result<()> {
    if s.production.is_none() {
        return Ok(());
    }
    // Sorting happens before sealing the producer's scope summaries.
    s.seal()?;
    let scopes = inventory(s)?;
    s.production.as_mut().unwrap().scopes = scopes
        .into_iter()
        .map(|(key, rows)| {
            let mut hash = Sha256::new();
            for row in &rows {
                absorb(&mut hash, &row.record(s)?)?;
            }
            let witness = digest(&serde_json::to_vec(&(
                &key,
                &s.working_tree_manifest,
                &s.analysis_inputs,
                &s.analysis_directories,
                s.providers
                    .iter()
                    .map(|p| (&p.name, &p.version, &p.binary_digest))
                    .collect::<Vec<_>>(),
                &s.analysis_request,
            ))?);
            Ok(ScopeDescriptor {
                completion: if s
                    .production
                    .as_ref()
                    .unwrap()
                    .failed_units
                    .contains_key(&key.unit)
                {
                    Completion::Failed
                } else {
                    Completion::Complete
                },
                key,
                input_witness: witness,
                rows: rows.len() as u64,
                digest: format!("{:x}", hash.finalize()),
            })
        })
        .collect::<Result<_>>()?;
    Ok(())
}
fn prune_failed(s: &mut SemanticSnapshot, failed: &BTreeMap<String, String>) {
    s.entities.retain(|e| !failed.contains_key(&e.unit));
    let ids = s
        .entities
        .iter()
        .map(|e| e.id.clone())
        .collect::<BTreeSet<_>>();
    for e in &mut s.entities {
        if e.parent.as_ref().is_some_and(|id| !ids.contains(id)) {
            e.parent = None;
        }
        e.type_definitions.retain(|id| ids.contains(id));
        for nav in &mut e.type_navigation {
            nav.candidates.retain(|id| ids.contains(id));
        }
    }
    s.bodies
        .retain(|b| !failed.contains_key(&b.unit) && ids.contains(&b.owner));
    let bodies = s
        .bodies
        .iter()
        .map(|b| b.id.clone())
        .collect::<BTreeSet<_>>();
    for b in &mut s.bodies {
        if b.parent.as_ref().is_some_and(|p| !bodies.contains(p)) {
            b.parent = None;
        }
    }
    fn resolution(r: &mut Resolution, ids: &BTreeSet<String>) {
        match r {
            Resolution::Resolved(id) if !ids.contains(id) => {
                *r = Resolution::Unknown("target unavailable in failed compilation scope".into())
            }
            Resolution::Candidates(v) => {
                v.retain(|id| ids.contains(id));
                if v.is_empty() {
                    *r = Resolution::Unknown("candidate scopes unavailable".into());
                }
            }
            _ => {}
        }
    }
    s.occurrences.retain(|o| {
        !failed.contains_key(&o.unit) && o.owner.as_ref().is_none_or(|id| ids.contains(id))
    });
    for o in &mut s.occurrences {
        resolution(&mut o.declaration, &ids);
        resolution(&mut o.executable, &ids);
        for a in &mut o.argument_bindings {
            resolution(&mut a.parameter, &ids);
        }
    }
    let occurrences = s
        .occurrences
        .iter()
        .map(|o| o.id.clone())
        .collect::<BTreeSet<_>>();
    s.relations.retain(|r| {
        ids.contains(&r.from)
            && ids.contains(&r.to)
            && r.occurrence
                .as_ref()
                .is_none_or(|id| occurrences.contains(id))
    });
    s.documents
        .retain(|d| d.owner.as_ref().is_none_or(|id| ids.contains(id)));
    let documents = s
        .documents
        .iter()
        .map(|d| d.id.clone())
        .collect::<BTreeSet<_>>();
    for d in &mut s.documents {
        if d.parent.as_ref().is_some_and(|id| !documents.contains(id)) {
            d.parent = None;
        }
        for link in &mut d.links {
            resolution(&mut link.target, &ids);
        }
    }
    for c in &mut s.contexts {
        if c.units.iter().any(|id| failed.contains_key(id)) {
            c.status = Support::Partial;
            c.gaps.push(Gap::new(
                "ScopeAnalysisFailed",
                &c.id,
                "one or more selected units failed; absence does not establish nonexistence",
            ));
        }
    }
}
pub fn family_name(f: &FactFamily) -> &'static str {
    match f {
        FactFamily::Entity => "entity",
        FactFamily::Body => "body",
        FactFamily::Occurrence => "occurrence",
        FactFamily::Relation => "relation",
        FactFamily::Document => "document",
        FactFamily::Metadata => "metadata",
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub value: Value,
}
#[derive(Clone, Copy)]
enum Index {
    Entity(usize),
    Body(usize),
    Occurrence(usize),
    Relation(usize),
    Document(usize),
}
impl Index {
    fn record(self, s: &SemanticSnapshot) -> Result<Record> {
        let (family, id, value) = match self {
            Self::Entity(i) => (
                "entity",
                &s.entities[i].id,
                serde_json::to_value(&s.entities[i])?,
            ),
            Self::Body(i) => ("body", &s.bodies[i].id, serde_json::to_value(&s.bodies[i])?),
            Self::Occurrence(i) => (
                "occurrence",
                &s.occurrences[i].id,
                serde_json::to_value(&s.occurrences[i])?,
            ),
            Self::Relation(i) => (
                "relation",
                &s.relations[i].id,
                serde_json::to_value(&s.relations[i])?,
            ),
            Self::Document(i) => (
                "document",
                &s.documents[i].id,
                serde_json::to_value(&s.documents[i])?,
            ),
        };
        Ok(Record {
            key: crate::ports::facts::fact_key(family, id),
            value,
        })
    }
}
fn inventory(s: &SemanticSnapshot) -> Result<BTreeMap<ScopeKey, Vec<Index>>> {
    let units = s
        .units
        .iter()
        .map(|u| u.id.as_str())
        .collect::<BTreeSet<_>>();
    let entity_units = s
        .entities
        .iter()
        .map(|e| (e.id.as_str(), e.unit.as_str()))
        .collect::<BTreeMap<_, _>>();
    let scope = |unit: &str, family: FactFamily| {
        let unit = if units.contains(unit) {
            unit
        } else if unit == "external" {
            "external"
        } else {
            "$project"
        }
        .to_owned();
        ScopeKey {
            context: digest(unit.as_bytes()),
            unit,
            family,
        }
    };
    let mut out = BTreeMap::<ScopeKey, Vec<Index>>::new();
    for unit in units.iter().copied().chain(["external", "$project"]) {
        for family in [
            FactFamily::Entity,
            FactFamily::Body,
            FactFamily::Occurrence,
            FactFamily::Relation,
            FactFamily::Document,
        ] {
            out.entry(scope(unit, family)).or_default();
        }
    }
    if let Some(p) = &s.production {
        for key in &p.retired {
            out.entry(key.clone()).or_default();
        }
    }
    for (i, e) in s.entities.iter().enumerate() {
        out.entry(scope(&e.unit, FactFamily::Entity))
            .or_default()
            .push(Index::Entity(i));
    }
    for (i, b) in s.bodies.iter().enumerate() {
        out.entry(scope(&b.unit, FactFamily::Body))
            .or_default()
            .push(Index::Body(i));
    }
    for (i, o) in s.occurrences.iter().enumerate() {
        out.entry(scope(&o.unit, FactFamily::Occurrence))
            .or_default()
            .push(Index::Occurrence(i));
    }
    for (i, r) in s.relations.iter().enumerate() {
        out.entry(scope(
            entity_units
                .get(r.from.as_str())
                .copied()
                .unwrap_or("$project"),
            FactFamily::Relation,
        ))
        .or_default()
        .push(Index::Relation(i));
    }
    for (i, d) in s.documents.iter().enumerate() {
        out.entry(scope(
            d.owner
                .as_deref()
                .and_then(|id| entity_units.get(id))
                .copied()
                .unwrap_or("$project"),
            FactFamily::Document,
        ))
        .or_default()
        .push(Index::Document(i));
    }
    Ok(out)
}
fn absorb(h: &mut Sha256, row: &Record) -> Result<usize> {
    let bytes = serde_json::to_vec(&row.value)?;
    if bytes.len() > RECORD_BYTES {
        return Err(Error::new("OversizedFact", "record exceeds 8 MiB"));
    }
    h.update((row.key.len() as u64).to_le_bytes());
    h.update(row.key.as_bytes());
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(Sha256::digest(&bytes));
    Ok(bytes.len())
}
pub trait Sink {
    fn begin(&mut self, scope: &ScopeDescriptor) -> Result<()>;
    fn batch(&mut self, scope: &ScopeKey, sequence: u64, records: &[Record]) -> Result<()>;
    fn end(&mut self, scope: &ScopeDescriptor, batches: u64) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}
/// Records are serialized only for the current batch. Indexes contain offsets,
/// not a second collection of semantic payloads.
pub fn emit(s: &SemanticSnapshot, sink: &mut dyn Sink) -> Result<()> {
    let p = s.production.as_ref().ok_or_else(|| {
        Error::new(
            "MissingProduction",
            "native publication requires scope descriptors",
        )
    })?;
    let mut rows = inventory(s)?;
    for scope in &p.scopes {
        sink.begin(scope)?;
        let indexes = rows
            .remove(&scope.key)
            .ok_or_else(|| Error::new("UnexpectedScope", "scope not in current inventory"))?;
        let mut batch = vec![];
        let mut bytes = 0;
        let mut sequence = 0;
        for index in indexes {
            let row = index.record(s)?;
            let size = serde_json::to_vec(&row.value)?.len();
            if !batch.is_empty() && (batch.len() >= BATCH_ROWS || bytes + size > BATCH_BYTES) {
                sink.batch(&scope.key, sequence, &batch)?;
                sequence += 1;
                batch.clear();
                bytes = 0;
            }
            bytes += size;
            batch.push(row);
        }
        if !batch.is_empty() {
            sink.batch(&scope.key, sequence, &batch)?;
            sequence += 1;
        }
        sink.end(scope, sequence)?;
    }
    if !rows.is_empty() {
        return Err(Error::new(
            "MissingScopeReceipt",
            "producer omitted selected scopes",
        ));
    }
    sink.finish()
}
struct Active {
    scope: ScopeDescriptor,
    hash: Sha256,
    rows: u64,
    batches: u64,
    last: Option<String>,
}
pub struct Validator {
    expected: BTreeMap<ScopeKey, ScopeDescriptor>,
    seen: BTreeSet<ScopeKey>,
    active: Option<Active>,
    poisoned: bool,
}
impl Validator {
    pub fn new(p: &Production) -> Result<Self> {
        if p.version != 1 || (!p.selection_complete && !p.partial) {
            return Err(Error::new(
                "IncompleteSelection",
                "unverified selection cannot delete old facts",
            ));
        }
        let mut expected = BTreeMap::new();
        for scope in &p.scopes {
            if !valid_digest(&scope.input_witness)
                || !valid_digest(&scope.digest)
                || (!p.partial && scope.completion != Completion::Complete)
                || scope.completion != Completion::Complete && scope.rows != 0
            {
                return Err(Error::new(
                    "InvalidScopeReceipt",
                    "invalid witness/completion",
                ));
            }
            if expected.insert(scope.key.clone(), scope.clone()).is_some() {
                return Err(Error::new(
                    "ConflictingScopeReceipt",
                    "duplicate selected scope",
                ));
            }
        }
        if expected.is_empty() {
            return Err(Error::new("IncompleteSelection", "no producer scopes"));
        }
        Ok(Self {
            expected,
            seen: BTreeSet::new(),
            active: None,
            poisoned: false,
        })
    }
}
impl Validator {
    fn begin_inner(&mut self, scope: &ScopeDescriptor) -> Result<()> {
        let expected = self
            .expected
            .get(&scope.key)
            .ok_or_else(|| Error::new("UnexpectedScope", "unselected scope"))?;
        if self.active.is_some()
            || !self.seen.insert(scope.key.clone())
            || serde_json::to_vec(expected)? != serde_json::to_vec(scope)?
        {
            return Err(Error::new(
                "ConflictingScopeReceipt",
                "scope begin differs/repeats",
            ));
        }
        self.active = Some(Active {
            scope: scope.clone(),
            hash: Sha256::new(),
            rows: 0,
            batches: 0,
            last: None,
        });
        Ok(())
    }
    fn batch_inner(&mut self, scope: &ScopeKey, sequence: u64, records: &[Record]) -> Result<()> {
        let a = self
            .active
            .as_mut()
            .ok_or_else(|| Error::new("MissingScopeBegin", "batch before begin"))?;
        if &a.scope.key != scope
            || a.scope.completion != Completion::Complete
            || sequence != a.batches
            || records.is_empty()
            || records.len() > BATCH_ROWS
        {
            return Err(Error::new(
                "InvalidBatch",
                "sequence, scope, completion or row bound",
            ));
        }
        let mut bytes = 0;
        for row in records {
            let id = row.value["id"]
                .as_str()
                .ok_or_else(|| Error::new("InvalidIdentity", "stream record missing id"))?;
            if row.key != crate::ports::facts::fact_key(family_name(&scope.family), id)
                || a.last.as_ref().is_some_and(|last| last >= &row.key)
            {
                return Err(Error::new(
                    "InvalidBatch",
                    "foreign family or unordered/duplicate identity",
                ));
            }
            bytes += absorb(&mut a.hash, row)?;
            a.last = Some(row.key.clone());
            a.rows += 1;
        }
        if bytes > BATCH_BYTES && records.len() != 1 {
            return Err(Error::new("InvalidBatch", "byte bound"));
        }
        a.batches += 1;
        Ok(())
    }
    fn end_inner(&mut self, scope: &ScopeDescriptor, batches: u64) -> Result<()> {
        let a = self
            .active
            .take()
            .ok_or_else(|| Error::new("MissingScopeBegin", "end before begin"))?;
        if serde_json::to_vec(&a.scope)? != serde_json::to_vec(scope)?
            || a.batches != batches
            || a.rows != scope.rows
            || format!("{:x}", a.hash.finalize()) != scope.digest
        {
            return Err(Error::new(
                "IncompleteScope",
                "missing batch or digest mismatch",
            ));
        }
        Ok(())
    }
    fn finish_inner(&mut self) -> Result<()> {
        if self.active.is_some() || self.seen.len() != self.expected.len() {
            return Err(Error::new(
                "MissingScopeReceipt",
                "missing scope completion",
            ));
        }
        Ok(())
    }
    fn checked(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if self.poisoned {
            return Err(Error::new(
                "InvalidProductionTransaction",
                "prior frame failed; discard candidate",
            ));
        }
        let result = f(self);
        self.poisoned = result.is_err();
        result
    }
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Sink for Validator {
    fn begin(&mut self, s: &ScopeDescriptor) -> Result<()> {
        self.checked(|v| v.begin_inner(s))
    }
    fn batch(&mut self, s: &ScopeKey, n: u64, r: &[Record]) -> Result<()> {
        self.checked(|v| v.batch_inner(s, n, r))
    }
    fn end(&mut self, s: &ScopeDescriptor, n: u64) -> Result<()> {
        self.checked(|v| v.end_inner(s, n))
    }
    fn finish(&mut self) -> Result<()> {
        self.checked(Self::finish_inner)
    }
}
