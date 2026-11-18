//! Multi-pass, record-at-a-time identity validation and canonical projection.
//! The only global structures are identity, ownership and interval indexes.
use super::{
    generation::{mapped, rebind, resolution},
    normalized::{self, Sink},
    production::{Production, ProvenanceObservation, UnitObservation},
    records::{Record, Source},
};
use crate::{Error, Result, model::*, ports::ContentReader};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
fn invalid(message: &str) -> Error {
    Error::new("InvalidAnalysisRecords", message)
}
fn targets(r: &Resolution) -> Vec<&str> {
    match r {
        Resolution::Resolved(id) => vec![id],
        Resolution::Candidates(v) => v.iter().map(String::as_str).collect(),
        _ => vec![],
    }
}
fn dangling_entity(e: &Entity, ids: &HashSet<String>) -> Error {
    let missing = e
        .parent
        .iter()
        .chain(e.type_definitions.iter())
        .filter(|id| !ids.contains(*id))
        .take(16)
        .collect::<Vec<_>>();
    let context = serde_json::json!({
        "entity": e.id,
        "unit": e.unit,
        "native_token": e.attributes.get("native_token"),
        "name": e.qualified_name.chars().take(512).collect::<String>(),
        "source": e.source,
        "missing": missing,
    });
    invalid(&format!("dangling entity parent/type: {context}"))
}
pub fn validate(
    header: &SemanticSnapshot,
    source: &dyn Source,
    content: &dyn ContentReader,
) -> Result<()> {
    super::identity::compilation_identities(&header.units, &header.root.display())?;
    super::validate::validate_metadata(header, content)?;
    let mut ids = HashSet::new();
    let mut bodies = HashSet::new();
    let mut occurrences = HashSet::new();
    let mut documents = HashSet::new();
    let mut parents = HashMap::new();
    for family in ["entity", "body", "occurrence", "document"] {
        source.visit_family(family, &mut |r| {
            if r.id().is_empty() {
                return Err(invalid("empty identity"));
            }
            match r {
                Record::Entity(e) => {
                    ids.insert(e.id.clone());
                    if let Some(p) = e.parent {
                        parents.insert(e.id, p);
                    }
                }
                Record::Body(b) => {
                    bodies.insert(b.id);
                }
                Record::Occurrence(o) => {
                    occurrences.insert(o.id);
                }
                Record::Document(d) => {
                    documents.insert(d.id);
                }
                _ => {}
            }
            Ok(())
        })?;
        // Catch missing containers before reading millions of occurrences and
        // relationships. Resolve details only for the first invalid entity.
        if family == "entity"
            && let Some((child, _)) = parents
                .iter()
                .filter(|(_, parent)| !ids.contains(*parent))
                .min_by_key(|(child, _)| *child)
            && let Some(Record::Entity(e)) = source.get("entity", child)?
        {
            return Err(dangling_entity(&e, &ids));
        }
    }
    let files = header
        .sources
        .files
        .iter()
        .map(|f| (f.id.as_str(), f.blob.len))
        .collect::<HashMap<_, _>>();
    let anchor = |a: &SourceAnchor| -> Result<()> {
        if files
            .get(a.file_revision.as_str())
            .is_none_or(|len| a.start > a.end || a.end > *len)
        {
            return Err(invalid("source range absent or outside frozen bytes"));
        }
        Ok(())
    };
    source.visit(&mut |r| {
        if r.id().is_empty() {
            return Err(invalid("empty identity"));
        }
        let provenance = match &r {
            Record::Entity(v) => Some(&v.provenance),
            Record::Occurrence(v) => Some(&v.provenance),
            Record::Relation(v) => Some(&v.provenance),
            Record::Document(v) => Some(&v.provenance),
            _ => None,
        };
        if provenance.is_some_and(|p| !header.provenance_records.contains_key(p)) {
            return Err(invalid("dangling provenance"));
        }
        match r {
            Record::Entity(e) => {
                for a in e
                    .source
                    .iter()
                    .chain(e.extent.iter())
                    .chain(e.locations.anchors())
                {
                    anchor(a)?;
                }
                for nav in &e.type_navigation {
                    anchor(&nav.source)?;
                    if let Some(a) = &nav.extent {
                        anchor(a)?;
                    }
                    if nav.candidates.iter().any(|id| !ids.contains(id)) {
                        return Err(invalid("dangling type candidate"));
                    }
                }
                if e.parent.as_ref().is_some_and(|p| !ids.contains(p))
                    || e.type_definitions.iter().any(|p| !ids.contains(p))
                {
                    return Err(dangling_entity(&e, &ids));
                }
            }
            Record::Body(b) => {
                anchor(&b.source)?;
                if !ids.contains(&b.owner) || b.parent.as_ref().is_some_and(|p| !bodies.contains(p))
                {
                    return Err(invalid("dangling body owner/parent"));
                }
            }
            Record::Occurrence(o) => {
                anchor(&o.source)?;
                for a in o.locations.anchors().chain(o.arguments.iter()) {
                    anchor(a)?;
                }
                if o.owner
                    .as_ref()
                    .is_some_and(|p| !ids.contains(p) && !bodies.contains(p))
                {
                    return Err(invalid("dangling occurrence owner"));
                }
                for t in targets(&o.declaration)
                    .into_iter()
                    .chain(targets(&o.executable))
                {
                    if !ids.contains(t) && !bodies.contains(t) {
                        return Err(invalid("dangling occurrence target"));
                    }
                }
                for b in &o.argument_bindings {
                    anchor(&b.source)?;
                    if targets(&b.parameter).iter().any(|t| !ids.contains(*t)) {
                        return Err(invalid("dangling argument binding"));
                    }
                }
            }
            Record::Relation(r) => {
                if !ids.contains(&r.from)
                    || !ids.contains(&r.to)
                    || r.occurrence
                        .as_ref()
                        .is_some_and(|o| !occurrences.contains(o))
                {
                    return Err(Error::new(
                        "DanglingRelation",
                        format!("{} -> {} ({:?})", r.from, r.to, r.kind),
                    ));
                }
                if let Some(a) = &r.source {
                    anchor(a)?;
                }
            }
            Record::Document(d) => {
                anchor(&d.source)?;
                if d.owner.as_ref().is_some_and(|o| !ids.contains(o))
                    || d.parent.as_ref().is_some_and(|p| !documents.contains(p))
                {
                    return Err(invalid("dangling document owner/parent"));
                }
            }
        }
        Ok(())
    })?;
    let mut done = HashSet::new();
    for child in parents.keys() {
        let mut chain = HashSet::new();
        let mut at = child;
        while let Some(p) = parents.get(at) {
            if done.contains(at) {
                break;
            }
            if !chain.insert(at) {
                return Err(Error::new("ParentCycle", "declaring-container cycle"));
            }
            at = p;
        }
        done.extend(chain);
    }
    Ok(())
}
struct Live<'a> {
    source: &'a dyn Source,
    failed: BTreeSet<String>,
    ids: HashSet<String>,
    bodies: HashSet<String>,
    occurrences: HashSet<String>,
    documents: HashSet<String>,
    partial: bool,
}
fn available(r: &mut Resolution, ids: &HashSet<String>) {
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
impl<'a> Live<'a> {
    fn new(source: &'a dyn Source, p: &Production) -> Result<Self> {
        let mut s = Self {
            source,
            failed: p.failed_units.keys().cloned().collect(),
            ids: HashSet::new(),
            bodies: HashSet::new(),
            occurrences: HashSet::new(),
            documents: HashSet::new(),
            partial: p.partial,
        };
        if !s.partial {
            return Ok(s);
        }
        source.visit_family("entity", &mut |r| {
            if let Record::Entity(e) = r
                && (!s.partial || !s.failed.contains(&e.unit))
            {
                s.ids.insert(e.id);
            }
            Ok(())
        })?;
        for family in ["body", "occurrence", "document"] {
            source.visit_family(family, &mut |r| {
                match r {
                    Record::Body(b)
                        if !s.partial
                            || !s.failed.contains(&b.unit) && s.ids.contains(&b.owner) =>
                    {
                        s.bodies.insert(b.id);
                    }
                    Record::Occurrence(o)
                        if !s.partial
                            || !s.failed.contains(&o.unit)
                                && o.owner.as_ref().is_none_or(|id| s.ids.contains(id)) =>
                    {
                        s.occurrences.insert(o.id);
                    }
                    Record::Document(d)
                        if !s.partial || d.owner.as_ref().is_none_or(|id| s.ids.contains(id)) =>
                    {
                        s.documents.insert(d.id);
                    }
                    _ => {}
                }
                Ok(())
            })?;
        }
        Ok(s)
    }
    fn filter(&self, mut r: Record) -> Option<Record> {
        if !self.partial {
            return Some(r);
        }
        match &mut r {
            Record::Entity(e) => {
                if !self.ids.contains(&e.id) {
                    return None;
                }
                if e.parent.as_ref().is_some_and(|p| !self.ids.contains(p)) {
                    e.parent = None;
                }
                e.type_definitions.retain(|p| self.ids.contains(p));
                for nav in &mut e.type_navigation {
                    nav.candidates.retain(|p| self.ids.contains(p));
                }
            }
            Record::Body(b) => {
                if !self.bodies.contains(&b.id) {
                    return None;
                }
                if b.parent.as_ref().is_some_and(|p| !self.bodies.contains(p)) {
                    b.parent = None;
                }
            }
            Record::Occurrence(o) => {
                if !self.occurrences.contains(&o.id) {
                    return None;
                }
                available(&mut o.declaration, &self.ids);
                available(&mut o.executable, &self.ids);
                for b in &mut o.argument_bindings {
                    available(&mut b.parameter, &self.ids);
                }
            }
            Record::Relation(r) => {
                if !self.ids.contains(&r.from)
                    || !self.ids.contains(&r.to)
                    || r.occurrence
                        .as_ref()
                        .is_some_and(|o| !self.occurrences.contains(o))
                {
                    return None;
                }
            }
            Record::Document(d) => {
                if !self.documents.contains(&d.id) {
                    return None;
                }
                if d.parent
                    .as_ref()
                    .is_some_and(|p| !self.documents.contains(p))
                {
                    d.parent = None;
                }
                for l in &mut d.links {
                    available(&mut l.target, &self.ids);
                }
            }
        }
        Some(r)
    }
}
impl Source for Live<'_> {
    fn get(&self, f: &str, id: &str) -> Result<Option<Record>> {
        Ok(self.source.get(f, id)?.and_then(|r| self.filter(r)))
    }
    fn visit(&self, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        self.source.visit(&mut |r| {
            if let Some(r) = self.filter(r) {
                f(r)?;
            }
            Ok(())
        })
    }
    fn visit_family(&self, family: &str, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        self.source.visit_family(family, &mut |r| {
            if let Some(r) = self.filter(r) {
                f(r)?;
            }
            Ok(())
        })
    }
}
pub fn canonicalize(
    header: &mut SemanticSnapshot,
    source: &dyn Source,
    sink: &mut dyn Sink,
) -> Result<normalized::Projection> {
    let started = std::time::Instant::now();
    let root = header.root.display();
    let contexts = super::identity::compilation_identities(&header.units, &root)?;
    contexts.report(header);
    let units = contexts.units;
    let handles = super::identity::Handles::new(header, source)?;
    let mut p = header.production.take().unwrap_or_default();
    p.unit_observations = header
        .units
        .iter()
        .map(|u| UnitObservation {
            unit: units[&u.id].clone(),
            observed: u.clone(),
        })
        .collect();
    p.provenance_observations = header
        .provenance_records
        .values()
        .map(|v| {
            let mut c = v.clone();
            c.build_run = None;
            Ok(ProvenanceObservation {
                canonical: ProvenanceId(digest(&serde_json::to_vec(&c)?)),
                observed: v.clone(),
            })
        })
        .collect::<Result<_>>()?;
    let live = Live::new(source, &p)?;
    let source = &live as &dyn Source;
    let mut ids = HashMap::new();
    let mut body_meta = HashMap::new();
    let mut entity_units = HashMap::new();
    let mut entity_keys = HashMap::new();
    let mut sources = BTreeMap::new();
    sources.insert("$metadata".into(), None);
    let files = header
        .sources
        .files
        .iter()
        .map(|f| (f.id.clone(), f.clone()))
        .collect::<HashMap<_, _>>();
    let key = |a: Option<&SourceAnchor>| {
        a.and_then(|a| files.get(&a.file_revision))
            .map(|f| f.logical_path.display())
            .unwrap_or_else(|| "$metadata".into())
    };
    let mut unmatched = 0;
    source.visit_family("entity", &mut |r| {
        if let Record::Entity(mut e) = r {
            handles.apply(&mut e);
            if e.attributes.contains_key("identity_scope_reason")
                || e.source.is_none()
                    && e.stable_handle
                        .as_ref()
                        .is_some_and(|h| h.starts_with("cqg1:"))
            {
                unmatched += 1;
            }
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
        Ok(())
    })?;
    source.visit_family("body", &mut |r| {
        if let Record::Body(b) = r {
            ids.insert(
                b.id.clone(),
                digest(&serde_json::to_vec(&(
                    ids.get(&b.owner),
                    b.kind,
                    &b.source,
                ))?),
            );
            body_meta.insert(b.id, (b.owner, b.source));
        }
        Ok(())
    })?;
    let mut docs = HashMap::new();
    source.visit_family("document", &mut |r| {
        if let Record::Document(d) = r {
            docs.insert(
                d.id.clone(),
                digest(&serde_json::to_vec(&(
                    &d.source,
                    d.kind,
                    &d.title,
                    &d.text,
                    d.owner.as_ref().and_then(|o| ids.get(o)),
                ))?),
            );
        }
        Ok(())
    })?;
    for (old, id) in &docs {
        if ids.contains_key(old) {
            ids.insert(old.clone(), id.clone());
        }
    }
    let mut provenance = BTreeMap::new();
    let mut prov_ids = HashMap::new();
    for (id, old) in &header.provenance_records {
        let mut v = old.clone();
        v.build_run = None;
        let next = ProvenanceId(digest(&serde_json::to_vec(&v)?));
        prov_ids.insert(id.clone(), next.clone());
        provenance.insert(next, v);
    }
    let mut referenced = BTreeSet::new();
    source.visit(&mut |r| {
        match &r {
            Record::Entity(e) => {
                for a in e
                    .source
                    .iter()
                    .chain(e.extent.iter())
                    .chain(e.locations.anchors())
                {
                    referenced.insert(a.file_revision.clone());
                }
                for nav in &e.type_navigation {
                    referenced.insert(nav.source.file_revision.clone());
                    if let Some(a) = &nav.extent {
                        referenced.insert(a.file_revision.clone());
                    }
                }
            }
            Record::Body(b) => {
                referenced.insert(b.source.file_revision.clone());
            }
            Record::Occurrence(o) => {
                for a in std::iter::once(&o.source)
                    .chain(o.locations.anchors())
                    .chain(o.arguments.iter())
                    .chain(o.argument_bindings.iter().map(|b| &b.source))
                {
                    referenced.insert(a.file_revision.clone());
                }
            }
            Record::Relation(r) => {
                if let Some(a) = &r.source {
                    referenced.insert(a.file_revision.clone());
                }
            }
            Record::Document(d) => {
                referenced.insert(d.source.file_revision.clone());
            }
        }
        Ok(())
    })?;
    for id in referenced {
        if let Some(f) = files.get(&id) {
            sources.insert(f.logical_path.display(), Some(f.clone()));
        }
    }
    source.visit_family("entity", &mut |r| {
        if let Record::Entity(mut e) = r {
            let old_id = e.id.clone();
            let old_unit = e.unit.clone();
            handles.apply(&mut e);
            e.id = ids[&old_id].clone();
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
                e.qualified_name = e.qualified_name.replace(&old_unit, &e.unit);
            }
            if matches!(e.kind, EntityKind::Document | EntityKind::DocSection) {
                e.provider_id = e.id.clone();
                e.qualified_name = format!("doc::{}", e.id);
            }
            if e.kind == EntityKind::Body {
                e.provider_id = e.id.clone();
                if let Some((owner, span)) = body_meta.get(&old_id) {
                    e.qualified_name = format!("{}::body@{}", ids[owner], span.start);
                }
            }
            if let Some(parent) = &mut e.parent {
                mapped(parent, &ids);
            }
            for t in &mut e.type_definitions {
                mapped(t, &ids);
            }
            for nav in &mut e.type_navigation {
                for t in &mut nav.candidates {
                    mapped(t, &ids);
                }
            }
            if let Some(c) = e.attributes.get_mut("definition_candidates") {
                let mut v = serde_json::from_str(c)?;
                rebind(&mut v, &ids);
                *c = serde_json::to_string(&v)?;
            }
            e.provenance = prov_ids[&e.provenance].clone();
            let k = if e.source.is_none() && !e.unit.is_empty() {
                format!("$metadata:{}", e.unit)
            } else {
                key(e.source.as_ref())
            };
            sources.entry(k.clone()).or_insert(None);
            entity_units.insert(old_id.clone(), e.unit.clone());
            entity_keys.insert(old_id, k.clone());
            sink.push(&e.unit.clone(), &k, Record::Entity(e))?;
        }
        Ok(())
    })?;
    source.visit_family("body", &mut |r| {
        if let Record::Body(mut b) = r {
            b.id = ids[&b.id].clone();
            mapped(&mut b.unit, &units);
            mapped(&mut b.owner, &ids);
            if let Some(parent) = &mut b.parent {
                mapped(parent, &ids);
            }
            sink.push(&b.unit.clone(), &key(Some(&b.source)), Record::Body(b))?;
        }
        Ok(())
    })?;
    let mut occurrences = HashMap::new();
    source.visit_family("occurrence", &mut |r| {
        if let Record::Occurrence(mut o) = r {
            let old = o.id.clone();
            mapped(&mut o.unit, &units);
            if let Some(owner) = &mut o.owner {
                mapped(owner, &ids);
            }
            resolution(&mut o.declaration, &ids);
            resolution(&mut o.executable, &ids);
            for b in &mut o.argument_bindings {
                resolution(&mut b.parameter, &ids);
            }
            o.provenance = prov_ids[&o.provenance].clone();
            o.id.clear();
            o.id = digest(&serde_json::to_vec(&o)?);
            occurrences.insert(old, o.id.clone());
            sink.push(
                &o.unit.clone(),
                &key(Some(&o.source)),
                Record::Occurrence(o),
            )?;
        }
        Ok(())
    })?;
    source.visit_family("relation", &mut |r| {
        if let Record::Relation(mut r) = r {
            let unit = entity_units
                .get(&r.from)
                .map(String::as_str)
                .unwrap_or("$project");
            let k = r.source.as_ref().map(|a| key(Some(a))).unwrap_or_else(|| {
                entity_keys
                    .get(&r.from)
                    .cloned()
                    .unwrap_or_else(|| "$metadata".into())
            });
            mapped(&mut r.from, &ids);
            mapped(&mut r.to, &ids);
            if let Some(o) = &mut r.occurrence {
                mapped(o, &occurrences);
            }
            r.provenance = prov_ids[&r.provenance].clone();
            r.id.clear();
            r.id = digest(&serde_json::to_vec(&r)?);
            sink.push(unit, &k, Record::Relation(r))?;
        }
        Ok(())
    })?;
    source.visit_family("document", &mut |r| {
        if let Record::Document(mut d) = r {
            let unit = d
                .owner
                .as_ref()
                .and_then(|id| entity_units.get(id))
                .map(String::as_str)
                .unwrap_or("$project");
            d.id = docs[&d.id].clone();
            if let Some(o) = &mut d.owner {
                mapped(o, &ids);
            }
            if let Some(parent) = &mut d.parent {
                mapped(parent, &docs);
            }
            for l in &mut d.links {
                resolution(&mut l.target, &ids);
            }
            d.provenance = prov_ids[&d.provenance].clone();
            sink.push(unit, &key(Some(&d.source)), Record::Document(d))?;
        }
        Ok(())
    })?;
    let mut cfgs = header
        .units
        .iter()
        .map(|u| super::identity::unit_key(u, &root))
        .collect::<Result<Vec<_>>>()?;
    cfgs.sort();
    cfgs.dedup();
    let mut producers = header
        .providers
        .iter()
        .map(|v| (&v.name, &v.version, &v.binary_digest))
        .collect::<Vec<_>>();
    producers.sort();
    producers.dedup();
    let universe = digest(&serde_json::to_vec(&(
        &header.project,
        &header.root,
        cfgs,
        producers,
    ))?);
    if p.partial {
        for c in &mut header.contexts {
            if c.units.iter().any(|u| p.failed_units.contains_key(u)) {
                c.status = Support::Partial;
                c.gaps.push(Gap::new(
                    "ScopeAnalysisFailed",
                    &c.id,
                    "one or more selected units failed; absence does not establish nonexistence",
                ));
            }
        }
    }
    for u in &mut header.units {
        mapped(&mut u.id, &units);
    }
    for c in &mut header.contexts {
        mapped(&mut c.entry_unit, &units);
        c.id = digest(c.entry_unit.as_bytes());
        for id in &mut c.units {
            mapped(id, &units);
        }
        c.units.sort();
        c.units.dedup();
    }
    for d in &mut header.build_dependencies {
        mapped(&mut d.consumer, &units);
        if let Observed::Value(v) = &mut d.producer {
            mapped(v, &units);
        }
    }
    p.failed_units = p
        .failed_units
        .into_iter()
        .map(|(mut id, reason)| {
            mapped(&mut id, &units);
            (id, reason)
        })
        .collect();
    for gap in &mut header.gaps {
        if gap.code == "ScopeAnalysisFailed" {
            mapped(&mut gap.scope, &units);
        }
    }
    p.identity_unmatched = unmatched;
    p.streamed = true;
    p.transport
        .insert("project_semantic_snapshot".into(), serde_json::json!(false));
    p.transport
        .insert("record_source".into(), serde_json::json!("journal"));
    p.stage_ms.insert(
        "canonical_payloads".into(),
        started.elapsed().as_millis() as u64,
    );
    header.provenance_records = provenance.clone();
    header.production = Some(p);
    Ok(normalized::Projection {
        universe,
        provenance,
        sources,
    })
}
