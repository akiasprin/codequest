use super::*;
use crate::{Error, Result, ports::ContentReader};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct LineIndex {
    text: String,
    starts: Vec<usize>,
}
impl LineIndex {
    fn new(text: String) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { text, starts }
    }
    fn lookup(&self, byte_offset: u64) -> (usize, usize, String) {
        let offset = (byte_offset as usize).min(self.text.len());
        let idx = self
            .starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1);
        let line_start = self.starts[idx];
        let line_end = self.starts.get(idx + 1).copied().unwrap_or(self.text.len());
        let line_slice = self.text.get(line_start..line_end).unwrap_or_default();
        let snippet: String = line_slice.trim().chars().take(160).collect();
        (idx + 1, offset.saturating_sub(line_start) + 1, snippet)
    }
}

pub struct QueryEngine<'a> {
    pub snapshot: &'a SemanticSnapshot,
    pub content: &'a dyn ContentReader,
    index: HashMap<&'a str, usize>,
    files: HashMap<&'a str, usize>,
    boundaries: HashMap<&'a str, Vec<usize>>,
    body_owner: HashMap<&'a str, (&'a str, BodyKind)>,
    out: HashMap<&'a str, Vec<usize>>,
    incoming: HashMap<&'a str, Vec<usize>>,
    relation_ids: HashMap<&'a str, usize>,
    occurrences_sorted: bool,
    root_prefix: String,
    lines: Mutex<HashMap<&'a str, Option<Arc<LineIndex>>>>,
    search: std::sync::Arc<search::SearchIndex>,
}
pub struct QueryResult {
    pub page: QueryPage,
    pub continuation: Option<QueryContinuation>,
}
struct WalkPage {
    rows: Vec<Value>,
    offset: usize,
    total: Option<usize>,
    complete: bool,
    frontier: Option<ContinuationState>,
    work: usize,
}
impl<'a> QueryEngine<'a> {
    pub fn new(s: &'a SemanticSnapshot, content: &'a dyn ContentReader) -> Self {
        Self::with_search(
            s,
            content,
            std::sync::Arc::new(search::SearchIndex::new(&s.entities)),
        )
    }
    pub fn with_search(
        s: &'a SemanticSnapshot,
        content: &'a dyn ContentReader,
        search: std::sync::Arc<search::SearchIndex>,
    ) -> Self {
        let index = s
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.as_str(), i))
            .collect();
        let mut out: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut incoming: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut relation_ids = HashMap::with_capacity(s.relations.len());
        for (i, e) in s.relations.iter().enumerate() {
            out.entry(&e.from).or_default().push(i);
            incoming.entry(&e.to).or_default().push(i);
            relation_ids.insert(e.id.as_str(), i);
        }
        let files = s
            .sources
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.as_str(), i))
            .collect();
        let mut boundaries: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut body_owner: HashMap<&str, (&str, BodyKind)> =
            HashMap::with_capacity(s.bodies.len());
        for (i, b) in s.bodies.iter().enumerate() {
            boundaries.entry(&b.id).or_default().push(i);
            if b.parent.is_none() && b.owner != b.id {
                boundaries.entry(&b.owner).or_default().push(i);
            }
            body_owner.insert(b.id.as_str(), (b.owner.as_str(), b.kind));
        }
        let occurrences_sorted = s.occurrences.is_sorted_by(|a, b| a.id <= b.id);
        let root_display = s.root.display();
        let root_prefix = if root_display.is_empty() {
            String::new()
        } else {
            format!("{}/", root_display.trim_end_matches('/'))
        };
        Self {
            files,
            boundaries,
            body_owner,
            snapshot: s,
            content,
            index,
            out,
            incoming,
            relation_ids,
            occurrences_sorted,
            root_prefix,
            lines: Mutex::new(HashMap::new()),
            search,
        }
    }
    fn entity(&self, id: &str) -> Option<&'a Entity> {
        self.index.get(id).map(|i| &self.snapshot.entities[*i])
    }
    fn file(&self, id: &str) -> Option<&'a FileRevision> {
        self.files.get(id).map(|i| &self.snapshot.sources.files[*i])
    }
    fn occurrence(&self, id: &str) -> Option<&'a Occurrence> {
        if self.occurrences_sorted {
            self.snapshot
                .occurrences
                .binary_search_by(|o| o.id.as_str().cmp(id))
                .ok()
                .map(|i| &self.snapshot.occurrences[i])
        } else {
            self.snapshot.occurrences.iter().find(|o| o.id == id)
        }
    }
    fn display_path(&self, file: &FileRevision, compact: bool) -> String {
        let p = file.logical_path.display();
        if compact && !self.root_prefix.is_empty() {
            p.strip_prefix(&self.root_prefix).unwrap_or(&p).to_owned()
        } else {
            p
        }
    }
    fn line_index(&self, file_rev: &str) -> Option<Arc<LineIndex>> {
        if let Some(cached) = self.lines.lock().unwrap().get(file_rev) {
            return cached.clone();
        }
        let f = self.file(file_rev)?;
        let built = self
            .content
            .read_blob(&f.blob)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|text| Arc::new(LineIndex::new(text)));
        self.lines
            .lock()
            .unwrap()
            .insert(f.id.as_str(), built.clone());
        built
    }
    fn anchor_location(&self, anchor: &SourceAnchor, compact: bool) -> Option<AnchorLocation> {
        let f = self.file(&anchor.file_revision)?;
        let path = self.display_path(f, compact);
        let pos = self
            .line_index(&anchor.file_revision)
            .map(|idx| idx.lookup(anchor.start));
        Some((path, pos))
    }
    fn enclosing_owner(&self, id: &str) -> Option<(&'a Entity, Option<BodyKind>)> {
        if let Some(&(owner_id, kind)) = self.body_owner.get(id) {
            return self.entity(owner_id).map(|e| (e, Some(kind)));
        }
        let e = self.entity(id)?;
        if e.kind == EntityKind::Body {
            let kind = match e.name.as_str() {
                "Closure" => Some(BodyKind::Closure),
                "AsyncBlock" => Some(BodyKind::AsyncBlock),
                "Function" => Some(BodyKind::Function),
                "AsyncFunction" => Some(BodyKind::AsyncFunction),
                "ConstInitializer" => Some(BodyKind::ConstInitializer),
                "StaticInitializer" => Some(BodyKind::StaticInitializer),
                "Generated" => Some(BodyKind::Generated),
                _ => None,
            };
            return e
                .parent
                .as_deref()
                .and_then(|p| self.entity(p))
                .map(|owner| (owner, kind));
        }
        Some((e, None))
    }
    fn is_nested_body(&self, id: &str) -> bool {
        if let Some(&(_, kind)) = self.body_owner.get(id) {
            return !matches!(kind, BodyKind::Function | BodyKind::AsyncFunction);
        }
        self.entity(id).is_some_and(|e| {
            e.kind == EntityKind::Body && !matches!(e.name.as_str(), "Function" | "AsyncFunction")
        })
    }
    fn body_has_matching_call(&self, body_id: &str, q: &QueryRequest) -> bool {
        self.out.get(body_id).into_iter().flatten().any(|&i| {
            let r = &self.snapshot.relations[i];
            self.allowed(&r.provenance, q)
                && (r.kind == RelationKind::Calls
                    || (q.include_candidates
                        && matches!(r.kind, RelationKind::MayCall | RelationKind::Dispatches)))
                && self.entity(&r.to).is_none_or(|e| self.selected(e, q))
        })
    }
    pub fn summary(&self, e: &Entity) -> Value {
        self.summary_for(e, false)
    }
    fn summary_for(&self, e: &Entity, compact: bool) -> Value {
        let loc = e
            .source
            .as_ref()
            .and_then(|a| self.anchor_location(a, compact));
        let file = loc.as_ref().map(|(p, _)| p.as_str());
        let line = loc.as_ref().and_then(|(_, pos)| pos.as_ref().map(|p| p.0));
        let boundaries: Vec<_> = self
            .boundaries
            .get(e.id.as_str())
            .into_iter()
            .flatten()
            .map(|i| {
                let b = &self.snapshot.bodies[*i];
                json!({"kind":b.kind,"evaluation":b.evaluation})
            })
            .collect();
        let owner_info = (e.kind == EntityKind::Body)
            .then(|| self.enclosing_owner(&e.id).map(|(o, _)| o))
            .flatten();
        if compact {
            let mut map = serde_json::Map::new();
            map.insert("id".into(), json!(e.id));
            map.insert("kind".into(), json!(e.kind));
            map.insert("name".into(), json!(e.name));
            map.insert("unit".into(), json!(e.unit));
            if let Some(unit) = self.snapshot.units.iter().find(|u| u.id == e.unit) {
                map.insert(
                    "build_context".into(),
                    super::presentation::symbol_context(unit),
                );
            }
            if !e.qualified_name.is_empty() && e.qualified_name != e.name {
                map.insert("qualified_name".into(), json!(e.qualified_name));
            }
            if let Some(a) = &e.source {
                map.insert("file_revision".into(), json!(a.file_revision));
            }
            if let Some(f) = file {
                map.insert("file".into(), json!(f));
            }
            if let Some(l) = line {
                map.insert("line".into(), json!(l));
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
            if !boundaries.is_empty() {
                map.insert("body_boundaries".into(), json!(boundaries));
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
            return Value::Object(map);
        }
        let mut value = json!({"id":e.id,"unit":e.unit,"kind":e.kind,"name":e.name,"qualified_name":e.qualified_name,"file":file,"source":e.source,"stable_handle":crate::semantic::identity::query_handle(e),"signature":e.signature.chars().take(512).collect::<String>(),"declaring_scope":e.parent,"external":e.external,"exported":e.exported,"provenance":e.provenance});
        if let Some(l) = line {
            value["line"] = json!(l);
        }
        if !e.locations.is_empty() {
            value["locations"] = json!(e.locations);
        }
        if !boundaries.is_empty() {
            value["body_boundaries"] = json!(boundaries);
        }
        if let Some(owner) = owner_info {
            value["owner_id"] = json!(owner.id);
            value["owner_name"] = json!(if owner.qualified_name.is_empty() {
                &owner.name
            } else {
                &owner.qualified_name
            });
            value["owner_kind"] = json!(owner.kind);
        }
        value
    }
    fn format_occurrence(&self, o: &Occurrence, subject_id: &str, compact: bool) -> Value {
        let loc = self.anchor_location(&o.source, compact);
        let owner_resolved = o.owner.as_deref().and_then(|id| self.enclosing_owner(id));
        let owner_name = owner_resolved.map(|(e, _)| {
            if e.qualified_name.is_empty() {
                e.name.as_str()
            } else {
                e.qualified_name.as_str()
            }
        });
        let owner_id = owner_resolved.map(|(e, _)| e.id.as_str());
        let owner_kind = owner_resolved.map(|(e, _)| e.kind);
        let deferred_body = owner_resolved
            .and_then(|(_, bk)| bk)
            .filter(|bk| !matches!(bk, BodyKind::Function | BodyKind::AsyncFunction));
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
            if let Some(bk) = deferred_body {
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
                    if let Some(t) = self.entity(target_id) {
                        map.insert(
                            "target".into(),
                            json!(if t.qualified_name.is_empty() {
                                &t.name
                            } else {
                                &t.qualified_name
                            }),
                        );
                    }
                } else {
                    map.insert("declaration".into(), json!(o.declaration));
                }
            }
            if !o.locations.is_empty() {
                map.insert("locations".into(), json!(o.locations));
            }
            return Value::Object(map);
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
        if let Some(bk) = deferred_body {
            value["body_kind"] = json!(bk);
        }
        value
    }
    fn entity_label(&self, id: &str) -> String {
        if let Some(e) = self.entity(id) {
            if e.kind == EntityKind::Body
                && let Some((owner, _)) = self.enclosing_owner(id)
            {
                let owner_name = if owner.qualified_name.is_empty() {
                    &owner.name
                } else {
                    &owner.qualified_name
                };
                return format!("{}({})", e.name, owner_name);
            }
            if !e.qualified_name.is_empty() {
                return e.qualified_name.clone();
            }
            return e.name.clone();
        }
        id.to_owned()
    }
    fn format_relation_row(
        &self,
        subject_id: &str,
        target: Option<&Entity>,
        r: &Relation,
        compact: bool,
    ) -> Value {
        let occ = r.occurrence.as_deref().and_then(|id| self.occurrence(id));
        if compact {
            let mut map = serde_json::Map::new();
            if let Some(e) = target {
                map.insert("entity".into(), self.summary_for(e, true));
            }
            map.insert("relation".into(), json!(r.kind));
            let anchor = occ.map(|o| &o.source).or(r.source.as_ref());
            if let Some(a) = anchor
                && let Some((path, pos)) = self.anchor_location(a, true)
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
            return Value::Object(map);
        }
        json!({
            "entity": target.map(|e| self.summary_for(e, false)),
            "relation": r,
            "occurrence": occ.map(|o| self.format_occurrence(o, subject_id, false)),
        })
    }
    fn format_via(&self, r: &Relation, compact: bool) -> Value {
        if !compact {
            return json!(r);
        }
        let mut map = serde_json::Map::new();
        map.insert("kind".into(), json!(r.kind));
        map.insert("from".into(), json!(self.entity_label(&r.from)));
        map.insert("to".into(), json!(self.entity_label(&r.to)));
        let occ = r.occurrence.as_deref().and_then(|id| self.occurrence(id));
        let anchor = occ.map(|o| &o.source).or(r.source.as_ref());
        if let Some(a) = anchor
            && let Some((path, pos)) = self.anchor_location(a, true)
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
        Value::Object(map)
    }
    fn format_walk_row(&self, node: &WalkNode, compact: bool) -> Value {
        let entity = self.entity(&node.id).map(|e| self.summary_for(e, compact));
        let via = node
            .via
            .as_deref()
            .and_then(|id| self.relation_ids.get(id))
            .map(|&i| self.format_via(&self.snapshot.relations[i], compact));
        if compact {
            let mut map = serde_json::Map::new();
            map.insert("entity".into(), json!(entity));
            map.insert("hop".into(), json!(node.hop));
            if let Some(v) = via {
                map.insert("via".into(), v);
            }
            return Value::Object(map);
        }
        json!({"entity": entity, "hop": node.hop, "via": via, "path": node.path})
    }
    fn format_path_row(&self, node: &WalkNode, compact: bool) -> Value {
        let target = self.entity(&node.id).map(|e| self.summary_for(e, compact));
        if compact {
            let hops: Vec<Value> = node
                .path
                .iter()
                .filter_map(|id| self.relation_ids.get(id.as_str()))
                .map(|&i| self.format_via(&self.snapshot.relations[i], true))
                .collect();
            return json!({
                "target": target,
                "hops": hops,
                "meaning": "potential static relationship path; deferred/dynamic boundaries remain in hop evidence"
            });
        }
        json!({"target": target, "relation_path": node.path, "meaning": "potential static relationship path; deferred/dynamic boundaries remain in hop evidence"})
    }
    fn allowed(&self, id: &ProvenanceId, q: &QueryRequest) -> bool {
        q.authority.is_none_or(|a| {
            self.snapshot
                .provenance_records
                .get(id)
                .is_some_and(|p| p.authority == a)
        })
    }
    fn selected(&self, e: &Entity, q: &QueryRequest) -> bool {
        if e.external && !q.include_external {
            return false;
        }
        if let Some(context) = &q.context {
            let Some(ctx) = self
                .snapshot
                .contexts
                .iter()
                .find(|c| &c.id == context || &c.entry_unit == context)
            else {
                return false;
            };
            if !e.external && !ctx.units.contains(&e.unit) {
                return false;
            }
        }
        if q.kind
            .as_ref()
            .is_some_and(|kind| !super::presentation::kind_matches(e.kind, kind))
        {
            return false;
        }
        if let Some(file) = &q.file
            && !e
                .source
                .as_ref()
                .and_then(|a| self.file(&a.file_revision))
                .is_some_and(|f| {
                    let p = f.logical_path.display();
                    p == *file || p.ends_with(&format!("/{file}"))
                })
        {
            return false;
        }
        if let Some(directory) = &q.directory
            && !e
                .source
                .as_ref()
                .and_then(|a| self.file(&a.file_revision))
                .is_some_and(|f| {
                    crate::ports::facts::path_matches(&f.logical_path.display(), directory, true)
                })
        {
            return false;
        }
        true
    }
    fn resolve(&self, subject: &str, q: &QueryRequest) -> Result<&Entity> {
        let candidates: Vec<_> = self
            .snapshot
            .entities
            .iter()
            .filter(|e| {
                self.selected(e, q)
                    && (e.id == subject
                        || crate::semantic::identity::query_handle(e).as_deref() == Some(subject)
                        || e.qualified_name == subject
                        || e.name == subject)
            })
            .collect();
        if candidates.len() == 1 {
            Ok(candidates[0])
        } else if candidates.is_empty() {
            Err(Error::new("MissingSymbol", subject))
        } else {
            Err(Error::new(
                "AmbiguousSymbol",
                serde_json::to_string(
                    &json!({"candidates_count":candidates.len(),"candidates":candidates.iter().take(7).map(|e|self.summary_for(e, q.compact)).collect::<Vec<_>>(),"hidden_count":candidates.len().saturating_sub(7),"continue_with":{"operation":"search","text":subject,"context":q.context},"guidance":"choose an exact id or build context; hidden candidates remain indexed"}),
                )?,
            ))
        }
    }
    fn subject(&self, q: &QueryRequest) -> Result<&Entity> {
        let name = q.subject.as_deref().ok_or_else(|| {
            Error::new(
                "MissingSubject",
                "select an exact symbol id or qualified name",
            )
        })?;
        // Result filters describe neighbors/occurrences, not the identity of an
        // explicitly selected container or referenced external declaration.
        let mut identity = q.clone();
        identity.kind = None;
        identity.file = None;
        if self.entity(name).is_some()
            || self
                .snapshot
                .entities
                .iter()
                .any(|e| crate::semantic::identity::query_handle(e).as_deref() == Some(name))
        {
            identity.include_external = true;
        }
        self.resolve(name, &identity)
    }
    fn occurrence_selected(&self, o: &Occurrence, q: &QueryRequest) -> bool {
        if let Some(context) = &q.context
            && !self
                .snapshot
                .contexts
                .iter()
                .find(|c| &c.id == context || &c.entry_unit == context)
                .is_some_and(|c| c.units.contains(&o.unit))
        {
            return false;
        }
        q.file.as_ref().is_none_or(|path| {
            self.file(&o.source.file_revision).is_some_and(|f| {
                let actual = f.logical_path.display();
                actual == *path || actual.ends_with(&format!("/{path}"))
            })
        })
    }
    pub fn execute(
        &self,
        q: &QueryRequest,
        resume: Option<QueryContinuation>,
    ) -> Result<QueryResult> {
        if q.operation == "semantic" && !super::embeddings::enabled() {
            return Err(Error::new(
                "VectorSearchDisabled",
                "vector retrieval is experimental and disabled by default",
            ));
        }
        if !["calls", "data_flow", "types"].contains(&q.mode.as_str()) {
            return Err(Error::new(
                "UnsupportedMode",
                "trace mode must be calls, data_flow or types; no full SSA/runtime inference",
            ));
        }
        if q.max_lines == 0 || q.max_lines > 1000 || q.line == Some(0) || q.column == Some(0) {
            return Err(Error::new(
                "InvalidBudget",
                "positive one-based lines/columns; max_lines 1..1000",
            ));
        }
        if q.operation == "search_code" && q.text.is_empty() {
            return Err(Error::new(
                "MissingText",
                "source search needs a nonempty literal or regex",
            ));
        }
        if q.limit == 0
            || q.limit > 1000
            || q.max_bytes < 128
            || q.max_work == 0
            || q.timeout_ms == 0
            || q.depth > 4096
        {
            return Err(Error::new(
                "InvalidBudget",
                "limit 1..1000, bytes >=128, positive work/time, depth <=4096",
            ));
        }
        if q.context.as_ref().is_some_and(|id| {
            !self
                .snapshot
                .contexts
                .iter()
                .any(|c| &c.id == id || &c.entry_unit == id)
        }) {
            return Err(Error::new("MissingContext", "unknown build context"));
        }
        super::presentation::validate_scope(q)?;
        let languages: Vec<_> = self.snapshot.units.iter().map(|u| u.language).collect();
        if let Some(gap) = super::presentation::semantic_file_language(q, &languages) {
            return Ok(QueryResult {
                page: super::presentation::unavailable_language_page(
                    &self.snapshot.revision,
                    gap,
                    q,
                )?,
                continuation: None,
            });
        }
        if let Some(path) = &q.file
            && !self
                .snapshot
                .sources
                .files
                .iter()
                .any(|f| crate::ports::facts::path_matches(&f.logical_path.display(), path, false))
            && self
                .snapshot
                .sources
                .files
                .iter()
                .any(|f| crate::ports::facts::path_matches(&f.logical_path.display(), path, true))
        {
            return Err(Error::new(
                "DirectoryPassedAsFile",
                "file requires an exact file path; use directory for a subtree",
            ));
        }
        let started = Instant::now();
        let hash = request_digest(q);
        if let Some(c) = &resume
            && (c.revision != self.snapshot.revision || c.query_digest != hash)
        {
            return Err(Error::new(
                "StaleCursor",
                "cursor revision or query differs",
            ));
        }
        let (rows, mut offset, total, complete, walk_state, work) = match resume.map(|r| r.state) {
            Some(ContinuationState::Rows {
                rows,
                offset,
                total,
                complete,
                after,
            }) => (rows, offset, total, complete, after.map(|s| *s), 0),
            state if matches!(q.operation.as_str(), "trace" | "path" | "impact") => {
                let p = self.walk(q, state, started)?;
                (p.rows, p.offset, p.total, p.complete, p.frontier, p.work)
            }
            Some(ContinuationState::Scan {
                position,
                rows: mut accumulated,
            }) => {
                let (mut rows, complete, work) = self.rows(q, started, position)?;
                accumulated.append(&mut rows);
                if complete {
                    if ["search", "semantic", "entrypoints"].contains(&q.operation.as_str()) {
                        accumulated.sort_by(|a, b| {
                            b["score"]
                                .as_f64()
                                .unwrap_or(0.)
                                .total_cmp(&a["score"].as_f64().unwrap_or(0.))
                                .then_with(|| {
                                    a["entity_ref"].as_str().cmp(&b["entity_ref"].as_str())
                                })
                        });
                    }
                    let total = Some(accumulated.len());
                    (
                        accumulated,
                        q.offset,
                        total,
                        true,
                        None,
                        work.saturating_sub(position),
                    )
                } else {
                    if work == position {
                        return Err(Error::new(
                            "QueryWorkBudgetTooSmall",
                            "increase timeout_ms to make scan progress",
                        ));
                    }
                    (
                        vec![],
                        0,
                        None,
                        false,
                        Some(ContinuationState::Scan {
                            position: work,
                            rows: accumulated,
                        }),
                        work - position,
                    )
                }
            }
            Some(_) => return Err(Error::new("InvalidCursor", "continuation kind mismatch")),
            None => {
                let (rows, complete, work) = self.rows(q, started, 0)?;
                if complete {
                    let total = Some(rows.len());
                    (rows, q.offset, total, true, None, work)
                } else {
                    (
                        vec![],
                        0,
                        None,
                        false,
                        Some(ContinuationState::Scan {
                            position: work,
                            rows,
                        }),
                        work,
                    )
                }
            }
        };
        if offset > rows.len() {
            return Err(Error::new("InvalidCursor", "row offset out of bounds"));
        }
        let coverage: Vec<_> = if disclosure::reports_coverage(q) {
            self.snapshot.coverage.iter().take(3).cloned().collect()
        } else {
            vec![]
        };
        let knowledge_partial = self.snapshot.production.as_ref().is_some_and(|p| p.partial);
        let uncertain_absence =
            knowledge_partial && disclosure::compilation_absence(q, rows.is_empty(), complete);
        // Global gaps are explicitly pageable through operation=gaps, never a
        // sample attached to unrelated reads (or charged to their byte budget).
        let mut gaps = if uncertain_absence {
            vec![Gap::new(
                "IncompleteKnowledge",
                &self.snapshot.project,
                "partial compilation coverage: an exhausted query does not establish absence in unavailable scopes",
            )]
        } else {
            vec![]
        };
        if q.operation == "search"
            && let Some(kind) = &q.kind
        {
            let mut scope = q.clone();
            scope.kind = None;
            if self.snapshot.entities.iter().any(|e| {
                (e.name.eq_ignore_ascii_case(&q.text)
                    || e.qualified_name.eq_ignore_ascii_case(&q.text))
                    && !super::presentation::kind_matches(e.kind, kind)
                    && self.selected(e, &scope)
                    && self.allowed(&e.provenance, q)
            }) {
                gaps.push(Gap::new("KindFilterExcludesExactName",q.file.as_deref().unwrap_or(&q.text),
                    "exact-name matches were excluded by kind; omit kind or use Callable for functions and methods"));
            }
        }
        let mut provenance_records = std::collections::BTreeMap::new();
        let metadata = serde_json::to_vec(
            &json!({"revision":self.snapshot.revision,"context":q.context,"coverage":coverage,"gaps":gaps}),
        )?
        .len()
            + if q.compact { 256 } else { 1024 };
        if metadata > q.max_bytes {
            return Err(Error::new(
                "OutputBudgetTooSmall",
                format!("coverage and revision metadata need at least {metadata} bytes"),
            ));
        }
        let mut items = vec![];
        let mut bytes = metadata;
        while offset < rows.len() && items.len() < q.limit {
            let item = if let Some(id) = rows[offset]["entity_ref"].as_str() {
                let entity = self.entity(id).ok_or_else(|| {
                    Error::new(
                        "InvalidCursor",
                        "search entity missing from pinned revision",
                    )
                })?;
                let mut item = self.summary_for(entity, q.compact);
                item["score"] = rows[offset]["score"].clone();
                if q.operation == "semantic" {
                    item["ranking_model"] = json!(self.search.model_name());
                }
                item
            } else {
                rows[offset].clone()
            };
            let mut added = std::collections::BTreeMap::new();
            if !q.compact {
                let mut ids = std::collections::BTreeSet::new();
                collect_provenance(&item, &mut ids);
                for id in ids {
                    if !provenance_records.contains_key(&id)
                        && let Some(p) = self.snapshot.provenance_records.get(&id)
                    {
                        added.insert(id, p.clone());
                    }
                }
            }
            let size = serde_json::to_vec(&item)?.len() + serde_json::to_vec(&added)?.len() + 4;
            if size > q.max_bytes.saturating_sub(bytes) {
                if items.is_empty() {
                    return Err(Error::new(
                        "OutputBudgetTooSmall",
                        format!(
                            "one item needs {size} bytes; raise max_bytes or request a source/document page"
                        ),
                    ));
                }
                break;
            }
            provenance_records.extend(added);
            items.push(item);
            bytes += size;
            offset += 1;
        }
        let next = if offset < rows.len() {
            Some(QueryContinuation {
                request: q.clone(),
                revision: self.snapshot.revision.clone(),
                query_digest: hash,
                state: ContinuationState::Rows {
                    rows: rows.clone(),
                    offset,
                    total,
                    complete,
                    after: walk_state.clone().map(Box::new),
                },
            })
        } else {
            walk_state.map(|state| QueryContinuation {
                request: q.clone(),
                revision: self.snapshot.revision.clone(),
                query_digest: hash,
                state,
            })
        };
        let truncated = next.is_some() || !complete;
        let mut hidden = if truncated {
            vec![HiddenGroup {
                category: "remaining_query_results".into(),
                count: total.map(|n| n.saturating_sub(offset)),
                reason: if next.is_some() {
                    "page/work budget"
                } else {
                    "work budget exhausted; increase max_work/timeout_ms"
                }
                .into(),
                continue_with: json!({"operation":q.operation,"subject":q.subject,"revision":self.snapshot.revision}),
            }]
        } else {
            vec![]
        };
        if !q.include_candidates
            && matches!(
                q.operation.as_str(),
                "callees" | "callers" | "trace" | "path" | "impact"
            )
            && let Ok(subject) = self.subject(q)
        {
            let adjacency = if q.operation == "callers" || q.operation == "impact" {
                &self.incoming
            } else {
                &self.out
            };
            let count = adjacency
                .get(subject.id.as_str())
                .into_iter()
                .flatten()
                .filter(|i| {
                    matches!(
                        self.snapshot.relations[**i].kind,
                        RelationKind::MayCall | RelationKind::Dispatches
                    ) && self.allowed(&self.snapshot.relations[**i].provenance, q)
                })
                .count();
            if count > 0 {
                hidden.push(HiddenGroup{category:"immediate_dispatch_or_implementation_candidates".into(),count:Some(count),reason:"bound declarations/possible bodies in this immediate neighborhood are hidden by candidate policy; an empty result is not disconnection".into(),continue_with:json!({"operation":q.operation,"subject":subject.id,"include_candidates":true})});
            }
        }
        if !q.compact {
            if !q.include_external {
                let count = self.snapshot.entities.iter().filter(|e| e.external).count();
                if count > 0 {
                    hidden.push(HiddenGroup{category:"external_entities_in_index".into(),count:Some(count),reason:"project boundary; count is index-wide, query matches may be fewer".into(),continue_with:json!({"operation":q.operation,"subject":q.subject,"text":q.text,"include_external":true})});
                }
            }
            if disclosure::reports_coverage(q) && coverage.len() < self.snapshot.coverage.len() {
                hidden.push(HiddenGroup {
                    category: "capability_details".into(),
                    count: Some(self.snapshot.coverage.len() - coverage.len()),
                    reason: "progressive disclosure".into(),
                    continue_with: json!({"operation":"coverage"}),
                });
            }
        }
        let mut result = QueryResult {
            page: QueryPage {
                ranking: None,
                provenance_records,
                revision: self.snapshot.revision.clone(),
                context: q.context.clone(),
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
                items,
                total,
                total_kind: if total.is_some() { "exact" } else { "unknown" }.into(),
                truncated,
                next_cursor: None,
                coverage,
                gaps,
                hidden,
                work,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            continuation: next,
        };
        if disclosure::is_overview(q) {
            for item in &mut result.page.items {
                if let Some(e) = item["id"].as_str().and_then(|id| self.entity(id)) {
                    item["_search_unit"] = json!(e.unit);
                    item["_search_module"] =
                        disclosure::module(e, |id| Ok(self.entity(id).cloned()))?
                            .unwrap_or(Value::Null);
                }
            }
        }
        {
            let deferred = disclosure::apply(&mut result.page, q)?;
            if !deferred.is_empty() {
                result.continuation = Some(QueryContinuation {
                    request: q.clone(),
                    revision: result.page.revision.clone(),
                    query_digest: request_digest(q),
                    state: ContinuationState::Rows {
                        rows: deferred,
                        offset: 0,
                        total: result.page.total,
                        complete: true,
                        after: result.continuation.take().map(|c| Box::new(c.state)),
                    },
                });
            }
        }
        Ok(result)
    }
    fn rows(
        &self,
        q: &QueryRequest,
        started: Instant,
        skip: usize,
    ) -> Result<(Vec<Value>, bool, usize)> {
        let mut result = vec![];
        let mut work: usize = 0;
        let mut complete = true;
        let budget = |work: usize| {
            work.saturating_sub(skip) >= q.max_work
                || started.elapsed() >= Duration::from_millis(q.timeout_ms)
        };
        let scoped = if ["overview", "topology", "hotspots", "cycles", "clusters"]
            .contains(&q.operation.as_str())
            && q.context.is_some()
        {
            let mut scope = self.snapshot.clone();
            scope.entities.retain(|e| self.selected(e, q));
            let ids: HashSet<_> = scope.entities.iter().map(|e| e.id.as_str()).collect();
            scope
                .relations
                .retain(|r| ids.contains(r.from.as_str()) && ids.contains(r.to.as_str()));
            Some(scope)
        } else {
            None
        };
        let graph = scoped.as_ref().unwrap_or(self.snapshot);
        if ["hotspots", "cycles", "clusters"].contains(&q.operation.as_str())
            && graph.entities.len() + graph.relations.len() > q.max_work
        {
            return Err(Error::new(
                "QueryWorkBudget",
                "architecture exceeds max_work; select a context or raise the explicit budget",
            ));
        }
        match q.operation.as_str() {
            "runtime_traces" => {
                result = self
                    .snapshot
                    .runtime_traces
                    .iter()
                    .map(|t| json!(t))
                    .collect()
            }
            "overview" => result = architecture::overview(graph),
            "topology" => {
                for unit in &self.snapshot.units {
                    if q.context.as_ref().is_some_and(|id| {
                        !self.snapshot.contexts.iter().any(|ctx| {
                            (&ctx.id == id || &ctx.entry_unit == id) && ctx.units.contains(&unit.id)
                        })
                    }) {
                        continue;
                    }
                    let entity = self
                        .snapshot
                        .entities
                        .iter()
                        .find(|e| e.unit == unit.id && e.kind == EntityKind::CompilationUnit);
                    let dependencies = self
                        .snapshot
                        .build_dependencies
                        .iter()
                        .filter(|d| d.consumer == unit.id)
                        .collect::<Vec<_>>();
                    result.push(json!({"unit":unit.id,"entity":entity.map(|e|self.summary_for(e, q.compact)),"name":unit.name,"language":unit.language,"role":unit.role,"root":unit.root.value().map(|p|p.display()),"binding":unit.binding,"dependency_count":dependencies.len(),"dependencies":dependencies.iter().take(7).map(|d|json!({"alias":d.alias,"producer":d.producer})).collect::<Vec<_>>(),"hidden_dependencies":dependencies.len().saturating_sub(7),"continue_with":{"operation":"build_configuration","subject":unit.id}}));
                }
            }
            "build_configuration" => {
                let subject = q
                    .subject
                    .as_ref()
                    .ok_or_else(|| Error::new("MissingSubject", "select a compilation unit"))?;
                let unit = self
                    .snapshot
                    .units
                    .iter()
                    .find(|u| &u.id == subject)
                    .or_else(|| {
                        self.entity(subject)
                            .and_then(|e| self.snapshot.units.iter().find(|u| u.id == e.unit))
                    })
                    .ok_or_else(|| Error::new("MissingUnit", subject))?;
                result.push(if q.compact && q.configuration_view == "summary" { super::presentation::unit_summary(unit) }
                    else { json!({"unit":unit,"dependencies":self.snapshot.build_dependencies.iter().filter(|d|d.consumer==unit.id).collect::<Vec<_>>()}) });
            }
            "capabilities" | "coverage" => {
                result = self.snapshot.coverage.iter().map(|c| json!(c)).collect()
            }
            "gaps" => result = self.snapshot.gaps.iter().map(|g| json!(g)).collect(),
            "contexts" => {
                result = self
                    .snapshot
                    .contexts
                    .iter()
                    .filter(|c| {
                        q.context
                            .as_ref()
                            .is_none_or(|id| c.id == *id || c.entry_unit == *id)
                    })
                    .map(|c| {
                        if !q.compact {
                            return json!(c);
                        }
                        let mut v = self
                            .snapshot
                            .units
                            .iter()
                            .find(|u| u.id == c.entry_unit)
                            .map(super::presentation::unit_summary)
                            .unwrap_or(json!({}));
                        v["id"] = json!(c.id);
                        v["entry_unit"] = json!(c.entry_unit);
                        v["status"] = json!(c.status);
                        v["dependency_units"] = json!(c.units.len());
                        v
                    })
                    .collect()
            }
            "hotspots" => result = architecture::hotspots(graph),
            "cycles" => result = architecture::cycles(graph),
            "clusters" => result = architecture::clusters(graph),
            "schema" => {
                let mut nodes = std::collections::BTreeMap::new();
                let mut edges = std::collections::BTreeMap::new();
                for e in &self.snapshot.entities {
                    *nodes.entry(e.kind.label()).or_insert(0usize) += 1;
                }
                for e in &self.snapshot.relations {
                    *edges.entry(e.kind.label()).or_insert(0usize) += 1;
                }
                result.push(json!({"nodes":nodes,"relationships":edges,"node_properties":["id","unit","kind","name","qualified_name","source","signature","external","provenance"],"relation_properties":["id","kind","source","occurrence","provenance"],"separate_occurrences":true}));
            }
            "search" | "semantic" | "entrypoints" => {
                let re = if q.regex {
                    Some(
                        regex::Regex::new(&q.text)
                            .map_err(|e| Error::new("InvalidPattern", e.to_string()))?,
                    )
                } else {
                    None
                };
                let terms = search::tokenize(&q.text);
                let q_lower = q.text.to_lowercase();
                let q_ascii = q_lower.is_ascii();
                let q_bytes = q_lower.as_bytes();
                let vector = if q.operation == "semantic" {
                    self.search.prepare_vector(&terms)
                } else {
                    vec![]
                };
                let mut scored = vec![];
                for (i, e) in self.snapshot.entities.iter().enumerate() {
                    if work < skip {
                        work += 1;
                        continue;
                    }
                    if work.saturating_sub(skip) >= q.max_work
                        || ((work & 1023) == 0
                            && started.elapsed() >= Duration::from_millis(q.timeout_ms))
                    {
                        complete = false;
                        break;
                    }
                    work += 1;
                    if (!q.include_external && e.external)
                        || !self.selected(e, q)
                        || !self.allowed(&e.provenance, q)
                        || !disclosure::visible(e, q)
                    {
                        continue;
                    }
                    let score = if q.operation == "entrypoints" {
                        if e.name == "main" || e.attributes.contains_key("test") {
                            10.
                        } else if matches!(e.kind, EntityKind::Function | EntityKind::Method)
                            && e.exported
                        {
                            1.
                        } else {
                            0.
                        }
                    } else if let Some(re) = &re {
                        if re.is_match(&e.name) || re.is_match(&e.qualified_name) {
                            1.
                        } else {
                            0.
                        }
                    } else if q.text.is_empty() {
                        1.
                    } else if q.operation == "semantic" {
                        self.search.semantic_vector(i, &vector) * 2.
                            + self.search.bm25_terms(i, &terms) * 0.1
                    } else {
                        let qn_match = if q_ascii && e.qualified_name.is_ascii() {
                            !q_bytes.is_empty()
                                && e.qualified_name
                                    .as_bytes()
                                    .windows(q_bytes.len())
                                    .any(|w| w.eq_ignore_ascii_case(q_bytes))
                        } else {
                            e.qualified_name.to_lowercase().contains(&q_lower)
                        };
                        self.search.bm25_terms(i, &terms)
                            + if e.name.eq_ignore_ascii_case(&q.text) {
                                20.
                            } else if qn_match {
                                2.
                            } else {
                                0.
                            }
                    };
                    if score > 0. {
                        scored.push((i, score));
                    }
                }
                scored.sort_by(|(a, sa), (b, sb)| {
                    sb.total_cmp(sa).then_with(|| {
                        self.snapshot.entities[*a]
                            .id
                            .cmp(&self.snapshot.entities[*b].id)
                    })
                });
                result = scored
                    .into_iter()
                    .map(|(i, score)| json!({"entity_ref":self.snapshot.entities[i].id,"score":score}))
                    .collect();
            }
            "details" | "definition" | "type_definition" | "members" => {
                let e = self.subject(q)?;
                let contained: HashSet<_> = self
                    .out
                    .get(e.id.as_str())
                    .into_iter()
                    .flatten()
                    .filter_map(|i| {
                        let r = &self.snapshot.relations[*i];
                        (matches!(
                            r.kind,
                            RelationKind::Contains
                                | RelationKind::HasMember
                                | RelationKind::LexicallyContains
                        ) && self.allowed(&r.provenance, q))
                        .then_some(r.to.as_str())
                    })
                    .collect();
                let owned_bodies: HashSet<_> = self
                    .snapshot
                    .bodies
                    .iter()
                    .filter(|b| b.owner == e.id)
                    .map(|b| b.id.as_str())
                    .collect();
                match q.operation.as_str() {
                    "definition" => {
                        if let Some(candidates) = e.attributes.get("definition_candidates") {
                            let ids: Vec<String> = serde_json::from_str(candidates)?;
                            result = ids.iter().filter_map(|id| self.entity(id)).map(|target| json!({"entity":self.summary_for(target, q.compact),"authority":"RuleDerived","meaning":"native USR candidate; linker selection not observed"})).collect();
                        } else {
                            result.push(self.summary_for(e, q.compact));
                        }
                    }
                    "type_definition" => {
                        if e.type_navigation.is_empty() {
                            result = e
                                .type_definitions
                                .iter()
                                .filter_map(|id| self.entity(id))
                                .map(|e| self.summary_for(e, q.compact))
                                .collect();
                        } else {
                            result=e.type_navigation.iter().map(|nav|json!({"source":nav.source,"extent":nav.extent,"file":self.file(&nav.source.file_revision).map(|f|self.display_path(f, q.compact)),"candidates":nav.candidates.iter().filter_map(|id|self.entity(id)).map(|e|self.summary_for(e, q.compact)).collect::<Vec<_>>(),"meaning":"exact native navigation span; graph identities can have multiple semantic interpretations"})).collect();
                        }
                    }
                    "members" => {
                        result = self
                            .snapshot
                            .entities
                            .iter()
                            .filter(|n| self.selected(n, q) && self.allowed(&n.provenance, q))
                            .filter(|n| {
                                n.parent.as_deref() == Some(&e.id)
                                    || contained.contains(n.id.as_str())
                            })
                            .map(|member| {
                                let mut value = self.summary_for(member, q.compact);
                                let membership: Vec<_> = self
                                    .out
                                    .get(e.id.as_str())
                                    .into_iter()
                                    .flatten()
                                    .map(|i| &self.snapshot.relations[*i])
                                    .filter(|r| {
                                        r.to == member.id
                                            && matches!(
                                                r.kind,
                                                RelationKind::Contains
                                                    | RelationKind::HasMember
                                                    | RelationKind::LexicallyContains
                                            )
                                            && self.allowed(&r.provenance, q)
                                    })
                                    .collect();
                                if q.compact {
                                    let kinds: Vec<_> = membership.iter().map(|r| r.kind).collect();
                                    if !kinds.is_empty() {
                                        value["membership"] = json!(kinds);
                                    }
                                } else {
                                    value["membership"] = json!(membership);
                                }
                                value
                            })
                            .collect()
                    }
                    _ => {
                        let members = self
                            .snapshot
                            .entities
                            .iter()
                            .filter(|n| self.selected(n, q) && self.allowed(&n.provenance, q))
                            .filter(|n| {
                                n.parent.as_deref() == Some(&e.id)
                                    || contained.contains(n.id.as_str())
                            })
                            .count();
                        let documents = self
                            .snapshot
                            .documents
                            .iter()
                            .filter(|d| d.owner.as_deref() == Some(&e.id))
                            .count();
                        let bodies = self
                            .snapshot
                            .bodies
                            .iter()
                            .filter(|b| b.owner == e.id)
                            .count();
                        let references = self
                            .snapshot
                            .occurrences
                            .iter()
                            .filter(|o| o.declaration.resolved() == Some(&e.id))
                            .count();
                        let unresolved = self
                            .snapshot
                            .occurrences
                            .iter()
                            .filter(|o| {
                                o.kind == RelationKind::Calls
                                    && o.executable.resolved().is_none()
                                    && o.owner
                                        .as_ref()
                                        .is_some_and(|id| owned_bodies.contains(id.as_str()))
                            })
                            .count();
                        result.push(json!({"entity":self.summary_for(e, q.compact),"counts":{"members":members,"documents":documents,"bodies":bodies,"references":references,"unresolved_calls":unresolved},"hidden":[{"category":"members","count":members,"continue_with":{"operation":"members","subject":e.id}},{"category":"documents","count":documents,"continue_with":{"operation":"docs","subject":e.id}},{"category":"references_and_calls","count":references,"continue_with":{"operation":"references","subject":e.id}},{"category":"body_boundaries","count":bodies,"continue_with":{"operation":"bodies","subject":e.id}}],"caveat":"collapsed facts still exist; unresolved calls can limit conclusions"}));
                    }
                }
            }
            "bodies" => {
                let e = self.subject(q)?;
                result = self
                    .snapshot
                    .bodies
                    .iter()
                    .filter(|b| b.owner == e.id)
                    .map(|b| json!(b))
                    .collect();
            }
            "references" | "occurrences" | "callers" | "callees" | "implementations"
            | "relations" => {
                let e = self.subject(q)?;
                let owned_bodies: HashSet<_> = self
                    .snapshot
                    .bodies
                    .iter()
                    .filter(|b| b.owner == e.id)
                    .map(|b| b.id.as_str())
                    .collect();
                if q.operation == "references" || q.operation == "occurrences" {
                    result = self
                        .snapshot
                        .occurrences
                        .iter()
                        .filter(|o| {
                            self.allowed(&o.provenance, q) && self.occurrence_selected(o, q)
                        })
                        .filter(|o| {
                            o.declaration.resolved() == Some(&e.id)
                                || o.owner.as_deref() == Some(&e.id)
                                || o.owner
                                    .as_ref()
                                    .is_some_and(|id| owned_bodies.contains(id.as_str()))
                        })
                        .filter(|o| {
                            q.relation
                                .as_ref()
                                .is_none_or(|r| RelationKind::parse(r) == Some(o.kind))
                        })
                        .map(|o| self.format_occurrence(o, &e.id, q.compact))
                        .collect();
                } else {
                    let inbound = q.operation == "callers" || q.operation == "implementations";
                    let edge_indices = if inbound {
                        self.incoming.get(e.id.as_str())
                    } else {
                        self.out.get(e.id.as_str())
                    };
                    for i in edge_indices.into_iter().flatten() {
                        let r = &self.snapshot.relations[*i];
                        if !self.allowed(&r.provenance, q) {
                            continue;
                        }
                        if matches!(q.operation.as_str(), "callers" | "callees")
                            && r.kind != RelationKind::Calls
                            && !(q.include_candidates
                                && matches!(
                                    r.kind,
                                    RelationKind::MayCall | RelationKind::Dispatches
                                ))
                        {
                            continue;
                        }
                        if q.operation == "implementations"
                            && !matches!(r.kind, RelationKind::Implements | RelationKind::Overrides)
                        {
                            continue;
                        }
                        if q.relation
                            .as_ref()
                            .is_some_and(|k| RelationKind::parse(k) != Some(r.kind))
                        {
                            continue;
                        }
                        let id = if inbound { &r.from } else { &r.to };
                        if self.entity(id).is_some_and(|e| !self.selected(e, q)) {
                            continue;
                        }
                        result.push(self.format_relation_row(&e.id, self.entity(id), r, q.compact));
                    }
                }
            }
            "outline" => {
                let file = q
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::new("MissingFile", "outline needs a file"))?;
                let mut entries = vec![];
                for e in &self.snapshot.entities {
                    if self.selected(e, q)
                        && !matches!(
                            e.kind,
                            EntityKind::File
                                | EntityKind::Body
                                | EntityKind::Variable
                                | EntityKind::Parameter
                        )
                        && !(q.kind.is_none() && e.kind == EntityKind::DocSection)
                        && e.source
                            .as_ref()
                            .and_then(|a| self.file(&a.file_revision))
                            .is_some_and(|f| {
                                let p = f.logical_path.display();
                                p == *file || p.ends_with(&format!("/{file}"))
                            })
                    {
                        let start = e.source.as_ref().map(|a| a.start).unwrap_or(0);
                        entries.push((start, self.summary_for(e, q.compact)));
                    }
                }
                entries.sort_by_key(|(start, _)| *start);
                result = entries.into_iter().map(|(_, v)| v).collect();
            }
            "source" | "search_code" => {
                let files: Vec<_> = if q.operation == "source" && q.file_revision.is_some() {
                    vec![
                        self.file(q.file_revision.as_deref().unwrap())
                            .ok_or_else(|| {
                                Error::new("MissingFile", "unknown frozen file revision")
                            })?,
                    ]
                } else if q.operation == "source" && q.subject.is_some() {
                    let e = self.subject(q)?;
                    vec![
                        self.file(
                            &e.source
                                .as_ref()
                                .ok_or_else(|| {
                                    Error::new("SourceUnavailable", "symbol has no fixed source")
                                })?
                                .file_revision,
                        )
                        .unwrap(),
                    ]
                } else {
                    self.snapshot
                        .sources
                        .files
                        .iter()
                        .filter(|f| {
                            q.include_external
                                || f.logical_path.path().starts_with(self.snapshot.root.path())
                        })
                        .filter(|f| {
                            q.file.as_ref().is_none_or(|p| {
                                f.logical_path.display() == *p
                                    || f.logical_path.display().ends_with(&format!("/{p}"))
                            })
                        })
                        .collect()
                };
                let re = if q.regex {
                    Some(
                        regex::Regex::new(&q.text)
                            .map_err(|e| Error::new("InvalidPattern", e.to_string()))?,
                    )
                } else {
                    None
                };
                for file in files {
                    if work >= skip && budget(work) {
                        complete = false;
                        break;
                    }
                    let bytes = self.content.read_blob(&file.blob)?;
                    let Ok(text) = std::str::from_utf8(&bytes) else {
                        continue;
                    };
                    let selected_start =
                        if q.operation == "source" && q.line.is_none() && q.subject.is_some() {
                            self.subject(q)?
                                .extent
                                .as_ref()
                                .or(self.subject(q)?.source.as_ref())
                                .filter(|a| a.file_revision == file.id)
                                .map(|a| {
                                    text.as_bytes()[..a.start as usize]
                                        .iter()
                                        .filter(|b| **b == b'\n')
                                        .count()
                                        + 1
                                })
                                .unwrap_or(1)
                        } else {
                            q.line.unwrap_or(1)
                        };
                    let total_lines = text.lines().count();
                    let display_file = self.display_path(file, q.compact);
                    let end_line = selected_start.saturating_add(q.max_lines);
                    let mut byte_offset = 0;
                    for (index, line) in text.split_inclusive('\n').enumerate() {
                        if work < skip {
                            work += 1;
                            byte_offset += line.len() as u64;
                            continue;
                        }
                        work += 1;
                        let line_no = index + 1;
                        if q.operation == "source" {
                            if line_no >= selected_start && line_no < end_line {
                                if q.compact {
                                    let mut row =
                                        json!({"file":&display_file,"line":line_no,"text":line});
                                    if line_no + 1 == end_line && line_no < total_lines {
                                        row["next_line"] = json!(line_no + 1);
                                    }
                                    result.push(row);
                                } else {
                                    result.push(json!({"file":&display_file,"line":line_no,"text":line,"next_line":(line_no<total_lines).then_some(line_no+1),"source":SourceAnchor{file_revision:file.id.clone(),start:byte_offset,end:byte_offset+line.len() as u64}}));
                                }
                            }
                        } else if re
                            .as_ref()
                            .map(|r| r.is_match(line))
                            .unwrap_or_else(|| line.contains(&q.text))
                        {
                            if q.compact {
                                result
                                    .push(json!({"file":&display_file,"line":line_no,"text":line}));
                            } else {
                                result.push(json!({"file":&display_file,"line":line_no,"text":line,"next_line":(line_no<total_lines).then_some(line_no+1),"source":SourceAnchor{file_revision:file.id.clone(),start:byte_offset,end:byte_offset+line.len() as u64}}));
                            }
                        }
                        byte_offset += line.len() as u64;
                        if budget(work) {
                            complete = false;
                            break;
                        }
                    }
                }
            }
            "docs" | "doc_search" | "document" => {
                for d in &self.snapshot.documents {
                    if !self.allowed(&d.provenance, q) {
                        continue;
                    }
                    if work < skip {
                        work += 1;
                        continue;
                    }
                    if budget(work) {
                        complete = false;
                        break;
                    }
                    work += 1;
                    if q.document_kind
                        .as_ref()
                        .is_some_and(|kind| !format!("{:?}", d.kind).eq_ignore_ascii_case(kind))
                    {
                        continue;
                    }
                    if !q.include_external
                        && d.owner
                            .as_ref()
                            .and_then(|id| self.entity(id))
                            .is_some_and(|e| e.external)
                    {
                        continue;
                    }
                    if q.subject
                        .as_ref()
                        .is_some_and(|id| id != &d.id && d.owner.as_ref() != Some(id))
                    {
                        continue;
                    }
                    if q.operation == "doc_search"
                        && !d.text.to_lowercase().contains(&q.text.to_lowercase())
                        && !d.title.to_lowercase().contains(&q.text.to_lowercase())
                    {
                        continue;
                    }
                    if q.operation == "document" {
                        let lines: Vec<_> = d.text.lines().collect();
                        let start = q.line.unwrap_or(1).saturating_sub(1).min(lines.len());
                        let end = start.saturating_add(q.max_lines).min(lines.len());
                        if q.compact {
                            let loc = self.anchor_location(&d.source, true);
                            let mut row = json!({"id":d.id,"kind":d.kind,"title":d.title,"text":lines[start..end].join("\n"),"start_line":start+1,"next_line":(end<lines.len()).then_some(end+1),"total_lines":lines.len()});
                            if let Some((path, pos)) = loc {
                                row["file"] = json!(path);
                                if let Some((l, _, _)) = pos {
                                    row["line"] = json!(l);
                                }
                            }
                            if let Some(owner) = &d.owner {
                                row["owner"] = json!(self.entity_label(owner));
                                row["owner_id"] = json!(owner);
                            }
                            result.push(row);
                        } else {
                            result.push(json!({"id":d.id,"kind":d.kind,"title":d.title,"text":lines[start..end].join("\n"),"start_line":start+1,"next_line":(end<lines.len()).then_some(end+1),"total_lines":lines.len(),"owner":d.owner,"source":d.source,"links":d.links,"provenance":d.provenance}));
                        }
                    } else if q.compact {
                        let loc = self.anchor_location(&d.source, true);
                        let mut row = json!({"id":d.id,"kind":d.kind,"title":d.title,"excerpt":d.text.chars().take(200).collect::<String>()});
                        if let Some((path, pos)) = loc {
                            row["file"] = json!(path);
                            if let Some((l, _, _)) = pos {
                                row["line"] = json!(l);
                            }
                        }
                        if let Some(owner) = &d.owner {
                            row["owner"] = json!(self.entity_label(owner));
                            row["owner_id"] = json!(owner);
                        }
                        result.push(row);
                    } else {
                        result.push(json!({"id":d.id,"kind":d.kind,"title":d.title,"excerpt":d.text.chars().take(200).collect::<String>(),"owner":d.owner,"source":d.source,"links":d.links}));
                    }
                }
            }
            "at" => {
                let path = q
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::new("MissingFile", "at needs file/line/column"))?;
                for file in &self.snapshot.sources.files {
                    if file.logical_path.display() != *path
                        && !file.logical_path.display().ends_with(&format!("/{path}"))
                    {
                        continue;
                    }
                    let bytes = self.content.read_blob(&file.blob)?;
                    let mut offset = 0usize;
                    for line in bytes
                        .split_inclusive(|b| *b == b'\n')
                        .take(q.line.unwrap_or(1).saturating_sub(1))
                    {
                        offset += line.len();
                    }
                    offset += q.column.unwrap_or(1).saturating_sub(1);
                    for o in &self.snapshot.occurrences {
                        if o.source.file_revision == file.id
                            && o.source.start <= offset as u64
                            && o.source.end >= offset as u64
                        {
                            result.push(json!({"occurrence":self.format_occurrence(o, "", q.compact),"definition":o.declaration.resolved().and_then(|id|self.entity(id)).map(|e|self.summary_for(e, q.compact))}));
                        }
                    }
                    for e in &self.snapshot.entities {
                        if self.selected(e, q)
                            && e.source.as_ref().is_some_and(|a| {
                                a.file_revision == file.id
                                    && a.start <= offset as u64
                                    && a.end >= offset as u64
                            })
                        {
                            result.push(self.summary_for(e, q.compact));
                        }
                    }
                }
            }
            "tour" => {
                let mut lookup = q.clone();
                lookup.operation = "search".into();
                let (ranked, finished, w) = self.rows(&lookup, started, skip)?;
                work = w;
                complete = finished;
                for entity in ranked.into_iter().take(q.limit) {
                    let id = entity["entity_ref"].as_str().ok_or_else(|| {
                        Error::new("InvalidSearchRow", "tour seed has no entity identity")
                    })?;
                    let focus = self.entity(id).map(|e| self.summary_for(e, q.compact));
                    let neighbors=self.out.get(id).into_iter().flatten().take(5).map(|i|{let e=&self.snapshot.relations[*i];json!({"kind":e.kind,"to":self.entity(&e.to).map(|e|self.summary_for(e, q.compact)),"evidence":e.source})}).collect::<Vec<_>>();
                    result.push(json!({"focus":focus,"neighbors":neighbors,"selection":"ranked by question relevance; author intent still requires cited statements or reviewable annotations"}));
                }
            }
            _ => return Err(Error::new("UnsupportedOperation", &q.operation)),
        }
        Ok((result, complete, work))
    }
    fn walk(
        &self,
        q: &QueryRequest,
        state: Option<ContinuationState>,
        started: Instant,
    ) -> Result<WalkPage> {
        let (initial, visited, target, path_mode) = match state {
            Some(ContinuationState::Walk {
                queue,
                visited,
                target,
                path_mode,
            }) => (queue, visited, target, path_mode),
            Some(_) => return Err(Error::new("InvalidCursor", "walk cursor expected")),
            None => {
                let e = self.subject(q)?;
                (
                    vec![WalkNode {
                        id: e.id.clone(),
                        hop: 0,
                        path: vec![],
                        via: None,
                        edge_offset: None,
                    }],
                    vec![],
                    q.target
                        .as_ref()
                        .map(|s| self.resolve(s, q).map(|e| e.id.clone()))
                        .transpose()?,
                    q.operation == "path",
                )
            }
        };
        let mut queue: VecDeque<_> = initial.into();
        let mut scheduled: HashSet<_> = queue.iter().map(|n| n.id.clone()).collect();
        let mut seen: HashSet<_> = visited.into_iter().collect();
        let mut rows = vec![];
        let mut work = 0;
        let mut complete = true;
        let mut found = false;
        'walk: while let Some(node) = queue.pop_front() {
            if work >= q.max_work
                || started.elapsed() >= Duration::from_millis(q.timeout_ms)
                || (!path_mode && rows.len() >= q.limit)
            {
                queue.push_front(node);
                complete = false;
                break;
            }
            if node.edge_offset.is_none() {
                if !seen.insert(node.id.clone()) {
                    continue;
                }
                work += 1;
                if path_mode && target.as_ref() == Some(&node.id) {
                    rows.push(self.format_path_row(&node, q.compact));
                    found = true;
                    break;
                }
                if !path_mode {
                    rows.push(self.format_walk_row(&node, q.compact));
                }
            }
            let inbound = q.direction == "inbound" || q.operation == "impact";
            let calls_mode = q.relation.is_none() && q.mode == "calls";
            let node_is_nested_body = calls_mode && self.is_nested_body(&node.id);
            let ascend_body_only = q.depth > 0
                && node.hop >= q.depth
                && node_is_nested_body
                && (inbound || q.direction == "both");
            if node.hop >= q.depth && !ascend_body_only {
                continue;
            }
            let incoming = self
                .incoming
                .get(node.id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let outgoing = self
                .out
                .get(node.id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let incoming = if inbound || q.direction == "both" {
                incoming
            } else {
                &[]
            };
            let outgoing = if (!inbound || q.direction == "both") && !ascend_body_only {
                outgoing
            } else {
                &[]
            };
            let edges = incoming
                .iter()
                .map(|i| (*i, true))
                .chain(outgoing.iter().map(|i| (*i, false)));
            for (offset, (i, back)) in edges.enumerate().skip(node.edge_offset.unwrap_or(0)) {
                if work >= q.max_work || started.elapsed() >= Duration::from_millis(q.timeout_ms) {
                    queue.push_front(WalkNode {
                        edge_offset: Some(offset),
                        ..node
                    });
                    complete = false;
                    break 'walk;
                }
                work += 1;
                let r = &self.snapshot.relations[i];
                if !self.allowed(&r.provenance, q) {
                    continue;
                }
                let allowed = if let Some(k) = &q.relation {
                    RelationKind::parse(k) == Some(r.kind)
                } else if q.mode == "data_flow" {
                    r.kind == RelationKind::PassesValue
                } else if q.mode == "types" {
                    matches!(
                        r.kind,
                        RelationKind::UsesType
                            | RelationKind::Extends
                            | RelationKind::Implements
                            | RelationKind::Overrides
                    )
                } else if ascend_body_only {
                    back && r.kind == RelationKind::CreatesBody
                } else {
                    r.kind == RelationKind::Calls
                        || (q.include_candidates
                            && matches!(r.kind, RelationKind::MayCall | RelationKind::Dispatches))
                        || (r.kind == RelationKind::CreatesBody
                            && if back {
                                node_is_nested_body
                            } else {
                                self.is_nested_body(&r.to) && self.body_has_matching_call(&r.to, q)
                            })
                };
                if !allowed {
                    continue;
                }
                let id = if back { &r.from } else { &r.to };
                if seen.contains(id) || scheduled.contains(id) {
                    continue;
                }
                if self.entity(id).is_some_and(|e| !self.selected(e, q)) {
                    continue;
                }
                let mut path = node.path.clone();
                path.push(r.id.clone());
                let next_hop = if r.kind == RelationKind::CreatesBody {
                    node.hop
                } else {
                    node.hop + 1
                };
                scheduled.insert(id.clone());
                queue.push_back(WalkNode {
                    id: id.clone(),
                    hop: next_hop,
                    path,
                    via: Some(r.id.clone()),
                    edge_offset: None,
                });
            }
        }
        let continuation = if !complete {
            let mut visited: Vec<_> = seen.into_iter().collect();
            visited.sort();
            Some(ContinuationState::Walk {
                queue: queue.into(),
                visited,
                target,
                path_mode,
            })
        } else {
            None
        };
        if path_mode && !found && complete {
            rows.push(json!({"state":"no_path_within_requested_depth_and_indexed_relations","coverage":"consult unresolved calls and provider coverage; not a proof of runtime disconnection"}));
        }
        let total = if complete && path_mode {
            Some(rows.len())
        } else {
            None
        };
        Ok(WalkPage {
            rows,
            offset: 0,
            total,
            complete,
            frontier: continuation,
            work,
        })
    }
}

fn collect_provenance(value: &Value, ids: &mut std::collections::BTreeSet<ProvenanceId>) {
    match value {
        Value::Object(map) => {
            if let Some(id) = map.get("provenance").and_then(Value::as_str) {
                ids.insert(ProvenanceId(id.into()));
            }
            for v in map.values() {
                collect_provenance(v, ids);
            }
        }
        Value::Array(rows) => {
            for row in rows {
                collect_provenance(row, ids);
            }
        }
        _ => {}
    }
}
