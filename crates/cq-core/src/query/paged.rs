//! Bounded query execution over a fixed FactReader; no project-wide adjacency
//! table or SemanticSnapshot is constructed on this path.
use super::*;
use crate::{
    Error, Result,
    ports::facts::{FactReader, FactRow, group},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    text: Option<text::TextPlan>,
    #[serde(default)]
    ranking_after: Option<(f64, String)>,
    #[serde(default)]
    rank_more: bool,
    #[serde(default)]
    query_vector: Option<Vec<f32>>,
    #[serde(default)]
    code: Option<CodeScan>,
    #[serde(default)]
    at: Option<(String, u64)>,
    #[serde(default)]
    extra_gaps: Vec<Gap>,
    #[serde(default)]
    input: VecDeque<FactRow>,
    groups: Vec<String>,
    group_index: usize,
    after: Option<String>,
    exhausted: bool,
    pending: VecDeque<Value>,
    ranked: Vec<Value>,
    ranking_done: bool,
    skip: usize,
    walk: VecDeque<Step>,
    visited: BTreeSet<String>,
    scheduled: BTreeSet<String>,
    target: Option<String>,
    #[serde(default)]
    subject_id: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct CodeScan {
    file: FileRevision,
    offset: u64,
    start: u64,
    line: usize,
    carry: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Step {
    id: String,
    hop: usize,
    path: Vec<String>,
    via: Option<String>,
    reported: bool,
    after: Option<String>,
    direction: usize,
}
mod text;

const RANK_WINDOW: usize = 1000;
fn rank_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    b["score"]
        .as_f64()
        .unwrap_or(0.)
        .total_cmp(&a["score"].as_f64().unwrap_or(0.))
        .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
}
fn retain_ranked(state: &mut State, value: Value) {
    if let Some((score, id)) = &state.ranking_after {
        let s = value["score"].as_f64().unwrap_or(0.);
        if s > *score || s == *score && value["id"].as_str().unwrap_or("") <= id.as_str() {
            return;
        }
    }
    match state.ranked.binary_search_by(|a| rank_order(a, &value)) {
        Ok(_) => {}
        Err(at) => {
            state.ranked.insert(at, value);
            if state.ranked.len() > RANK_WINDOW {
                state.ranked.pop();
                state.rank_more = true;
            }
        }
    }
}
pub struct Engine<'a> {
    pub reader: &'a dyn FactReader,
    pub model: Option<Arc<embeddings::TokenVectors>>,
    pub control: Option<&'a crate::control::CaptureControl>,
}
impl<'a> Engine<'a> {
    fn value(&self, family: &str, id: &str) -> Result<Option<Value>> {
        Ok(self.reader.get(family, id)?.map(|r| r.value))
    }
    fn entity(&self, id: &str) -> Result<Option<Entity>> {
        self.value("entity", id)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }
    fn file(&self, id: &str) -> Result<Option<FileRevision>> {
        self.value("file", id)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }
    fn is_nested_body(e: &Entity) -> bool {
        e.kind == EntityKind::Body && !matches!(e.name.as_str(), "Function" | "AsyncFunction")
    }
    fn root_prefix(&self) -> Option<String> {
        let root = self.reader.metadata()["root"]["display"]
            .as_str()
            .or_else(|| self.reader.metadata()["root"].as_str())?;
        if root.is_empty() {
            None
        } else {
            Some(format!("{}/", root.trim_end_matches('/')))
        }
    }
    fn display_path(&self, file: &FileRevision, compact: bool) -> String {
        let p = file.logical_path.display();
        if compact
            && let Some(prefix) = self.root_prefix()
            && let Some(stripped) = p.strip_prefix(&prefix)
        {
            return stripped.to_owned();
        }
        p
    }
    fn anchor_location(
        &self,
        anchor: &SourceAnchor,
        compact: bool,
    ) -> Result<Option<AnchorLocation>> {
        let Some(f) = self.file(&anchor.file_revision)? else {
            return Ok(None);
        };
        let path = self.display_path(&f, compact);
        let pos = self
            .reader
            .source_position(&f.id, anchor.start, f.blob.len)
            .ok()
            .flatten();
        Ok(Some((path, pos)))
    }
    fn enclosing_owner(&self, id: &str) -> Result<Option<(Entity, Option<String>)>> {
        let Some(e) = self.entity(id)? else {
            return Ok(None);
        };
        if e.kind == EntityKind::Body {
            let body_kind =
                (!matches!(e.name.as_str(), "Function" | "AsyncFunction")).then(|| e.name.clone());
            if let Some(parent_id) = e.parent.as_deref()
                && let Some(owner) = self.entity(parent_id)?
            {
                return Ok(Some((owner, body_kind)));
            }
            return Ok(Some((e, body_kind)));
        }
        Ok(Some((e, None)))
    }
    fn entity_label(&self, id: &str) -> Result<String> {
        if let Some(e) = self.entity(id)? {
            if e.kind == EntityKind::Body
                && let Some((owner, _)) = self.enclosing_owner(id)?
                && owner.id != e.id
            {
                let owner_name = if owner.qualified_name.is_empty() {
                    &owner.name
                } else {
                    &owner.qualified_name
                };
                return Ok(format!("{}({})", e.name, owner_name));
            }
            return Ok(if e.qualified_name.is_empty() {
                e.name
            } else {
                e.qualified_name
            });
        }
        Ok(id.to_owned())
    }
    fn body_has_matching_call(
        &self,
        body_id: &str,
        q: &QueryRequest,
        ctx: Option<&AnalysisContext>,
    ) -> Result<bool> {
        let mut after = None;
        let mut checked = 0usize;
        while checked < 1024 {
            let batch = self
                .reader
                .scan(&group("out", body_id), after.as_deref(), 64)?;
            for row in batch.rows {
                checked += 1;
                let r: Relation = serde_json::from_value(row.value)?;
                if !self.allowed(&r.provenance, q)? {
                    continue;
                }
                if (r.kind == RelationKind::Calls
                    || (q.include_candidates
                        && matches!(r.kind, RelationKind::MayCall | RelationKind::Dispatches)))
                    && let Some(target) = self.entity(&r.to)?
                    && self.selected(&target, q, ctx)?
                {
                    return Ok(true);
                }
            }
            if batch.complete {
                break;
            }
            after = batch.after;
        }
        Ok(false)
    }
    fn summary(&self, e: &Entity) -> Result<Value> {
        self.summary_for(e, false)
    }
    fn summary_for(&self, e: &Entity, compact: bool) -> Result<Value> {
        let owner_info = if e.kind == EntityKind::Body {
            e.parent
                .as_deref()
                .map(|p| self.entity(p))
                .transpose()?
                .flatten()
        } else {
            None
        };
        if compact {
            let loc = e
                .source
                .as_ref()
                .map(|a| self.anchor_location(a, true))
                .transpose()?
                .flatten();
            let mut map = serde_json::Map::new();
            map.insert("id".into(), json!(e.id));
            map.insert("kind".into(), json!(e.kind));
            map.insert("name".into(), json!(e.name));
            map.insert("unit".into(), json!(e.unit));
            if let Some(unit) = self.value("unit", &e.unit)? {
                let unit: BuildUnit = serde_json::from_value(unit)?;
                map.insert(
                    "build_context".into(),
                    super::presentation::symbol_context(&unit),
                );
            }
            if let Some(a) = &e.source {
                map.insert("file_revision".into(), json!(a.file_revision));
            }
            if !e.qualified_name.is_empty() && e.qualified_name != e.name {
                map.insert("qualified_name".into(), json!(e.qualified_name));
            }
            if let Some((path, pos)) = loc {
                map.insert("file".into(), json!(path));
                if let Some((line, _, _)) = pos {
                    map.insert("line".into(), json!(line));
                }
            }
            let sig: String = e.signature.trim().chars().take(200).collect();
            if !sig.is_empty() {
                map.insert("signature".into(), json!(sig));
            }
            if e.exported {
                map.insert("exported".into(), json!(true));
            }
            if e.external {
                map.insert("external".into(), json!(true));
            }
            if !e.locations.is_empty() {
                map.insert("locations".into(), json!(e.locations));
            }
            if let Some(owner) = owner_info {
                map.insert("owner_id".into(), json!(owner.id));
                map.insert(
                    "owner_name".into(),
                    json!(if owner.qualified_name.is_empty() {
                        &owner.name
                    } else {
                        &owner.qualified_name
                    }),
                );
                map.insert("owner_kind".into(), json!(owner.kind));
            }
            return Ok(Value::Object(map));
        }
        let mut value = json!({"id":e.id,"unit":e.unit,"kind":e.kind,"name":e.name,"qualified_name":e.qualified_name,"file":e.source.as_ref().map(|a|self.file(&a.file_revision)).transpose()?.flatten().map(|f|f.logical_path.display()),"source":e.source,"locations":e.locations,"stable_handle":crate::semantic::identity::query_handle(e),"signature":e.signature.chars().take(512).collect::<String>(),"declaring_scope":e.parent,"external":e.external,"exported":e.exported,"provenance":e.provenance});
        if let Some(owner) = owner_info {
            value["owner_id"] = json!(owner.id);
            value["owner_name"] = json!(if owner.qualified_name.is_empty() {
                &owner.name
            } else {
                &owner.qualified_name
            });
            value["owner_kind"] = json!(owner.kind);
        }
        Ok(value)
    }
    fn format_occurrence(&self, o: &Occurrence, subject_id: &str, compact: bool) -> Result<Value> {
        let loc = self.anchor_location(&o.source, compact)?;
        let owner_resolved = o
            .owner
            .as_deref()
            .map(|id| self.enclosing_owner(id))
            .transpose()?
            .flatten();
        let owner_name = owner_resolved.as_ref().map(|(e, _)| {
            if e.qualified_name.is_empty() {
                e.name.as_str()
            } else {
                e.qualified_name.as_str()
            }
        });
        let owner_id = owner_resolved.as_ref().map(|(e, _)| e.id.as_str());
        let owner_kind = owner_resolved.as_ref().map(|(e, _)| e.kind);
        let body_kind = owner_resolved.as_ref().and_then(|(_, bk)| bk.as_deref());
        if compact {
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), json!(o.kind));
            map.insert("file_revision".into(), json!(o.source.file_revision));
            if let Some((path, pos)) = loc {
                map.insert("file".into(), json!(path));
                if let Some((line, col, snippet)) = pos {
                    map.insert("line".into(), json!(line));
                    map.insert("column".into(), json!(col));
                    if !snippet.is_empty() {
                        map.insert("snippet".into(), json!(snippet));
                    }
                }
            }
            if let Some(name) = owner_name {
                map.insert("owner".into(), json!(name));
            }
            if let Some(id) = owner_id {
                map.insert("owner_id".into(), json!(id));
            }
            if let Some(k) = owner_kind {
                map.insert("owner_kind".into(), json!(k));
            }
            if let Some(bk) = body_kind {
                map.insert("body_kind".into(), json!(bk));
            }
            if o.evaluation != Evaluation::Immediate {
                map.insert("evaluation".into(), json!(o.evaluation));
            }
            if o.dispatch != Dispatch::Direct {
                map.insert("dispatch".into(), json!(o.dispatch));
            }
            if o.declaration.resolved() != Some(subject_id) {
                if let Some(target_id) = o.declaration.resolved() {
                    map.insert("target_id".into(), json!(target_id));
                    map.insert("target".into(), json!(self.entity_label(target_id)?));
                } else {
                    map.insert("declaration".into(), json!(o.declaration));
                }
            }
            if !o.locations.is_empty() {
                map.insert("locations".into(), json!(o.locations));
            }
            return Ok(Value::Object(map));
        }
        let mut value = json!(o);
        if let Some((path, pos)) = loc {
            value["file"] = json!(path);
            if let Some((line, col, snippet)) = pos {
                value["line"] = json!(line);
                value["column"] = json!(col);
                if !snippet.is_empty() {
                    value["snippet"] = json!(snippet);
                }
            }
        }
        if let Some(name) = owner_name {
            value["owner_name"] = json!(name);
        }
        if let Some(id) = owner_id {
            value["owner_id"] = json!(id);
        }
        if let Some(k) = owner_kind {
            value["owner_kind"] = json!(k);
        }
        if let Some(bk) = body_kind {
            value["body_kind"] = json!(bk);
        }
        Ok(value)
    }
    fn format_via(&self, r: &Relation, compact: bool) -> Result<Value> {
        if !compact {
            return Ok(json!(r));
        }
        let mut map = serde_json::Map::new();
        map.insert("kind".into(), json!(r.kind));
        map.insert("from".into(), json!(self.entity_label(&r.from)?));
        map.insert("to".into(), json!(self.entity_label(&r.to)?));
        let occ: Option<Occurrence> = r
            .occurrence
            .as_deref()
            .map(|id| self.value("occurrence", id))
            .transpose()?
            .flatten()
            .map(serde_json::from_value)
            .transpose()?;
        let anchor = occ.as_ref().map(|o| &o.source).or(r.source.as_ref());
        if let Some(a) = anchor
            && let Some((path, pos)) = self.anchor_location(a, true)?
        {
            map.insert("file_revision".into(), json!(a.file_revision));
            map.insert("file".into(), json!(path));
            if let Some((line, _, snippet)) = pos {
                map.insert("line".into(), json!(line));
                if !snippet.is_empty() {
                    map.insert("snippet".into(), json!(snippet));
                }
            }
        }
        if let Some(o) = occ {
            if o.evaluation != Evaluation::Immediate {
                map.insert("evaluation".into(), json!(o.evaluation));
            }
            if o.dispatch != Dispatch::Direct {
                map.insert("dispatch".into(), json!(o.dispatch));
            }
        }
        Ok(Value::Object(map))
    }
    fn format_relation_row(
        &self,
        subject_id: &str,
        target: &Entity,
        r: &Relation,
        compact: bool,
    ) -> Result<Value> {
        let occ: Option<Occurrence> = r
            .occurrence
            .as_deref()
            .map(|id| self.value("occurrence", id))
            .transpose()?
            .flatten()
            .map(serde_json::from_value)
            .transpose()?;
        if compact {
            let mut map = serde_json::Map::new();
            map.insert("entity".into(), self.summary_for(target, true)?);
            map.insert("relation".into(), json!(r.kind));
            let anchor = occ.as_ref().map(|o| &o.source).or(r.source.as_ref());
            if let Some(a) = anchor
                && let Some((path, pos)) = self.anchor_location(a, true)?
            {
                map.insert("file_revision".into(), json!(a.file_revision));
                map.insert("file".into(), json!(path));
                if let Some((line, col, snippet)) = pos {
                    map.insert("line".into(), json!(line));
                    map.insert("column".into(), json!(col));
                    if !snippet.is_empty() {
                        map.insert("snippet".into(), json!(snippet));
                    }
                }
            }
            if let Some(o) = occ {
                if o.evaluation != Evaluation::Immediate {
                    map.insert("evaluation".into(), json!(o.evaluation));
                }
                if o.dispatch != Dispatch::Direct {
                    map.insert("dispatch".into(), json!(o.dispatch));
                }
            }
            return Ok(Value::Object(map));
        }
        let occ_val = occ
            .as_ref()
            .map(|o| self.format_occurrence(o, subject_id, false))
            .transpose()?;
        Ok(json!({"entity":self.summary_for(target, false)?,"relation":r,"occurrence":occ_val}))
    }
    fn context(&self, q: &QueryRequest) -> Result<Option<AnalysisContext>> {
        let Some(id) = &q.context else {
            return Ok(None);
        };
        if let Some(v) = self.value("context", id)? {
            return Ok(Some(serde_json::from_value(v)?));
        }
        let mut after = None;
        loop {
            let p = self.reader.scan("all:context", after.as_deref(), 128)?;
            for row in p.rows {
                let c: AnalysisContext = serde_json::from_value(row.value)?;
                if c.entry_unit == *id {
                    return Ok(Some(c));
                }
            }
            if p.complete {
                break;
            }
            after = p.after;
        }
        Err(Error::new("MissingContext", id))
    }
    fn selected(
        &self,
        e: &Entity,
        q: &QueryRequest,
        ctx: Option<&AnalysisContext>,
    ) -> Result<bool> {
        if e.external && !q.include_external {
            return Ok(false);
        }
        if ctx.is_some_and(|c| !e.external && !c.units.contains(&e.unit)) {
            return Ok(false);
        }
        if q.kind
            .as_ref()
            .is_some_and(|k| !super::presentation::kind_matches(e.kind, k))
        {
            return Ok(false);
        }
        if q.file.is_some() || q.directory.is_some() {
            let f = e
                .source
                .as_ref()
                .map(|a| self.file(&a.file_revision))
                .transpose()?
                .flatten();
            let (path, directory) = q
                .file
                .as_ref()
                .map(|p| (p, false))
                .unwrap_or_else(|| (q.directory.as_ref().unwrap(), true));
            if !f.is_some_and(|f| {
                crate::ports::facts::path_matches(&f.logical_path.display(), path, directory)
            }) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn allowed(&self, p: &ProvenanceId, q: &QueryRequest) -> Result<bool> {
        if let Some(a) = q.authority {
            let p = self
                .value("provenance", &p.0)?
                .ok_or_else(|| Error::new("MissingProvenance", &p.0))?;
            Ok(serde_json::from_value::<Provenance>(p)?.authority == a)
        } else {
            Ok(true)
        }
    }
    pub fn resolve(&self, name: &str, q: &QueryRequest) -> Result<Entity> {
        let ctx = self.context(q)?;
        let mut identity = q.clone();
        identity.kind = None;
        identity.file = None;
        if let Some(e) = self.entity(name)? {
            identity.include_external = true;
            if self.selected(&e, &identity, ctx.as_ref())? {
                return Ok(e);
            }
        }
        if let Some(id) = name.strip_prefix("cqg1:")
            && let Some(e) = self.entity(id)?
            && crate::semantic::identity::query_handle(&e).as_deref() == Some(name)
        {
            identity.include_external = true;
            if self.selected(&e, &identity, ctx.as_ref())? {
                return Ok(e);
            }
        }
        let p = self.reader.scan(&group("lookup", name), None, 1000)?;
        let mut candidates = vec![];
        for row in p.rows {
            let e: Entity = serde_json::from_value(row.value)?;
            if e.stable_handle.as_deref() == Some(name) {
                identity.include_external = true;
            }
            if self.selected(&e, &identity, ctx.as_ref())? {
                candidates.push(e);
            }
        }
        if candidates.len() == 1 && p.complete {
            return Ok(candidates.remove(0));
        }
        if candidates.is_empty() && p.complete {
            return Err(Error::new("MissingSymbol", name));
        }
        let summaries = candidates
            .iter()
            .take(7)
            .map(|e| self.summary(e))
            .collect::<Result<Vec<_>>>()?;
        Err(Error::new("AmbiguousSymbol",json!({"candidates":summaries,"candidates_count":candidates.len(),"complete":p.complete,"continue_with":{"operation":"search","text":name}}).to_string()))
    }
    fn subject(&self, q: &QueryRequest) -> Result<Entity> {
        self.resolve(
            q.subject
                .as_deref()
                .ok_or_else(|| Error::new("MissingSubject", "select a symbol"))?,
            q,
        )
    }
    fn edge_allowed(&self, r: &Relation, q: &QueryRequest, walk: bool) -> Result<bool> {
        if !self.allowed(&r.provenance, q)? {
            return Ok(false);
        }
        Ok(if let Some(kind) = &q.relation {
            RelationKind::parse(kind) == Some(r.kind)
        } else if walk {
            match q.mode.as_str() {
                "types" => matches!(
                    r.kind,
                    RelationKind::UsesType
                        | RelationKind::Implements
                        | RelationKind::Extends
                        | RelationKind::Overrides
                ),
                "data_flow" => r.kind == RelationKind::PassesValue,
                _ => {
                    r.kind == RelationKind::Calls
                        || (q.include_candidates
                            && matches!(r.kind, RelationKind::MayCall | RelationKind::Dispatches))
                }
            }
        } else {
            match q.operation.as_str() {
                "callees" | "callers" => {
                    r.kind == RelationKind::Calls
                        || (q.include_candidates
                            && matches!(r.kind, RelationKind::MayCall | RelationKind::Dispatches))
                }
                "implementations" => {
                    matches!(r.kind, RelationKind::Implements | RelationKind::Overrides)
                }
                _ => true,
            }
        })
    }
    fn entity_groups(&self, q: &QueryRequest) -> Result<Vec<String>> {
        if self.reader.metadata()["search_index"]["version"] == 1 {
            if q.operation == "semantic" && !q.regex {
                let index = search::SearchIndex::with_model(&[], self.model.clone());
                if self.reader.metadata()["search_index"]["vector_model"] == index.model_name() {
                    return Ok(if q.exact_semantic {
                        vec!["all:entity_vector".into()]
                    } else {
                        search::vector_buckets(&index.prepare_vector(&search::tokenize(&q.text)))
                            .into_iter()
                            .map(|b| group("vector_bucket", &b))
                            .collect()
                    });
                }
                return Err(Error::new(
                    "VectorIndexModelMismatch",
                    "query model differs from the frozen vector index; configure its original model",
                ));
            }
            if q.operation == "search" && !q.text.is_empty() {
                if q.regex {
                    if let Some(name) = search::literal_regex(&q.text) {
                        return Ok(vec![group("lookup_folded", &name.to_lowercase())]);
                    }
                } else {
                    if search::grams(&q.text).is_empty() {
                        return Ok(vec![
                            if q.include_external {
                                "all:entity"
                            } else {
                                "internal:entity"
                            }
                            .into(),
                        ]);
                    }
                    let mut groups = search::tokenize(&q.text)
                        .into_iter()
                        .map(|t| group("search_term", &t))
                        .collect::<Vec<_>>();
                    if let Some(gram) = search::grams(&q.text).into_iter().next() {
                        groups.push(group("search_gram", &gram));
                    }
                    groups.sort();
                    groups.dedup();
                    if !groups.is_empty() {
                        return Ok(groups);
                    }
                }
            }
        }
        if q.file.is_some() || q.directory.is_some() {
            let (path, directory) = if let Some(path) = &q.file {
                (path, false)
            } else {
                (q.directory.as_ref().unwrap(), true)
            };
            let files = self.reader.named_files(path, directory)?;
            if !directory && files.is_empty() && !self.reader.named_files(path, true)?.is_empty() {
                return Err(Error::new(
                    "DirectoryPassedAsFile",
                    "file requires an exact file path; use directory for a subtree",
                ));
            }
            return Ok(files.iter().map(|f| group("file_entity", &f.id)).collect());
        }
        if let Some(c) = self.context(q)? {
            return Ok(c.units.iter().map(|u| group("unit", u)).collect());
        }
        if let Some(kind) = &q.kind {
            return Ok(if kind.eq_ignore_ascii_case("Callable") {
                vec![
                    group("kind_entity", "function"),
                    group("kind_entity", "method"),
                ]
            } else {
                vec![group("kind_entity", &kind.to_ascii_lowercase())]
            });
        }
        Ok(vec![
            if q.include_external {
                "all:entity"
            } else {
                "internal:entity"
            }
            .into(),
        ])
    }
    fn file_named(&self, path: &str) -> Result<FileRevision> {
        let mut files = self.reader.named_files(path, false)?;
        if files.len() > 1 {
            return Err(Error::new("AmbiguousFile", path));
        }
        files.pop().ok_or_else(|| Error::new("MissingFile", path))
    }

    fn byte_at(&self, file: &FileRevision, q: &QueryRequest) -> Result<u64> {
        let target = q.line.unwrap_or(1);
        let column = q.column.unwrap_or(1) as u64 - 1;
        let mut line = 1;
        let mut offset = 0;
        let mut start = None;
        let mut blocks = 0;
        let began = Instant::now();
        while offset < file.blob.len {
            if blocks >= q.max_work || began.elapsed() > Duration::from_millis(q.timeout_ms) {
                return Err(Error::new(
                    "QueryWorkBudget",
                    "source position prefix exceeds budget; increase max_work/time",
                ));
            }
            if self.control.is_some_and(|c| c.is_cancelled()) {
                return Err(Error::new("Cancelled", "source position cancelled"));
            }
            let end = (offset + 64 * 1024).min(file.blob.len);
            let bytes = self.reader.source_range(&file.id, offset, end)?;
            blocks += 1;
            for byte in bytes {
                if line == target {
                    let base = *start.get_or_insert(offset);
                    if byte == b'\n' {
                        return if column <= offset - base {
                            Ok(base + column)
                        } else {
                            Err(Error::new("SourceRange", "column outside selected line"))
                        };
                    }
                }
                offset += 1;
                if byte == b'\n' {
                    line += 1;
                }
            }
        }
        if let Some(base) = start {
            if column <= offset - base {
                return Ok(base + column);
            }
        } else if line == target && column == 0 {
            return Ok(offset);
        }
        Err(Error::new("SourceRange", "line/column outside frozen file"))
    }
    fn code_step(&self, state: &mut State, q: &QueryRequest) -> Result<Option<Value>> {
        let scan = state
            .code
            .as_mut()
            .ok_or_else(|| Error::new("InvalidCursor", "source scan state"))?;
        if let Some(n) = scan
            .carry
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .or_else(|| {
                (scan.offset == scan.file.blob.len && !scan.carry.is_empty())
                    .then_some(scan.carry.len())
            })
        {
            let bytes = scan.carry.drain(..n).collect::<Vec<_>>();
            let start = scan.start;
            let line = scan.line;
            scan.start += n as u64;
            scan.line += 1;
            let text = match String::from_utf8(bytes) {
                Ok(t) => t,
                Err(_) => {
                    let file = scan.file.logical_path.display();
                    state.extra_gaps.push(Gap::new(
                        "NonUtf8Source",
                        file,
                        "text search skipped non-UTF8 bytes; frozen raw evidence remains available",
                    ));
                    state.code = None;
                    return Ok(None);
                }
            };
            let matched = if q.regex {
                regex::Regex::new(&q.text)
                    .map_err(|e| Error::new("InvalidPattern", e.to_string()))?
                    .is_match(&text)
            } else {
                text.contains(&q.text)
            };
            return Ok(matched.then(|| {
                let file_name = self.display_path(&scan.file, q.compact);
                if q.compact {
                    json!({"file":file_name,"line":line,"text":text})
                } else {
                    json!({"file":file_name,"line":line,"text":text,"next_line":(scan.start<scan.file.blob.len).then_some(line+1),"source":SourceAnchor{file_revision:scan.file.id.clone(),start,end:start+n as u64}})
                }
            }));
        }
        if scan.offset < scan.file.blob.len {
            let end = (scan.offset + 64 * 1024).min(scan.file.blob.len);
            scan.carry
                .extend(self.reader.source_range(&scan.file.id, scan.offset, end)?);
            scan.offset = end;
            if scan.carry.len() > 8 * 1024 * 1024 {
                return Err(Error::new(
                    "SourceLineBudget",
                    "one source line exceeds 8 MiB",
                ));
            }
        } else {
            state.code = None;
        }
        Ok(None)
    }
    fn initial(&self, q: &QueryRequest) -> Result<State> {
        if q.operation == "outline" && q.file.is_none() {
            return Err(Error::new("MissingFile", "outline needs file"));
        }
        let mut s = State {
            skip: q.offset,
            ..Default::default()
        };
        s.groups = match q.operation.as_str() {
            "search_code" => vec!["all:file".into()],
            "at" => {
                let file = self.file_named(
                    q.file
                        .as_deref()
                        .ok_or_else(|| Error::new("MissingFile", "at needs file/line/column"))?,
                )?;
                let position = self.byte_at(&file, q)?;
                if position > file.blob.len {
                    return Err(Error::new("SourceRange", "column outside frozen file"));
                }
                s.at = Some((file.id.clone(), position));
                vec![
                    group("file_entity", &file.id),
                    group("file_occurrence", &file.id),
                ]
            }
            "coverage" | "capabilities" => vec!["all:coverage".into()],
            "gaps" => vec!["all:gaps".into()],
            "contexts" => vec!["all:context".into()],
            "scopes" => vec!["all:scope".into()],
            "observations" => vec!["all:observation_unit".into()],
            "provenance_observations" => vec!["all:observation_provenance".into()],
            "runtime_traces" => vec!["all:runtime_trace".into()],
            "search" | "semantic" | "entrypoints" | "outline" => self.entity_groups(q)?,
            "docs" | "doc_search" | "document" => {
                if let Some(id) = &q.subject {
                    if self.value("document", id)?.is_some() {
                        s.pending.push_back(self.value("document", id)?.unwrap());
                        vec![]
                    } else {
                        let e = self.resolve(id, q)?;
                        vec![group("document", &e.id)]
                    }
                } else {
                    if q.operation == "doc_search"
                        && self.reader.metadata()["search_index"]["version"] == 1
                    {
                        let mut chosen = None;
                        for gram in search::grams(&q.text) {
                            let g = group("document_gram", &gram);
                            let n = self.reader.count(&g)?;
                            if chosen.as_ref().is_none_or(|(_, best)| n < *best) {
                                chosen = Some((g, n));
                            }
                            if n == 0 {
                                break;
                            }
                        }
                        chosen
                            .map(|(g, _)| vec![g])
                            .unwrap_or_else(|| vec!["all:document".into()])
                    } else {
                        vec!["all:document".into()]
                    }
                }
            }
            "bodies" => vec![group("body", &self.subject(q)?.id)],
            "callers" | "implementations" => {
                let e = self.subject(q)?;
                s.subject_id = Some(e.id.clone());
                vec![group("in", &e.id)]
            }
            "callees" | "relations" => {
                let e = self.subject(q)?;
                s.subject_id = Some(e.id.clone());
                vec![group("out", &e.id)]
            }
            "references" | "occurrences" => {
                let e = self.subject(q)?;
                s.subject_id = Some(e.id.clone());
                let mut g = vec![group("reference", &e.id), group("occurrence_owner", &e.id)];
                let bodies = self.reader.scan(&group("body", &e.id), None, 1000)?;
                if !bodies.complete {
                    return Err(Error::new(
                        "QueryWorkBudget",
                        "too many bodies; select a specific body",
                    ));
                }
                for b in bodies.rows {
                    if let Some(id) = b.value["id"].as_str() {
                        g.push(group("occurrence_owner", id));
                    }
                }
                g
            }
            "members" => vec![
                group("member", &self.subject(q)?.id),
                group("membership", &self.subject(q)?.id),
            ],
            "topology" => vec!["all:unit".into()],
            "trace" | "impact" | "path" => {
                let e = self.subject(q)?;
                s.scheduled.insert(e.id.clone());
                s.walk.push_back(Step {
                    id: e.id,
                    hop: 0,
                    path: vec![],
                    via: None,
                    reported: false,
                    after: None,
                    direction: 0,
                });
                s.target = q
                    .target
                    .as_ref()
                    .map(|t| self.resolve(t, q).map(|e| e.id))
                    .transpose()?;
                vec![]
            }
            "overview"
            | "schema"
            | "details"
            | "definition"
            | "type_definition"
            | "build_configuration"
            | "source" => vec![],
            other => {
                return Err(Error::new(
                    "ExplicitGlobalJobRequired",
                    format!("{other} uses the bounded compatibility/export job"),
                ));
            }
        };
        Ok(s)
    }
    fn score(&self, e: &Entity, q: &QueryRequest) -> Result<f64> {
        if q.operation == "entrypoints" {
            return Ok(if e.name == "main" || e.attributes.contains_key("test") {
                10.
            } else if e.exported && matches!(e.kind, EntityKind::Function | EntityKind::Method) {
                1.
            } else {
                0.
            });
        }
        if q.regex {
            let r = regex::Regex::new(&q.text)
                .map_err(|e| Error::new("InvalidPattern", e.to_string()))?;
            return Ok(if r.is_match(&e.name) || r.is_match(&e.qualified_name) {
                1.
            } else {
                0.
            });
        }
        if q.text.is_empty() {
            return Ok(1.);
        }
        let terms = search::tokenize(&q.text);
        let doc = search::terms(e);
        let corpus = self
            .value("search_stats", "corpus")?
            .ok_or_else(|| Error::new("IndexIncomplete", "search corpus"))?;
        let n = corpus["documents"].as_f64().unwrap_or(0.);
        let avg = (corpus["terms"].as_f64().unwrap_or(0.) / n.max(1.)).max(1.);
        let mut bm = 0.;
        for t in &terms {
            let freq = doc.iter().filter(|x| *x == t).count() as f64;
            if freq == 0. {
                continue;
            }
            let df = self
                .value("term_df", t)?
                .and_then(|v| v.as_f64())
                .unwrap_or(0.);
            bm += ((n - df + 0.5) / (df + 0.5) + 1.).ln() * freq * 2.2
                / (freq + 1.2 * (0.25 + 0.75 * doc.len() as f64 / avg));
        }
        if q.operation == "semantic" {
            let one = search::SearchIndex::with_model(std::slice::from_ref(e), self.model.clone());
            return Ok(one.semantic(0, &q.text) * 2. + bm * 0.1);
        }
        if q.operation == "semantic_lexical" {
            return Ok(bm);
        }
        Ok(bm
            + if e.name.eq_ignore_ascii_case(&q.text) {
                20.
            } else if e
                .qualified_name
                .to_lowercase()
                .contains(&q.text.to_lowercase())
            {
                2.
            } else {
                0.
            })
    }
    fn transform(
        &self,
        v: Value,
        q: &QueryRequest,
        ctx: Option<&AnalysisContext>,
        subject_id: Option<&str>,
    ) -> Result<Option<Value>> {
        match q.operation.as_str() {
            "search" | "semantic" | "entrypoints" | "outline" | "members" => {
                let v = if v.get("from").is_some() {
                    let id = v["to"]
                        .as_str()
                        .ok_or_else(|| Error::new("InvalidRelation", "membership"))?;
                    self.value("entity", id)?
                        .ok_or_else(|| Error::new("DanglingEndpoint", id))?
                } else {
                    v
                };
                let e: Entity = serde_json::from_value(v)?;
                if !self.selected(&e, q, ctx)? || !self.allowed(&e.provenance, q)? {
                    return Ok(None);
                }
                if ["search", "semantic", "entrypoints"].contains(&q.operation.as_str()) {
                    if !disclosure::visible(&e, q) {
                        return Ok(None);
                    }
                    let score = self.score(&e, q)?;
                    return Ok((score > 0.).then(|| json!({"id":e.id,"score":score})));
                }
                if q.operation == "outline"
                    && (matches!(
                        e.kind,
                        EntityKind::Body
                            | EntityKind::File
                            | EntityKind::Variable
                            | EntityKind::Parameter
                    ) || (q.kind.is_none() && e.kind == EntityKind::DocSection))
                {
                    return Ok(None);
                }
                let mut value = self.summary_for(&e, q.compact)?;
                if q.operation == "members" {
                    let owner = self.subject(q)?;
                    let b = self.reader.scan(
                        &group("membership_pair", &format!("{}\0{}", owner.id, e.id)),
                        None,
                        1000,
                    )?;
                    if q.compact {
                        let kinds: Vec<_> = b
                            .rows
                            .into_iter()
                            .filter_map(|r| r.value.get("kind").cloned())
                            .collect();
                        if !kinds.is_empty() {
                            value["membership"] = json!(kinds);
                        }
                    } else {
                        value["membership"] =
                            json!(b.rows.into_iter().map(|r| r.value).collect::<Vec<_>>());
                        value["membership_complete"] = json!(b.complete);
                    }
                }

                Ok(Some(value))
            }
            "callers" | "callees" | "implementations" | "relations" => {
                let r: Relation = serde_json::from_value(v)?;
                if !self.edge_allowed(&r, q, false)? {
                    return Ok(None);
                }
                let id = if ["callers", "implementations"].contains(&q.operation.as_str()) {
                    &r.from
                } else {
                    &r.to
                };
                let e = self
                    .entity(id)?
                    .ok_or_else(|| Error::new("DanglingEndpoint", id))?;
                if !self.selected(&e, q, ctx)? {
                    return Ok(None);
                }
                let subj = subject_id.unwrap_or("");
                Ok(Some(self.format_relation_row(subj, &e, &r, q.compact)?))
            }
            "references" | "occurrences" => {
                let o: Occurrence = serde_json::from_value(v)?;
                if let Some(path) = &q.file {
                    let f = self
                        .file(&o.source.file_revision)?
                        .ok_or_else(|| Error::new("MissingEvidence", &o.source.file_revision))?;
                    if f.logical_path.display() != *path
                        && !f.logical_path.display().ends_with(&format!("/{path}"))
                    {
                        return Ok(None);
                    }
                }

                if !self.allowed(&o.provenance, q)?
                    || ctx.is_some_and(|c| !c.units.contains(&o.unit))
                    || q.relation
                        .as_ref()
                        .is_some_and(|k| RelationKind::parse(k) != Some(o.kind))
                {
                    return Ok(None);
                }
                let subj = subject_id.unwrap_or("");
                Ok(Some(self.format_occurrence(&o, subj, q.compact)?))
            }
            "docs" | "doc_search" | "document" => {
                let d: DocumentBlock = serde_json::from_value(v)?;
                if !self.allowed(&d.provenance, q)?
                    || q.document_kind
                        .as_ref()
                        .is_some_and(|k| !format!("{:?}", d.kind).eq_ignore_ascii_case(k))
                {
                    return Ok(None);
                }
                if let Some(owner) = &d.owner
                    && let Some(e) = self.entity(owner)?
                    && !self.selected(&e, q, ctx)?
                {
                    return Ok(None);
                }
                if q.operation == "doc_search"
                    && !d.text.to_lowercase().contains(&q.text.to_lowercase())
                    && !d.title.to_lowercase().contains(&q.text.to_lowercase())
                {
                    return Ok(None);
                }
                Ok(Some(if q.operation == "document" {
                    let lines: Vec<_> = d.text.lines().collect();
                    let start = q.line.unwrap_or(1).saturating_sub(1).min(lines.len());
                    let end = (start + q.max_lines).min(lines.len());
                    if q.compact {
                        let loc = self.anchor_location(&d.source, true)?;
                        let mut row = json!({"id":d.id,"kind":d.kind,"title":d.title,"text":lines[start..end].join("\n"),"start_line":start+1,"next_line":(end<lines.len()).then_some(end+1),"total_lines":lines.len()});
                        if let Some((path, pos)) = loc {
                            row["file"] = json!(path);
                            if let Some((l, _, _)) = pos {
                                row["line"] = json!(l);
                            }
                        }
                        if let Some(owner) = &d.owner {
                            row["owner"] = json!(self.entity_label(owner)?);
                            row["owner_id"] = json!(owner);
                        }
                        row
                    } else {
                        json!({"id":d.id,"kind":d.kind,"title":d.title,"text":lines[start..end].join("\n"),"start_line":start+1,"next_line":(end<lines.len()).then_some(end+1),"total_lines":lines.len(),"owner":d.owner,"source":d.source,"links":d.links,"provenance":d.provenance})
                    }
                } else if q.compact {
                    let loc = self.anchor_location(&d.source, true)?;
                    let mut row = json!({"id":d.id,"kind":d.kind,"title":d.title,"excerpt":d.text.chars().take(200).collect::<String>()});
                    if let Some((path, pos)) = loc {
                        row["file"] = json!(path);
                        if let Some((l, _, _)) = pos {
                            row["line"] = json!(l);
                        }
                    }
                    if let Some(owner) = &d.owner {
                        row["owner"] = json!(self.entity_label(owner)?);
                        row["owner_id"] = json!(owner);
                    }
                    row
                } else {
                    json!({"id":d.id,"kind":d.kind,"title":d.title,"excerpt":d.text.chars().take(200).collect::<String>(),"owner":d.owner,"source":d.source,"links":d.links,"provenance":d.provenance})
                }))
            }
            "contexts" => {
                let c: AnalysisContext = serde_json::from_value(v)?;
                if q.context
                    .as_ref()
                    .is_some_and(|id| c.id != *id && c.entry_unit != *id)
                {
                    return Ok(None);
                }
                if !q.compact {
                    return Ok(Some(json!(c)));
                }
                let mut row = if let Some(u) = self.value("unit", &c.entry_unit)? {
                    super::presentation::unit_summary(&serde_json::from_value(u)?)
                } else {
                    json!({"entry_unit":c.entry_unit})
                };
                row["id"] = json!(c.id);
                row["entry_unit"] = json!(c.entry_unit);
                row["status"] = json!(c.status);
                row["dependency_units"] = json!(c.units.len());
                row["continue_with"] =
                    json!({"operation":"contexts","context":c.id,"compact":false});
                Ok(Some(row))
            }
            _ => Ok(Some(v)),
        }
    }
    fn singleton(&self, q: &QueryRequest) -> Result<Vec<Value>> {
        let context = if let Some(c) = self.context(q)? {
            self.value("context_stats", &c.id)?
        } else {
            None
        };
        let m = context.as_ref().unwrap_or(self.reader.metadata());
        Ok(match q.operation.as_str() {
            "overview" => vec![
                json!({"project":self.reader.project(),"revision":self.reader.revision(),"entities":m["counts"]["entities"],"relations":m["counts"]["relations"],"occurrences":m["counts"]["occurrences"],"documents":m["counts"]["documents"],"kinds":m["entity_kinds"],"context_count":m["counts"]["contexts"],"completion":self.reader.metadata()["completion"],"storage_rejections":self.reader.metadata()["storage_rejections"],"explanation":"Compiler facts in separate contexts; author intent requires separately cited evidence."}),
            ],
            "schema" => vec![
                json!({"nodes":m["entity_kinds"],"relationships":m["relation_kinds"],"separate_occurrences":true}),
            ],
            "definition" => {
                let e = self.subject(q)?;
                if let Some(ids) = e.attributes.get("definition_candidates") {
                    let ids: Vec<String> = serde_json::from_str(ids)?;
                    let mut v = vec![];
                    for id in ids {
                        if let Some(t) = self.entity(&id)? {
                            v.push(json!({"entity":self.summary_for(&t, q.compact)?,"authority":"RuleDerived","meaning":"native USR candidate; linker selection not observed"}));
                        }
                    }
                    v
                } else {
                    vec![self.summary_for(&e, q.compact)?]
                }
            }
            "type_definition" => {
                let e = self.subject(q)?;
                let mut v = vec![];
                if e.type_navigation.is_empty() {
                    for id in &e.type_definitions {
                        if let Some(t) = self.entity(id)? {
                            v.push(self.summary_for(&t, q.compact)?);
                        }
                    }
                } else {
                    for nav in &e.type_navigation {
                        let mut candidates = vec![];
                        for id in &nav.candidates {
                            if let Some(t) = self.entity(id)? {
                                candidates.push(self.summary_for(&t, q.compact)?);
                            }
                        }
                        v.push(json!({"source":nav.source,"extent":nav.extent,"candidates":candidates,"meaning":"native navigation candidates; not runtime dispatch"}));
                    }
                }
                v
            }
            "details" => {
                let e = self.subject(q)?;
                let direct_members = self.reader.count(&group("member", &e.id))?;
                let member_edges = self.reader.count(&group("membership", &e.id))?;
                let counts = json!({"members":if direct_members==0&&member_edges==0{Some(0)}else{None},"declaring_members":direct_members,"membership_edges":member_edges,"documents":self.reader.count(&group("document",&e.id))?,"bodies":self.reader.count(&group("body",&e.id))?,"references":self.reader.count(&group("reference",&e.id))?});
                vec![
                    json!({"entity":self.summary_for(&e, q.compact)?,"counts":counts,"hidden":[{"category":"members","count":counts["members"],"continue_with":{"operation":"members","subject":e.id}},{"category":"documents","count":counts["documents"],"continue_with":{"operation":"docs","subject":e.id}},{"category":"references","count":counts["references"],"continue_with":{"operation":"references","subject":e.id}},{"category":"bodies","count":counts["bodies"],"continue_with":{"operation":"bodies","subject":e.id}}]}),
                ]
            }
            "source" => self.source(q)?,
            "build_configuration" => {
                let id = q
                    .subject
                    .as_deref()
                    .ok_or_else(|| Error::new("MissingSubject", "select unit"))?;
                let unit = if let Some(v) = self.value("unit", id)? {
                    v
                } else {
                    let e = self.resolve(id, q)?;
                    self.value("unit", &e.unit)?
                        .ok_or_else(|| Error::new("MissingUnit", id))?
                };
                if q.compact && q.configuration_view == "summary" {
                    vec![super::presentation::unit_summary(&serde_json::from_value(
                        unit,
                    )?)]
                } else {
                    vec![json!({"unit":unit})]
                }
            }
            _ => vec![],
        })
    }
    fn source(&self, q: &QueryRequest) -> Result<Vec<Value>> {
        let mut anchor_start = None;
        let file = if let Some(id) = &q.file_revision {
            self.file(id)?
                .ok_or_else(|| Error::new("MissingFile", "unknown frozen file revision"))?
        } else if q.subject.is_some() {
            let e = self.subject(q)?;
            let a = e
                .extent
                .as_ref()
                .or(e.source.as_ref())
                .ok_or_else(|| Error::new("SourceUnavailable", "no frozen source"))?;
            anchor_start = Some(a.start);
            self.file(&a.file_revision)?
                .ok_or_else(|| Error::new("MissingEvidence", &a.file_revision))?
        } else {
            let p = q
                .file
                .as_deref()
                .ok_or_else(|| Error::new("MissingFile", "select source file"))?;
            let mut after = None;
            let mut found = None;
            loop {
                let b = self.reader.scan("all:file", after.as_deref(), 64)?;
                for row in b.rows {
                    let f: FileRevision = serde_json::from_value(row.value)?;
                    if f.logical_path.display() == p
                        || f.logical_path.display().ends_with(&format!("/{p}"))
                    {
                        if found.is_some() {
                            return Err(Error::new("AmbiguousFile", p));
                        }
                        found = Some(f);
                    }
                }
                if b.complete {
                    break;
                }
                after = b.after;
            }
            found.ok_or_else(|| Error::new("MissingFile", p))?
        };
        let target = if let Some(line) = q.line {
            line
        } else if let Some(anchor) = anchor_start {
            let mut offset = 0;
            let mut line = 1;
            while offset < anchor {
                let end = (offset + 64 * 1024).min(anchor);
                let bytes = self.reader.source_range(&file.id, offset, end)?;
                line += bytes.iter().filter(|b| **b == b'\n').count();
                offset = end;
            }
            line
        } else {
            1
        };
        let mut offset = 0_u64;
        let mut line = 1;
        let mut carry = Vec::new();
        let mut rows = vec![];
        // Only blocks needed to reach and display the selected line are read.
        while offset < file.blob.len && line < target + q.max_lines {
            let end = (offset + 64 * 1024).min(file.blob.len);
            let bytes = self.reader.source_range(&file.id, offset, end)?;
            carry.extend_from_slice(&bytes);
            offset = end;
            loop {
                let n = carry
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|n| n + 1)
                    .or_else(|| {
                        (offset == file.blob.len && !carry.is_empty()).then_some(carry.len())
                    });
                let Some(n) = n else {
                    break;
                };
                let start = offset - carry.len() as u64;
                let text = String::from_utf8_lossy(&carry[..n]).into_owned();
                if line >= target && line < target + q.max_lines {
                    let display_file = self.display_path(&file, q.compact);
                    if q.compact {
                        let mut row = json!({"file":display_file,"line":line,"text":text});
                        if line + 1 == target + q.max_lines && start + (n as u64) < file.blob.len {
                            row["next_line"] = json!(line + 1);
                        }
                        rows.push(row);
                    } else {
                        rows.push(json!({"file":display_file,"line":line,"text":text,"next_line":(start+(n as u64)<file.blob.len).then_some(line+1),"source":SourceAnchor{file_revision:file.id.clone(),start,end:start+n as u64}}));
                    }
                }
                carry.drain(..n);
                line += 1;
                if line >= target + q.max_lines {
                    break;
                }
            }
            if carry.len() > 8 * 1024 * 1024 {
                return Err(Error::new("SourceLineBudget", "one line exceeds 8 MiB"));
            }
        }
        Ok(rows)
    }
    fn walk_one(
        &self,
        s: &mut State,
        q: &QueryRequest,
        ctx: Option<&AnalysisContext>,
    ) -> Result<Option<Value>> {
        let Some(mut step) = s.walk.pop_front() else {
            return Ok(None);
        };
        let inbound = q.direction == "inbound" || q.operation == "impact";
        let both = q.direction == "both";
        let calls_mode = q.relation.is_none() && q.mode == "calls";
        if !step.reported {
            s.visited.insert(step.id.clone());
            step.reported = true;
            let e = self
                .entity(&step.id)?
                .ok_or_else(|| Error::new("DanglingEndpoint", &step.id))?;
            if q.operation == "path" && s.target.as_ref() == Some(&step.id) {
                s.walk.clear();
                s.exhausted = true;
                if q.compact {
                    let mut hops = Vec::with_capacity(step.path.len());
                    for id in &step.path {
                        if let Some(v) = self.value("relation", id)? {
                            let r: Relation = serde_json::from_value(v)?;
                            hops.push(self.format_via(&r, true)?);
                        }
                    }
                    return Ok(Some(json!({
                        "target": self.summary_for(&e, true)?,
                        "hops": hops,
                        "meaning": "potential static path, not runtime proof"
                    })));
                }
                return Ok(Some(
                    json!({"target":self.summary_for(&e, false)?,"relation_path":step.path,"meaning":"potential static path, not runtime proof"}),
                ));
            }
            let row = if q.operation != "path" {
                let via = if let Some(id) = step.via.as_deref() {
                    self.value("relation", id)?
                        .map(|v| -> Result<Value> {
                            if q.compact {
                                let r: Relation = serde_json::from_value(v)?;
                                self.format_via(&r, true)
                            } else {
                                Ok(v)
                            }
                        })
                        .transpose()?
                } else {
                    None
                };
                if q.compact {
                    let mut map = serde_json::Map::new();
                    map.insert("entity".into(), self.summary_for(&e, true)?);
                    map.insert("hop".into(), json!(step.hop));
                    if let Some(v) = via {
                        map.insert("via".into(), v);
                    }
                    Some(Value::Object(map))
                } else {
                    Some(
                        json!({"entity":self.summary_for(&e, false)?,"hop":step.hop,"via":via,"path":step.path}),
                    )
                }
            } else {
                None
            };
            let ascend_body_only = q.depth > 0
                && step.hop >= q.depth
                && calls_mode
                && Self::is_nested_body(&e)
                && (inbound || both);
            if step.hop < q.depth || ascend_body_only {
                s.walk.push_front(step);
            }
            return Ok(row);
        }
        let cur_entity = self.entity(&step.id)?;
        let node_is_nested_body =
            calls_mode && cur_entity.as_ref().is_some_and(Self::is_nested_body);
        let ascend_body_only =
            q.depth > 0 && step.hop >= q.depth && node_is_nested_body && (inbound || both);
        let back = if ascend_body_only {
            true
        } else if both {
            step.direction == 0
        } else {
            inbound
        };
        let p = self.reader.scan(
            &group(if back { "in" } else { "out" }, &step.id),
            step.after.as_deref(),
            1,
        )?;
        for row in p.rows {
            let r: Relation = serde_json::from_value(row.value)?;
            let allowed = if ascend_body_only {
                self.allowed(&r.provenance, q)? && back && r.kind == RelationKind::CreatesBody
            } else if self.edge_allowed(&r, q, true)? {
                true
            } else if calls_mode
                && r.kind == RelationKind::CreatesBody
                && self.allowed(&r.provenance, q)?
            {
                if back {
                    node_is_nested_body
                } else {
                    self.entity(&r.to)?
                        .as_ref()
                        .is_some_and(Self::is_nested_body)
                        && self.body_has_matching_call(&r.to, q, ctx)?
                }
            } else {
                false
            };
            if !allowed {
                continue;
            }
            let id = if back { &r.from } else { &r.to };
            if s.scheduled.contains(id) {
                continue;
            }
            let e = self
                .entity(id)?
                .ok_or_else(|| Error::new("DanglingEndpoint", id))?;
            if !self.selected(&e, q, ctx)? {
                continue;
            }
            s.scheduled.insert(id.clone());
            let mut path = step.path.clone();
            path.push(r.id.clone());
            let next_hop = if r.kind == RelationKind::CreatesBody {
                step.hop
            } else {
                step.hop + 1
            };
            s.walk.push_back(Step {
                id: id.clone(),
                hop: next_hop,
                path,
                via: Some(r.id),
                reported: false,
                after: None,
                direction: 0,
            });
        }
        if !p.complete {
            step.after = p.after;
            s.walk.push_front(step);
        } else if both && !ascend_body_only && step.direction == 0 {
            step.direction = 1;
            step.after = None;
            s.walk.push_front(step);
        }
        Ok(None)
    }
    pub fn execute(
        &self,
        q: &QueryRequest,
        resume: Option<QueryContinuation>,
    ) -> Result<QueryResult> {
        let request = self.reader.request_view()?;
        let inner = request.as_deref().unwrap_or(self.reader);
        let cache = self.reader.query_cache().unwrap_or_else(|| {
            Arc::new(crate::ports::facts::ReadCache::new(
                inner.project(),
                inner.revision(),
            ))
        });
        let cached = crate::ports::facts::CachedReader::new(inner, cache)?;
        let mut result = Engine {
            reader: &cached,
            model: self.model.clone(),
            control: self.control,
        }
        .execute_inner(q, resume)?;
        if disclosure::is_overview(q) {
            let engine = Engine {
                reader: &cached,
                model: self.model.clone(),
                control: self.control,
            };
            for item in &mut result.page.items {
                if let Some(id) = item["id"].as_str()
                    && let Some(e) = engine.entity(id)?
                {
                    item["_search_unit"] = json!(e.unit);
                    item["_search_module"] =
                        disclosure::module(&e, |id| engine.entity(id))?.unwrap_or(Value::Null);
                }
            }
        }
        {
            let deferred = disclosure::apply(&mut result.page, q)?;
            if !deferred.is_empty() {
                let mut state = if let Some(c) = result.continuation.take() {
                    match c.state {
                        ContinuationState::Store { state, .. } => {
                            serde_json::from_value::<State>(state)?
                        }
                        _ => {
                            return Err(Error::new(
                                "InvalidCursor",
                                "grouped search needs a store cursor",
                            ));
                        }
                    }
                } else {
                    State {
                        exhausted: true,
                        ranking_done: true,
                        text: (q.operation == "search" && !q.regex).then(text::TextPlan::finished),
                        ..Default::default()
                    }
                };
                for item in deferred.into_iter().rev() {
                    state.pending.push_front(item);
                }
                result.continuation = Some(QueryContinuation {
                    request: q.clone(),
                    revision: result.page.revision.clone(),
                    query_digest: request_digest(q),
                    state: ContinuationState::Store {
                        project: self.reader.project().into(),
                        state: serde_json::to_value(state)?,
                    },
                });
            }
        }
        Ok(result)
    }
    fn execute_inner(
        &self,
        q: &QueryRequest,
        resume: Option<QueryContinuation>,
    ) -> Result<QueryResult> {
        if q.limit == 0
            || q.limit > 1000
            || q.max_work == 0
            || q.max_bytes < 128
            || q.timeout_ms == 0
            || q.depth > 4096
            || q.max_lines == 0
            || q.max_lines > 1000
            || q.line == Some(0)
            || q.column == Some(0)
        {
            return Err(Error::new("InvalidBudget", "invalid query budget"));
        }
        if !["calls", "types", "data_flow"].contains(&q.mode.as_str()) {
            return Err(Error::new("UnsupportedMode", &q.mode));
        }
        if q.operation == "search_code" && q.text.is_empty() {
            return Err(Error::new(
                "MissingText",
                "source search needs nonempty text",
            ));
        }
        if q.regex {
            regex::Regex::new(&q.text).map_err(|e| Error::new("InvalidPattern", e.to_string()))?;
        }
        if q.operation == "semantic"
            && (!embeddings::enabled()
                || self.reader.metadata()["search_index"]["vector_enabled"] == false)
        {
            return Err(Error::new(
                "VectorSearchDisabled",
                "vector retrieval is experimental and disabled by default",
            ));
        }
        super::presentation::validate_scope(q)?;
        let start = Instant::now();
        self.reader.set_timeout(q.timeout_ms);
        if let Some(path) = &q.file
            && ["search", "semantic", "outline"].contains(&q.operation.as_str())
            && self.reader.named_files(path, false)?.is_empty()
            && !self.reader.named_files(path, true)?.is_empty()
        {
            return Err(Error::new(
                "DirectoryPassedAsFile",
                "file requires an exact file path; use directory for a subtree",
            ));
        }
        let ctx = self.context(q)?;
        let digest = request_digest(q);
        if q.operation == "search" && !q.regex {
            return self.execute_text(q, resume, ctx.as_ref(), start, digest);
        }
        let is_walk = ["trace", "impact", "path"].contains(&q.operation.as_str());
        let ranked = ["search", "semantic", "entrypoints"].contains(&q.operation.as_str());
        let mut state = if let Some(c) = resume {
            if c.revision != self.reader.revision() || c.query_digest != digest {
                return Err(Error::new("StaleCursor", "revision or query differs"));
            }
            if let ContinuationState::Store { project, state } = c.state {
                if project != self.reader.project() {
                    return Err(Error::new("WrongProject", "cursor project"));
                }
                serde_json::from_value(state)?
            } else {
                return Err(Error::new("InvalidCursor", "store cursor required"));
            }
        } else {
            let mut s = self.initial(q)?;
            if s.groups.is_empty() && !is_walk {
                let rows = if ["document", "docs", "doc_search"].contains(&q.operation.as_str()) {
                    std::mem::take(&mut s.pending)
                        .into_iter()
                        .map(|v| self.transform(v, q, ctx.as_ref(), s.subject_id.as_deref()))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect()
                } else {
                    self.singleton(q)?
                };
                s.pending = rows.into();
                s.exhausted = true;
                s.ranking_done = true;
            }
            s
        };
        let mut items = vec![];
        let mut work = 0;
        let mut bytes = 0;
        while work < q.max_work
            && start.elapsed() < Duration::from_millis(q.timeout_ms)
            && items.len() < q.limit
        {
            if self.control.is_some_and(|c| c.is_cancelled()) {
                return Err(Error::new(
                    "Cancelled",
                    "query cancelled; no partial cursor published",
                ));
            }
            if let Some(mut v) = state.pending.pop_front() {
                if ranked && state.ranking_done && v.get("kind").is_none() {
                    let id = v["id"]
                        .as_str()
                        .ok_or_else(|| Error::new("InvalidCursor", "rank identity"))?
                        .to_owned();
                    let score = v["score"].as_f64().unwrap_or(0.);
                    let e = self
                        .entity(&id)?
                        .ok_or_else(|| Error::new("MissingEntity", &id))?;
                    v = self.summary_for(&e, q.compact)?;
                    v["score"] = json!(score);
                    if !q.compact {
                        v["entity_ref"] = json!(id);
                    }
                }
                let size = serde_json::to_vec(&v)?.len();
                if size > q.max_bytes && items.is_empty() {
                    state.pending.push_front(v);
                    return Err(Error::new(
                        "QueryByteBudget",
                        "one result exceeds max_bytes; select a smaller source/document range or increase budget",
                    ));
                }
                if bytes + size > q.max_bytes {
                    state.pending.push_front(v);
                    break;
                }
                if state.skip > 0 {
                    if ranked {
                        state.ranking_after = Some((
                            v["score"].as_f64().unwrap_or(0.),
                            v["id"].as_str().unwrap_or("").into(),
                        ));
                    }
                    state.skip -= 1;
                    continue;
                }
                bytes += size;
                if ranked {
                    state.ranking_after = Some((
                        v["score"].as_f64().unwrap_or(0.),
                        v["id"].as_str().unwrap_or("").into(),
                    ));
                }
                items.push(v);
                continue;
            }
            if state.exhausted {
                if ranked && state.rank_more {
                    state.exhausted = false;
                    state.rank_more = false;
                    state.ranking_done = false;
                    state.group_index = 0;
                    state.after = None;
                    continue;
                }
                break;
            }
            if is_walk {
                if state.walk.is_empty() {
                    state.exhausted = true;
                    if q.operation == "path" {
                        state.pending.push_back(json!({"state":"no_path_within_requested_depth_and_indexed_relations","coverage":"unresolved/provider gaps prevent runtime disconnection conclusions"}));
                    }
                    continue;
                }
                if let Some(v) = self.walk_one(&mut state, q, ctx.as_ref())? {
                    state.pending.push_back(v);
                }
                work += 1;
                continue;
            }
            if state.code.is_some() {
                if let Some(value) = self.code_step(&mut state, q)? {
                    state.pending.push_back(value);
                }
                work += 1;
                continue;
            }
            if state.input.is_empty() && state.group_index >= state.groups.len() {
                state.exhausted = true;
                if ranked && !state.ranking_done {
                    state.ranked.sort_by(|a, b| {
                        b["score"]
                            .as_f64()
                            .unwrap_or(0.)
                            .total_cmp(&a["score"].as_f64().unwrap_or(0.))
                            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
                    });
                    state.pending = std::mem::take(&mut state.ranked).into();
                    state.ranking_done = true;
                }
                continue;
            }
            if state.input.is_empty() {
                let b = self.reader.scan(
                    &state.groups[state.group_index],
                    state.after.as_deref(),
                    32.min(q.max_work.saturating_sub(work)).max(1),
                )?;
                state.after = b.after;
                if b.complete {
                    state.group_index += 1;
                    state.after = None;
                }
                state.input = b.rows.into();
            }
            if let Some(row) = state.input.pop_front() {
                work += 1;
                if row.key.starts_with("entity_vector:") && q.operation == "semantic" {
                    let e: Entity = serde_json::from_value(row.value["entity"].clone())?;
                    if !self.selected(&e, q, ctx.as_ref())? || !self.allowed(&e.provenance, q)? {
                        continue;
                    }
                    let index = search::SearchIndex::with_model(&[], self.model.clone());
                    if row.value["model"] != index.model_name() {
                        return Err(Error::new(
                            "VectorIndexModelMismatch",
                            "reindex with the configured token model",
                        ));
                    }
                    let query = state
                        .query_vector
                        .get_or_insert_with(|| index.prepare_vector(&search::tokenize(&q.text)));
                    let vector: Vec<f32> = serde_json::from_value(row.value["vector"].clone())?;
                    if vector.len() != query.len() {
                        return Err(Error::new("IndexIncomplete", "vector dimensions"));
                    }
                    let similarity = vector
                        .iter()
                        .zip(query.iter())
                        .map(|(a, b)| (*a * *b) as f64)
                        .sum::<f64>()
                        .max(0.);
                    let mut lexical = q.clone();
                    lexical.operation = "semantic_lexical".into();
                    let score = similarity * 2. + self.score(&e, &lexical)? * 0.1;
                    if score > 0. {
                        retain_ranked(&mut state, json!({"id":e.id,"score":score}));
                    }
                    continue;
                }
                if ["references", "occurrences"].contains(&q.operation.as_str())
                    && !state.visited.insert(row.key.clone())
                {
                    continue;
                }
                if q.operation == "search_code" {
                    let file: FileRevision = serde_json::from_value(row.value)?;
                    let name = file.logical_path.display();
                    let root: NativeValue =
                        serde_json::from_value(self.reader.metadata()["root"].clone())?;
                    if (!q.include_external && !file.logical_path.path().starts_with(root.path()))
                        || q.file
                            .as_ref()
                            .is_some_and(|p| name != *p && !name.ends_with(&format!("/{p}")))
                    {
                        continue;
                    }
                    state.code = Some(CodeScan {
                        file,
                        offset: 0,
                        start: 0,
                        line: 1,
                        carry: vec![],
                    });
                    continue;
                }
                if q.operation == "at" {
                    let (file, position) = state
                        .at
                        .as_ref()
                        .ok_or_else(|| Error::new("InvalidCursor", "missing source position"))?;
                    let Some(anchor) = row.value.get("source").filter(|v| !v.is_null()) else {
                        continue;
                    };
                    let anchor: SourceAnchor = serde_json::from_value(anchor.clone())?;
                    if anchor.file_revision != *file
                        || anchor.start > *position
                        || anchor.end < *position
                    {
                        continue;
                    }
                    if row.key.starts_with("occurrence:") {
                        let o: Occurrence = serde_json::from_value(row.value.clone())?;
                        if self.allowed(&o.provenance, q)?
                            && ctx.as_ref().is_none_or(|c| c.units.contains(&o.unit))
                        {
                            let definition = o
                                .declaration
                                .resolved()
                                .map(|id| self.entity(id))
                                .transpose()?
                                .flatten()
                                .as_ref()
                                .map(|e| self.summary_for(e, q.compact))
                                .transpose()?;
                            state.pending.push_back(json!({"occurrence":self.format_occurrence(&o, "", q.compact)?,"definition":definition}));
                        }
                    } else {
                        let e: Entity = serde_json::from_value(row.value)?;
                        if self.selected(&e, q, ctx.as_ref())? && self.allowed(&e.provenance, q)? {
                            state.pending.push_back(self.summary_for(&e, q.compact)?);
                        }
                    }
                    continue;
                }
                if let Some(v) =
                    self.transform(row.value, q, ctx.as_ref(), state.subject_id.as_deref())?
                {
                    if q.operation == "members" {
                        let id = v["id"]
                            .as_str()
                            .ok_or_else(|| Error::new("InvalidMember", "missing id"))?;
                        if !state.visited.insert(id.into()) {
                            continue;
                        }
                    }

                    if ranked {
                        retain_ranked(&mut state, v);
                    } else {
                        state.pending.push_back(v);
                    }
                }
            }
        }
        self.finish_page(q, state, items, work, start, digest, ranked)
    }
    #[allow(clippy::too_many_arguments)]
    fn finish_page(
        &self,
        q: &QueryRequest,
        state: State,
        items: Vec<Value>,
        work: usize,
        start: Instant,
        digest: String,
        ranked: bool,
    ) -> Result<QueryResult> {
        let text = state.text.is_some();
        let complete = state.exhausted && state.pending.is_empty() && !state.rank_more;
        let mut hidden = vec![];
        if !complete {
            hidden.push(HiddenGroup {
                category: "remaining_query_results".into(),
                count: None,
                reason: if ranked && !state.ranking_done {
                    "ranking corpus not exhausted; no global order claimed"
                } else {
                    "page/work/time budget; collapsed facts remain available"
                }
                .into(),
                continue_with: json!({"operation":"next"}),
            });
        }
        if !q.include_candidates
            && ["callees", "callers", "trace", "impact", "path"].contains(&q.operation.as_str())
        {
            let subject = self.subject(q)?;
            let count = self.reader.count(&group(
                if q.operation == "callers" || q.operation == "impact" {
                    "candidate_in"
                } else {
                    "candidate_out"
                },
                &subject.id,
            ))?;
            if count > 0 {
                hidden.push(HiddenGroup{category:"immediate_dispatch_or_implementation_candidates".into(),count:Some(count as usize),reason:"immediate candidate edges before authority/result filters; hidden by candidate policy".into(),continue_with:json!({"operation":q.operation,"subject":subject.id,"include_candidates":true})});
            }
        }
        if !q.compact && !q.include_external {
            hidden.push(HiddenGroup {
                category: "external_entities_in_index".into(),
                count: self.reader.metadata()["counts"]["external"]
                    .as_u64()
                    .map(|v| v as usize),
                reason: "project boundary; indexed facts remain available".into(),
                continue_with: json!({"operation":q.operation,"include_external":true}),
            });
        }
        let mut provenance_records = BTreeMap::new();
        if !q.compact {
            fn ids(v: &Value, found: &mut BTreeSet<String>) {
                match v {
                    Value::Object(m) => {
                        for (k, v) in m {
                            if k == "provenance" {
                                if let Some(s) = v.as_str() {
                                    found.insert(s.into());
                                }
                            } else {
                                ids(v, found);
                            }
                        }
                    }
                    Value::Array(a) => {
                        for v in a {
                            ids(v, found);
                        }
                    }
                    _ => {}
                }
            }
            let mut used = BTreeSet::new();
            for v in &items {
                ids(v, &mut used);
            }
            for id in used {
                if let Some(p) = self.value("provenance", &id)? {
                    provenance_records.insert(ProvenanceId(id), serde_json::from_value(p)?);
                }
            }
        }
        let coverage = if disclosure::reports_coverage(q) {
            self.reader
                .scan("all:coverage", None, 3)?
                .rows
                .into_iter()
                .map(|r| serde_json::from_value(r.value).map_err(Into::into))
                .collect::<Result<_>>()?
        } else {
            vec![]
        };
        // Query-local failures remain visible; index-wide diagnostics are
        // available through operation=gaps without taxing every result page.
        let mut gaps = state.extra_gaps.clone();
        if q.operation == "semantic"
            && !q.exact_semantic
            && state.groups.iter().any(|g| g.starts_with("vector_bucket:"))
        {
            gaps.push(Gap::new("ApproximateVectorCandidates",self.reader.revision(),"LSH candidate retrieval; exact_semantic=true scans the frozen vectors with bounded resumable ranking"));
        }
        let knowledge_partial = self.reader.metadata()["production"]["partial"] == true;
        let uncertain_absence =
            knowledge_partial && disclosure::compilation_absence(q, items.is_empty(), complete);
        if uncertain_absence {
            gaps.push(Gap::new("IncompleteKnowledge",self.reader.project(),"partial compilation coverage: an exhausted query does not establish absence in unavailable scopes"));
        }
        let continuation = if complete {
            None
        } else {
            let state = serde_json::to_value(state)?;
            if serde_json::to_vec(&state)?.len() > 16 * 1024 * 1024 {
                return Err(Error::new(
                    "CursorBudgetExceeded",
                    "query frontier/ranking exceeds 16 MiB; narrow scope",
                ));
            }
            Some(QueryContinuation {
                request: q.clone(),
                revision: self.reader.revision().into(),
                query_digest: digest,
                state: ContinuationState::Store {
                    project: self.reader.project().into(),
                    state,
                },
            })
        };
        Ok(QueryResult {
            page: QueryPage {
                revision: self.reader.revision().into(),
                context: q.context.clone(),
                items: items.clone(),
                provenance_records,
                total: None,
                total_kind: "unknown".into(),
                truncated: !complete,
                next_cursor: None,
                ranking: text.then(|| json!({"policy":"exact_name_then_candidate_windows","scope":"candidate_window","globally_sorted":false})),
                state: if items.is_empty() {
                    if complete {
                        if uncertain_absence {
                            "unknown_due_to_incomplete_compilation"
                        } else {
                            "no_matches_in_declared_scope"
                        }
                    } else {
                        "not_exhaustively_searched"
                    }
                } else {
                    "results"
                }
                .into(),
                coverage,
                gaps,
                hidden,
                work,
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
            continuation,
        })
    }
}
