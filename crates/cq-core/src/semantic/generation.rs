//! Validated semantic shard transactions. Compiler observations remain in the
//! immutable source snapshot; this projection excludes run-specific coordinates.
use crate::{Error, Result, model::*, ports::ContentReader};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shard {
    pub key: String,
    pub source: Option<FileRevision>,
    pub entities: Vec<Entity>,
    pub bodies: Vec<Body>,
    pub occurrences: Vec<Occurrence>,
    pub relations: Vec<Relation>,
    pub documents: Vec<DocumentBlock>,
}
impl Shard {
    fn empty(key: String, source: Option<FileRevision>) -> Self {
        Self {
            key,
            source,
            entities: vec![],
            bodies: vec![],
            occurrences: vec![],
            relations: vec![],
            documents: vec![],
        }
    }
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub key: String,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Generation {
    pub version: u32,
    pub project: String,
    pub root: NativeValue,
    pub universe: String,
    pub sequence: u64,
    pub generation: String,
    pub manifest: Vec<ManifestEntry>,
    pub shards: BTreeMap<String, Shard>,
    pub provenance: BTreeMap<ProvenanceId, Provenance>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delta {
    pub version: u32,
    pub project: String,
    pub root: NativeValue,
    pub universe: String,
    pub sequence: u64,
    pub base_generation: Option<String>,
    pub generation: String,
    pub manifest: Vec<ManifestEntry>,
    pub upserts: Vec<Shard>,
    pub deletes: Vec<String>,
    pub provenance: BTreeMap<ProvenanceId, Provenance>,
}
fn fail(message: &str) -> Error {
    Error::new("InvalidGeneration", message)
}
fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn generation(universe: &str, manifest: &[ManifestEntry]) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(universe, manifest))?))
}
fn manifest(shards: &BTreeMap<String, Shard>) -> Result<Vec<ManifestEntry>> {
    shards
        .iter()
        .map(|(key, s)| {
            Ok(ManifestEntry {
                key: key.clone(),
                digest: s.digest()?,
            })
        })
        .collect()
}
impl Generation {
    pub fn delta(&self, prior: Option<&Generation>) -> Delta {
        let base = prior.filter(|p| p.universe == self.universe);
        let old: BTreeMap<_, _> = base
            .map(|p| p.manifest.iter().map(|e| (&e.key, &e.digest)).collect())
            .unwrap_or_default();
        Delta {
            version: 1,
            project: self.project.clone(),
            root: self.root.clone(),
            universe: self.universe.clone(),
            sequence: prior.map(|p| p.sequence + 1).unwrap_or(1),
            base_generation: base.map(|p| p.generation.clone()),
            generation: self.generation.clone(),
            manifest: self.manifest.clone(),
            upserts: self
                .manifest
                .iter()
                .filter(|e| old.get(&e.key).is_none_or(|d| **d != e.digest))
                .map(|e| self.shards[&e.key].clone())
                .collect(),
            deletes: old
                .keys()
                .filter(|k| !self.shards.contains_key(**k))
                .map(|k| (*k).clone())
                .collect(),
            provenance: self.provenance.clone(),
        }
    }
    pub fn materialize(&self) -> SemanticSnapshot {
        let mut snapshot = SemanticSnapshot::empty(self.project.clone(), self.root.clone());
        snapshot.provenance_records = self.provenance.clone();
        let files = self
            .shards
            .values()
            .filter_map(|s| s.source.clone())
            .collect();
        snapshot.sources = SourceSnapshot::seal(files, InputBinding::PrePostChecked, vec![]);
        for s in self.shards.values() {
            snapshot.entities.extend(s.entities.clone());
            snapshot.bodies.extend(s.bodies.clone());
            snapshot.occurrences.extend(s.occurrences.clone());
            snapshot.relations.extend(s.relations.clone());
            snapshot.documents.extend(s.documents.clone());
        }
        snapshot
    }
    pub fn validate(&self, content: &dyn ContentReader) -> Result<()> {
        if self.version != 1
            || !valid_digest(&self.universe)
            || self.sequence == 0
            || self.shards.is_empty()
        {
            return Err(fail("version/universe/sequence/empty shards"));
        }
        for (key, shard) in &self.shards {
            if key != &shard.key
                || key.is_empty()
                || shard
                    .source
                    .as_ref()
                    .is_some_and(|f| f.logical_path.display() != *key)
                || shard.source.is_none() && key != "$metadata" && !key.starts_with("$metadata:")
            {
                return Err(fail("shard key/source mismatch"));
            }
        }
        if self.manifest != manifest(&self.shards)?
            || self.generation != generation(&self.universe, &self.manifest)?
        {
            return Err(fail("manifest/generation digest mismatch"));
        }
        crate::semantic::validate::validate(&self.materialize(), content)
    }
    /// Pure prepare: validation never mutates the currently published generation.
    pub fn prepare(
        prior: Option<&Generation>,
        delta: &Delta,
        content: &dyn ContentReader,
    ) -> Result<Self> {
        if delta.version != 1
            || !valid_digest(&delta.generation)
            || !valid_digest(&delta.universe)
            || delta.sequence == 0
        {
            return Err(fail("malformed transaction coordinates"));
        }
        if delta.deletes.windows(2).any(|p| p[0] >= p[1])
            || delta.upserts.windows(2).any(|p| p[0].key >= p[1].key)
        {
            return Err(fail("non-canonical upsert/delete order"));
        }
        if let Some(prior) = prior {
            if prior.project != delta.project || prior.root != delta.root {
                return Err(fail("foreign project/root"));
            }
            if delta.sequence != prior.sequence + 1 {
                return Err(fail("non-monotonic sequence"));
            }
            if let Some(base) = &delta.base_generation {
                if base != &prior.generation || delta.universe != prior.universe {
                    return Err(fail("stale base or changed universe requires full reload"));
                }
            } else if delta.generation == prior.generation {
                return Err(fail("unchanged generation lost base"));
            }
        } else if delta.base_generation.is_some() {
            return Err(fail("delta requires committed base"));
        }
        if !delta.deletes.windows(2).all(|w| w[0] < w[1])
            || !delta.manifest.windows(2).all(|w| w[0].key < w[1].key)
        {
            return Err(fail("noncanonical manifest/deletes"));
        }
        let mut shards = if delta.base_generation.is_some() {
            prior.unwrap().shards.clone()
        } else {
            BTreeMap::new()
        };
        let mut touched = BTreeSet::new();
        for key in &delta.deletes {
            if !touched.insert(key.clone()) || shards.remove(key).is_none() {
                return Err(fail("missing/duplicate delete"));
            }
        }
        for shard in &delta.upserts {
            if !touched.insert(shard.key.clone()) {
                return Err(fail("duplicate upsert/delete collision"));
            }
            shards.insert(shard.key.clone(), shard.clone());
        }
        let next = Self {
            version: 1,
            project: delta.project.clone(),
            root: delta.root.clone(),
            universe: delta.universe.clone(),
            sequence: delta.sequence,
            generation: delta.generation.clone(),
            manifest: delta.manifest.clone(),
            shards,
            provenance: delta.provenance.clone(),
        };
        next.validate(content)?;
        Ok(next)
    }
}
pub(super) fn mapped(value: &mut String, map: &HashMap<String, String>) {
    if let Some(id) = map.get(value) {
        *value = id.clone();
    }
}
pub(super) fn resolution(value: &mut Resolution, map: &HashMap<String, String>) {
    match value {
        Resolution::Resolved(id) => mapped(id, map),
        Resolution::Candidates(ids) => {
            for id in ids {
                mapped(id, map)
            }
        }
        _ => {}
    }
}
pub(super) fn rebind(value: &mut serde_json::Value, map: &HashMap<String, String>) {
    match value {
        serde_json::Value::String(s) => mapped(s, map),
        serde_json::Value::Array(a) => {
            for v in a {
                rebind(v, map)
            }
        }
        serde_json::Value::Object(o) => {
            for (key, v) in o {
                if ![
                    "text",
                    "title",
                    "signature",
                    "name",
                    "qualified_name",
                    "message",
                    "reason",
                    "note",
                    "destination",
                ]
                .contains(&key.as_str())
                {
                    rebind(v, map)
                }
            }
        }
        _ => {}
    }
}

