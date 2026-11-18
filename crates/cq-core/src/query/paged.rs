//! Bounded query execution over a fixed FactReader; no project-wide adjacency
//! table or SemanticSnapshot is constructed on this path.
use super::*;
use crate::{
    Error, Result,
    ports::facts::{FactBatch, FactReader, FactRow, ReadStats, group},
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
struct CachedReader<'a> {
    inner: &'a dyn FactReader,
    cache: std::sync::Mutex<(usize, BTreeMap<String, FactRow>)>,
}
impl FactReader for CachedReader<'_> {
    fn project(&self) -> &str {
        self.inner.project()
    }
    fn revision(&self) -> &str {
        self.inner.revision()
    }
    fn metadata(&self) -> &Value {
        self.inner.metadata()
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<FactRow>> {
        let key = format!("{family}:{id}");
        if let Some(v) = self.cache.lock().unwrap().1.get(&key).cloned() {
            return Ok(Some(v));
        }
        let value = self.inner.get(family, id)?;
        if let Some(v) = &value {
            let n = serde_json::to_vec(v)?.len();
            if n < 128 * 1024 {
                let mut c = self.cache.lock().unwrap();
                if c.0 + n > 1024 * 1024 {
                    c.0 = 0;
                    c.1.clear();
                }
                c.0 += n;
                c.1.insert(key, v.clone());
            }
        }
        Ok(value)
    }
    fn scan(&self, g: &str, a: Option<&str>, n: usize) -> Result<FactBatch> {
        self.inner.scan(g, a, n)
    }
    fn count(&self, g: &str) -> Result<u64> {
        self.inner.count(g)
    }
    fn source_range(&self, f: &str, a: u64, b: u64) -> Result<Vec<u8>> {
        self.inner.source_range(f, a, b)
    }
    fn stats(&self) -> ReadStats {
        self.inner.stats()
    }
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
    fn summary(&self, e: &Entity) -> Result<Value> {
        Ok(
            json!({"id":e.id,"unit":e.unit,"kind":e.kind,"name":e.name,"qualified_name":e.qualified_name,"file":e.source.as_ref().map(|a|self.file(&a.file_revision)).transpose()?.flatten().map(|f|f.logical_path.display()),"source":e.source,"locations":e.locations,"stable_handle":crate::semantic::identity::query_handle(e),"signature":e.signature.chars().take(512).collect::<String>(),"declaring_scope":e.parent,"external":e.external,"exported":e.exported,"provenance":e.provenance}),
        )
    }
    fn context(&self, q: &QueryRequest) -> Result<Option<AnalysisContext>> {
        let Some(id) = &q.context else {
            return Ok(None);
        };
        if let Some(v) = self.value("context", id)? {
            return Ok(Some(serde_json::from_value(v)?));
        }
        let p = self.reader.scan("all:context", None, 1000)?;
        for row in p.rows {
            let c: AnalysisContext = serde_json::from_value(row.value)?;
            if c.entry_unit == *id {
                return Ok(Some(c));
            }
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
            .is_some_and(|k| !e.kind.label().eq_ignore_ascii_case(k))
        {
            return Ok(false);
        }
        if let Some(path) = &q.file {
            let f = e
                .source
                .as_ref()
                .map(|a| self.file(&a.file_revision))
                .transpose()?
                .flatten();
            if !f.is_some_and(|f| {
                let p = f.logical_path.display();
                p == *path || p.ends_with(&format!("/{path}"))
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
        if let Some(path) = &q.file {
            let mut after = None;
            let mut result = vec![];
            loop {
                let b = self.reader.scan("all:file", after.as_deref(), 256)?;
                for row in b.rows {
                    let f: FileRevision = serde_json::from_value(row.value)?;
                    if f.logical_path.display() == *path
                        || f.logical_path.display().ends_with(&format!("/{path}"))
                    {
                        result.push(group("file_entity", &f.id));
                    }
                }
                if b.complete {
                    break;
                }
                after = b.after;
            }
            return Ok(result);
        }
        if let Some(c) = self.context(q)? {
            return Ok(c.units.iter().map(|u| group("unit", u)).collect());
        }
        if let Some(kind) = &q.kind {
            return Ok(vec![group("kind_entity", &kind.to_ascii_lowercase())]);
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
        let mut after = None;
        let mut found = None;
        loop {
            let b = self.reader.scan("all:file", after.as_deref(), 128)?;
            for row in b.rows {
                let f: FileRevision = serde_json::from_value(row.value)?;
                let name = f.logical_path.display();
                if name == path || name.ends_with(&format!("/{path}")) {
                    if found.is_some() {
                        return Err(Error::new("AmbiguousFile", path));
                    }
                    found = Some(f);
                }
            }
            if b.complete {
                break;
            }
            after = b.after;
        }
        found.ok_or_else(|| Error::new("MissingFile", path))
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
            return Ok(matched.then(||json!({"file":scan.file.logical_path.display(),"line":line,"text":text,"next_line":(scan.start<scan.file.blob.len).then_some(line+1),"source":SourceAnchor{file_revision:scan.file.id.clone(),start,end:start+n as u64}})));
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
            "callers" | "implementations" => vec![group("in", &self.subject(q)?.id)],
            "callees" | "relations" => vec![group("out", &self.subject(q)?.id)],
            "references" | "occurrences" => {
                let e = self.subject(q)?;
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
                    let score = self.score(&e, q)?;
                    return Ok((score > 0.).then(|| json!({"id":e.id,"score":score})));
                }
                let mut value = self.summary(&e)?;
                if q.operation == "members" {
                    let owner = self.subject(q)?;
                    let b = self.reader.scan(
                        &group("membership_pair", &format!("{}\0{}", owner.id, e.id)),
                        None,
                        1000,
                    )?;
                    value["membership"] =
                        json!(b.rows.into_iter().map(|r| r.value).collect::<Vec<_>>());
                    value["membership_complete"] = json!(b.complete);
                }
                if q.operation == "outline"
                    && matches!(
                        e.kind,
                        EntityKind::Body
                            | EntityKind::File
                            | EntityKind::Variable
                            | EntityKind::Parameter
                    )
                {
                    return Ok(None);
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
                let occurrence = r
                    .occurrence
                    .as_ref()
                    .map(|id| self.value("occurrence", id))
                    .transpose()?
                    .flatten();
                Ok(Some(
                    json!({"entity":self.summary(&e)?,"relation":r,"occurrence":occurrence}),
                ))
            }
            "references" | "occurrences" => {
                let o: Occurrence = serde_json::from_value(v.clone())?;
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
                Ok(Some(v))
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
                    json!({"id":d.id,"kind":d.kind,"title":d.title,"text":lines[start..end].join("\n"),"start_line":start+1,"next_line":(end<lines.len()).then_some(end+1),"total_lines":lines.len(),"owner":d.owner,"source":d.source,"links":d.links,"provenance":d.provenance})
                } else {
                    json!({"id":d.id,"kind":d.kind,"title":d.title,"excerpt":d.text.chars().take(200).collect::<String>(),"owner":d.owner,"source":d.source,"links":d.links,"provenance":d.provenance})
                }))
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
                            v.push(json!({"entity":self.summary(&t)?,"authority":"RuleDerived","meaning":"native USR candidate; linker selection not observed"}));
                        }
                    }
                    v
                } else {
                    vec![self.summary(&e)?]
                }
            }
            "type_definition" => {
                let e = self.subject(q)?;
                let mut v = vec![];
                if e.type_navigation.is_empty() {
                    for id in &e.type_definitions {
                        if let Some(t) = self.entity(id)? {
                            v.push(self.summary(&t)?);
                        }
                    }
                } else {
                    for nav in &e.type_navigation {
                        let mut candidates = vec![];
                        for id in &nav.candidates {
                            if let Some(t) = self.entity(id)? {
                                candidates.push(self.summary(&t)?);
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
                    json!({"entity":self.summary(&e)?,"counts":counts,"hidden":[{"category":"members","count":counts["members"],"continue_with":{"operation":"members","subject":e.id}},{"category":"documents","count":counts["documents"],"continue_with":{"operation":"docs","subject":e.id}},{"category":"references","count":counts["references"],"continue_with":{"operation":"references","subject":e.id}},{"category":"bodies","count":counts["bodies"],"continue_with":{"operation":"bodies","subject":e.id}}]}),
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
                vec![json!({"unit":unit})]
            }
            _ => vec![],
        })
    }
    fn source(&self, q: &QueryRequest) -> Result<Vec<Value>> {
        let mut anchor_start = None;
        let file = if q.subject.is_some() {
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
                    rows.push(json!({"file":file.logical_path.display(),"line":line,"text":text,"next_line":(start+(n as u64)<file.blob.len).then_some(line+1),"source":SourceAnchor{file_revision:file.id.clone(),start,end:start+n as u64}}));
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
        if !step.reported {
            s.visited.insert(step.id.clone());
            step.reported = true;
            let e = self
                .entity(&step.id)?
                .ok_or_else(|| Error::new("DanglingEndpoint", &step.id))?;
            if q.operation == "path" && s.target.as_ref() == Some(&step.id) {
                s.walk.clear();
                s.exhausted = true;
                return Ok(Some(
                    json!({"target":self.summary(&e)?,"relation_path":step.path,"meaning":"potential static path, not runtime proof"}),
                ));
            }
            let row = if q.operation != "path" {
                Some(
                    json!({"entity":self.summary(&e)?,"hop":step.hop,"via":step.via.as_ref().map(|id|self.value("relation",id)).transpose()?.flatten(),"path":step.path}),
                )
            } else {
                None
            };
            if step.hop < q.depth {
                s.walk.push_front(step);
            }
            return Ok(row);
        }
        let inbound = q.direction == "inbound" || q.operation == "impact";
        let both = q.direction == "both";
        let back = if both { step.direction == 0 } else { inbound };
        let p = self.reader.scan(
            &group(if back { "in" } else { "out" }, &step.id),
            step.after.as_deref(),
            1,
        )?;
        for row in p.rows {
            let r: Relation = serde_json::from_value(row.value)?;
            if !self.edge_allowed(&r, q, true)? {
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
            s.walk.push_back(Step {
                id: id.clone(),
                hop: step.hop + 1,
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
        } else if both && step.direction == 0 {
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
        let cached = CachedReader {
            inner: request.as_deref().unwrap_or(self.reader),
            cache: std::sync::Mutex::new((0, BTreeMap::new())),
        };
        Engine {
            reader: &cached,
            model: self.model.clone(),
            control: self.control,
        }
        .execute_inner(q, resume)
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
        let start = Instant::now();
        self.reader.set_timeout(q.timeout_ms);
        let ctx = self.context(q)?;
        let digest = request_digest(q);
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
                        .map(|v| self.transform(v, q, ctx.as_ref()))
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
                if ranked && state.ranking_done && v.get("qualified_name").is_none() {
                    let id = v["id"]
                        .as_str()
                        .ok_or_else(|| Error::new("InvalidCursor", "rank identity"))?
                        .to_owned();
                    let score = v["score"].as_f64().unwrap_or(0.);
                    let e = self
                        .entity(&id)?
                        .ok_or_else(|| Error::new("MissingEntity", &id))?;
                    v = self.summary(&e)?;
                    v["score"] = json!(score);
                    v["entity_ref"] = json!(id);
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
                                .map(|e| self.summary(e))
                                .transpose()?;
                            state
                                .pending
                                .push_back(json!({"occurrence":o,"definition":definition}));
                        }
                    } else {
                        let e: Entity = serde_json::from_value(row.value)?;
                        if self.selected(&e, q, ctx.as_ref())? && self.allowed(&e.provenance, q)? {
                            state.pending.push_back(self.summary(&e)?);
                        }
                    }
                    continue;
                }
                if let Some(v) = self.transform(row.value, q, ctx.as_ref())? {
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
        if !q.include_external {
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
        let coverage = self
            .reader
            .scan("all:coverage", None, 3)?
            .rows
            .into_iter()
            .map(|r| serde_json::from_value(r.value).map_err(Into::into))
            .collect::<Result<_>>()?;
        let mut gaps: Vec<Gap> = self
            .reader
            .scan("all:gaps", None, 7)?
            .rows
            .into_iter()
            .map(|r| serde_json::from_value(r.value).map_err(Into::into))
            .collect::<Result<_>>()?;
        gaps.extend(state.extra_gaps.clone());
        if q.operation == "semantic"
            && !q.exact_semantic
            && state.groups.iter().any(|g| g.starts_with("vector_bucket:"))
        {
            gaps.push(Gap::new("ApproximateVectorCandidates",self.reader.revision(),"LSH candidate retrieval; exact_semantic=true scans the frozen vectors with bounded resumable ranking"));
        }
        let knowledge_partial = self.reader.metadata()["production"]["partial"] == true;
        if knowledge_partial {
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
                state: if items.is_empty() {
                    if complete {
                        if knowledge_partial {
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