pub fn project(snapshot: &SemanticSnapshot, sequence: u64) -> Result<Generation> {
    project_impl(snapshot, sequence, None)
}
/// Rebind one record at a time. No second project-wide semantic payload is built.
pub fn project_into(
    snapshot: &SemanticSnapshot,
    sink: &mut dyn super::normalized::Sink,
) -> Result<super::normalized::Projection> {
    let g = project_impl(snapshot, 1, Some(sink))?;
    Ok(super::normalized::Projection {
        universe: g.universe,
        provenance: g.provenance,
        sources: g.shards.into_iter().map(|(k, s)| (k, s.source)).collect(),
    })
}
fn project_impl(
    snapshot: &SemanticSnapshot,
    sequence: u64,
    mut output: Option<&mut dyn super::normalized::Sink>,
) -> Result<Generation> {
    let root = snapshot.root.display();
    let units: HashMap<_, _> = snapshot
        .units
        .iter()
        .map(|u| Ok((u.id.clone(), super::identity::unit_key(u, &root)?)))
        .collect::<Result<_>>()?;
    let mut ids = HashMap::new();
    for e in &snapshot.entities {
        let id = if e.kind == EntityKind::CompilationUnit {
            format!("cqu1:{}", units.get(&e.unit).unwrap_or(&e.unit))
        } else if let Some(h) = e.stable_handle.as_ref().filter(|h| h.starts_with("cq1:")) {
            h.clone()
        } else {
            digest(&serde_json::to_vec(&(
                units.get(&e.unit),
                e.kind,
                super::identity::rust_symbol(&e.provider_id),
                &e.source,
                &e.signature,
                e.attributes.get("definition_kind"),
                (e.source.is_none() || e.attributes.contains_key("identity_scope_reason"))
                    .then_some(&e.id),
            ))?)
        };
        ids.insert(e.id.clone(), id);
    }
    for b in &snapshot.bodies {
        ids.insert(
            b.id.clone(),
            digest(&serde_json::to_vec(&(
                ids.get(&b.owner),
                b.kind,
                &b.source,
            ))?),
        );
    }
    let body_lookup: HashMap<_, _> = snapshot.bodies.iter().map(|b| (&b.id, b)).collect();
    let docs: HashMap<_, _> = snapshot
        .documents
        .iter()
        .map(|d| {
            Ok((
                d.id.clone(),
                digest(&serde_json::to_vec(&(
                    &d.source,
                    d.kind,
                    &d.title,
                    &d.text,
                    d.owner.as_ref().and_then(|id| ids.get(id)),
                ))?),
            ))
        })
        .collect::<Result<_>>()?;
    for doc in &snapshot.documents {
        if ids.contains_key(&doc.id) {
            ids.insert(doc.id.clone(), docs[&doc.id].clone());
        }
    }
    let mut provenance = BTreeMap::new();
    let mut prov_ids = HashMap::new();
    for (id, p) in &snapshot.provenance_records {
        let mut p = p.clone();
        p.build_run = None;
        let next = ProvenanceId(digest(&serde_json::to_vec(&p)?));
        prov_ids.insert(id.clone(), next.clone());
        provenance.insert(next, p);
    }
    // The primary snapshot retains all build evidence, including dep-info and
    // macro binaries. Only bytes referenced by semantic facts belong in this
    // graph projection. Never infer this boundary from a filename extension.
    let mut referenced = BTreeSet::new();
    let mut anchor = |a: &SourceAnchor| {
        referenced.insert(a.file_revision.clone());
    };
    for e in &snapshot.entities {
        for a in e
            .source
            .iter()
            .chain(e.extent.iter())
            .chain(e.locations.anchors())
        {
            anchor(a);
        }
        for nav in &e.type_navigation {
            anchor(&nav.source);
            if let Some(a) = &nav.extent {
                anchor(a);
            }
        }
    }
    for b in &snapshot.bodies {
        anchor(&b.source);
    }
    for o in &snapshot.occurrences {
        anchor(&o.source);
        for a in o.locations.anchors() {
            anchor(a);
        }
        for a in &o.arguments {
            anchor(a);
        }
        for b in &o.argument_bindings {
            anchor(&b.source);
        }
    }
    for r in &snapshot.relations {
        if let Some(a) = &r.source {
            anchor(a);
        }
    }
    for d in &snapshot.documents {
        anchor(&d.source);
    }
    let mut shards: BTreeMap<_, _> = snapshot
        .sources
        .files
        .iter()
        .filter(|f| referenced.contains(&f.id))
        .map(|f| {
            (
                f.logical_path.display(),
                Shard::empty(f.logical_path.display(), Some(f.clone())),
            )
        })
        .collect();
    shards.insert("$metadata".into(), Shard::empty("$metadata".into(), None));
    let source_keys: HashMap<_, _> = snapshot
        .sources
        .files
        .iter()
        .map(|f| (f.id.clone(), f.logical_path.display()))
        .collect();
    let key = |a: Option<&SourceAnchor>| {
        a.and_then(|a| source_keys.get(&a.file_revision))
            .cloned()
            .unwrap_or_else(|| "$metadata".into())
    };
    let mut entity_keys = HashMap::new();
    let entity_units = snapshot
        .entities
        .iter()
        .map(|e| {
            (
                e.id.as_str(),
                units
                    .get(&e.unit)
                    .cloned()
                    .unwrap_or_else(|| e.unit.clone()),
            )
        })
        .collect::<HashMap<_, _>>();
    for old in &snapshot.entities {
        let mut e = old.clone();
        e.id = ids[&old.id].clone();
        if e.stable_handle
            .as_ref()
            .is_some_and(|h| !h.starts_with("cq1:"))
        {
            e.stable_handle = None;
        }
        mapped(&mut e.unit, &units);
        e.provider_id = super::identity::rust_symbol(&e.provider_id);
        e.attributes.remove("native_token");
        if e.kind == EntityKind::CompilationUnit {
            e.provider_id = e.unit.clone();
            e.attributes.remove("configuration");
            e.stable_handle = None;
            e.qualified_name = e.qualified_name.replace(&old.unit, &e.unit);
        }
        if matches!(e.kind, EntityKind::Document | EntityKind::DocSection) {
            e.provider_id = e.id.clone();
            e.qualified_name = format!("doc::{}", e.id);
        }
        if e.kind == EntityKind::Body {
            e.provider_id = e.id.clone();
            if let Some(body) = body_lookup.get(&old.id).copied() {
                e.qualified_name = format!("{}::body@{}", ids[&body.owner], body.source.start);
            }
        }
        if let Some(p) = &mut e.parent {
            mapped(p, &ids)
        }
        for id in &mut e.type_definitions {
            mapped(id, &ids)
        }
        for n in &mut e.type_navigation {
            for id in &mut n.candidates {
                mapped(id, &ids)
            }
        }
        if let Some(c) = e.attributes.get_mut("definition_candidates") {
            let mut v: serde_json::Value = serde_json::from_str(c)?;
            rebind(&mut v, &ids);
            *c = serde_json::to_string(&v)?;
        }
        e.provenance = prov_ids[&e.provenance].clone();
        let k = if e.source.is_none() && !e.unit.is_empty() {
            format!("$metadata:{}", e.unit)
        } else {
            key(e.source.as_ref())
        };
        shards
            .entry(k.clone())
            .or_insert_with(|| Shard::empty(k.clone(), None));
        entity_keys.insert(old.id.clone(), k.clone());
        if let Some(sink) = output.as_deref_mut() {
            sink.push(&e.unit.clone(), &k, super::normalized::Record::Entity(e))?;
        } else {
            shards.get_mut(&k).unwrap().entities.push(e);
        }
    }
    for old in &snapshot.bodies {
        let mut b = old.clone();
        b.id = ids[&old.id].clone();
        mapped(&mut b.unit, &units);
        mapped(&mut b.owner, &ids);
        if let Some(p) = &mut b.parent {
            mapped(p, &ids)
        }
        if let Some(sink) = output.as_deref_mut() {
            sink.push(
                &b.unit.clone(),
                &key(Some(&b.source)),
                super::normalized::Record::Body(b),
            )?;
        } else {
            shards
                .get_mut(&key(Some(&b.source)))
                .unwrap()
                .bodies
                .push(b);
        }
    }
    let mut occurrences = HashMap::new();
    for old in &snapshot.occurrences {
        let mut o = old.clone();
        mapped(&mut o.unit, &units);
        if let Some(p) = &mut o.owner {
            mapped(p, &ids)
        }
        resolution(&mut o.declaration, &ids);
        resolution(&mut o.executable, &ids);
        for a in &mut o.argument_bindings {
            resolution(&mut a.parameter, &ids)
        }
        o.provenance = prov_ids[&o.provenance].clone();
        o.id.clear();
        o.id = digest(&serde_json::to_vec(&o)?);
        occurrences.insert(old.id.clone(), o.id.clone());
        if let Some(sink) = output.as_deref_mut() {
            sink.push(
                &o.unit.clone(),
                &key(Some(&o.source)),
                super::normalized::Record::Occurrence(o),
            )?;
        } else {
            shards
                .get_mut(&key(Some(&o.source)))
                .unwrap()
                .occurrences
                .push(o);
        }
    }
    for old in &snapshot.relations {
        let mut r = old.clone();
        mapped(&mut r.from, &ids);
        mapped(&mut r.to, &ids);
        if let Some(o) = &mut r.occurrence {
            mapped(o, &occurrences)
        }
        r.provenance = prov_ids[&r.provenance].clone();
        r.id.clear();
        r.id = digest(&serde_json::to_vec(&r)?);
        let k = r.source.as_ref().map(|a| key(Some(a))).unwrap_or_else(|| {
            entity_keys
                .get(&old.from)
                .cloned()
                .unwrap_or_else(|| "$metadata".into())
        });
        if let Some(sink) = output.as_deref_mut() {
            sink.push(
                entity_units
                    .get(old.from.as_str())
                    .map(String::as_str)
                    .unwrap_or("$project"),
                &k,
                super::normalized::Record::Relation(r),
            )?;
        } else {
            shards.get_mut(&k).unwrap().relations.push(r);
        }
    }
    for old in &snapshot.documents {
        let mut d = old.clone();
        d.id = docs[&old.id].clone();
        if let Some(o) = &mut d.owner {
            mapped(o, &ids)
        }
        if let Some(p) = &mut d.parent {
            mapped(p, &docs)
        }
        for l in &mut d.links {
            resolution(&mut l.target, &ids)
        }
        d.provenance = prov_ids[&d.provenance].clone();
        if let Some(sink) = output.as_deref_mut() {
            sink.push(
                old.owner
                    .as_deref()
                    .and_then(|id| entity_units.get(id))
                    .map(String::as_str)
                    .unwrap_or("$project"),
                &key(Some(&d.source)),
                super::normalized::Record::Document(d),
            )?;
        } else {
            shards
                .get_mut(&key(Some(&d.source)))
                .unwrap()
                .documents
                .push(d);
        }
    }
    macro_rules! canonical_set {
        ($items:expr,$kind:literal) => {{
            $items.sort_by(|a, b| a.id.cmp(&b.id));
            for pair in $items.windows(2) {
                if pair[0].id == pair[1].id
                    && serde_json::to_vec(&pair[0])? != serde_json::to_vec(&pair[1])?
                {
                    return Err(Error::new(
                        "ConflictingSemanticIdentity",
                        format!("conflicting canonical {} {}", $kind, pair[0].id),
                    ));
                }
            }
            $items.dedup_by(|a, b| a.id == b.id);
        }};
    }
    for shard in shards.values_mut() {
        canonical_set!(shard.entities, "entity");
        canonical_set!(shard.bodies, "body");
        canonical_set!(shard.occurrences, "occurrence");
        canonical_set!(shard.relations, "relation");
        canonical_set!(shard.documents, "document");
    }
    let mut cfgs: Vec<_> = units.values().collect();
    cfgs.sort();
    cfgs.dedup();
    let mut producers: Vec<_> = snapshot
        .providers
        .iter()
        .map(|p| (&p.name, &p.version, &p.binary_digest))
        .collect();
    producers.sort();
    producers.dedup();
    let universe = digest(&serde_json::to_vec(&(
        &snapshot.project,
        &snapshot.root,
        cfgs,
        producers,
    ))?);
    let manifest = manifest(&shards)?;
    let generation = generation(&universe, &manifest)?;
    Ok(Generation {
        version: 1,
        project: snapshot.project.clone(),
        root: snapshot.root.clone(),
        universe,
        sequence,
        generation,
        manifest,
        shards,
        provenance,
    })
}
