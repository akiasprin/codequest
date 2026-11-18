use super::*;
use crate::{Error, Result, ports::ContentReader};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

pub struct QueryEngine<'a> {
    pub snapshot: &'a SemanticSnapshot,
    pub content: &'a dyn ContentReader,
    index: HashMap<&'a str, usize>,
    files: HashMap<&'a str, usize>,
    boundaries: HashMap<&'a str, Vec<usize>>,
    out: HashMap<&'a str, Vec<usize>>,
    incoming: HashMap<&'a str, Vec<usize>>,
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
        for (i, e) in s.relations.iter().enumerate() {
            out.entry(&e.from).or_default().push(i);
            incoming.entry(&e.to).or_default().push(i);
        }
        let files = s
            .sources
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.as_str(), i))
            .collect();
        let mut boundaries: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, b) in s.bodies.iter().enumerate() {
            boundaries.entry(&b.id).or_default().push(i);
            if b.parent.is_none() && b.owner != b.id {
                boundaries.entry(&b.owner).or_default().push(i);
            }
        }
        Self {
            files,
            boundaries,
            snapshot: s,
            content,
            index,
            out,
            incoming,
            search,
        }
    }
    fn entity(&self, id: &str) -> Option<&Entity> {
        self.index.get(id).map(|i| &self.snapshot.entities[*i])
    }
    fn file(&self, id: &str) -> Option<&FileRevision> {
        self.files.get(id).map(|i| &self.snapshot.sources.files[*i])
    }
    pub fn summary(&self, e: &Entity) -> Value {
        let mut value = json!({"id":e.id,"unit":e.unit,"kind":e.kind,"name":e.name,"qualified_name":e.qualified_name,"file":e.source.as_ref().and_then(|a|self.file(&a.file_revision)).map(|f|f.logical_path.display()),"source":e.source,"locations":e.locations,"stable_handle":crate::semantic::identity::query_handle(e),"signature":e.signature.chars().take(512).collect::<String>(),"declaring_scope":e.parent,"external":e.external,"exported":e.exported,"provenance":e.provenance});
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
        if !boundaries.is_empty() {
            value["body_boundaries"] = json!(boundaries);
        }
        value
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
            .is_some_and(|kind| !e.kind.label().eq_ignore_ascii_case(kind))
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
                    &json!({"candidates_count":candidates.len(),"candidates":candidates.iter().take(7).map(|e|self.summary(e)).collect::<Vec<_>>(),"hidden_count":candidates.len().saturating_sub(7),"continue_with":{"operation":"search","text":subject,"context":q.context},"guidance":"choose an exact id or build context; hidden candidates remain indexed"}),
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
        let coverage: Vec<_> = self.snapshot.coverage.iter().take(3).cloned().collect();
        let gaps: Vec<_> = self.snapshot.gaps.iter().take(7).cloned().collect();
        let mut provenance_records = std::collections::BTreeMap::new();
        let metadata=serde_json::to_vec(&json!({"revision":self.snapshot.revision,"context":q.context,"coverage":coverage,"gaps":gaps}))?.len()+1024;
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
                let mut item = self.summary(entity);
                item["score"] = rows[offset]["score"].clone();
                if q.operation == "semantic" {
                    item["ranking_model"] = json!(self.search.model_name());
                }
                item
            } else {
                rows[offset].clone()
            };
            let mut ids = std::collections::BTreeSet::new();
            collect_provenance(&item, &mut ids);
            let mut added = std::collections::BTreeMap::new();
            for id in ids {
                if !provenance_records.contains_key(&id)
                    && let Some(p) = self.snapshot.provenance_records.get(&id)
                {
                    added.insert(id, p.clone());
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
        if !q.include_external {
            let count = self.snapshot.entities.iter().filter(|e| e.external).count();
            if count > 0 {
                hidden.push(HiddenGroup{category:"external_entities_in_index".into(),count:Some(count),reason:"project boundary; count is index-wide, query matches may be fewer".into(),continue_with:json!({"operation":q.operation,"subject":q.subject,"text":q.text,"include_external":true})});
            }
        }
        if coverage.len() < self.snapshot.coverage.len() {
            hidden.push(HiddenGroup {
                category: "capability_details".into(),
                count: Some(self.snapshot.coverage.len() - coverage.len()),
                reason: "progressive disclosure".into(),
                continue_with: json!({"operation":"coverage"}),
            });
        }
        if gaps.len() < self.snapshot.gaps.len() {
            hidden.push(HiddenGroup {
                category: "coverage_gaps".into(),
                count: Some(self.snapshot.gaps.len() - gaps.len()),
                reason:
                    "progressive disclosure; absence from this page is not absence from the index"
                        .into(),
                continue_with: json!({"operation":"gaps"}),
            });
        }
        Ok(QueryResult {
            page: QueryPage {
                provenance_records,
                revision: self.snapshot.revision.clone(),
                context: q.context.clone(),
                state: if items.is_empty() {
                    if complete {
                        "no_matches_in_declared_scope"
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
        })
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
                    result.push(json!({"unit":unit.id,"entity":entity.map(|e|self.summary(e)),"name":unit.name,"language":unit.language,"role":unit.role,"root":unit.root.value().map(|p|p.display()),"binding":unit.binding,"dependency_count":dependencies.len(),"dependencies":dependencies.iter().take(7).map(|d|json!({"alias":d.alias,"producer":d.producer})).collect::<Vec<_>>(),"hidden_dependencies":dependencies.len().saturating_sub(7),"continue_with":{"operation":"build_configuration","subject":unit.id}}));
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
                result.push(json!({"unit":unit,"dependencies":self.snapshot.build_dependencies.iter().filter(|d|d.consumer==unit.id).collect::<Vec<_>>()}));
            }
            "capabilities" | "coverage" => {
                result = self.snapshot.coverage.iter().map(|c| json!(c)).collect()
            }
            "gaps" => result = self.snapshot.gaps.iter().map(|g| json!(g)).collect(),
            "contexts" => result = self.snapshot.contexts.iter().map(|c| json!(c)).collect(),
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
                    if budget(work) {
                        complete = false;
                        break;
                    }
                    work += 1;
                    if (!q.include_external && e.external)
                        || !self.selected(e, q)
                        || !self.allowed(&e.provenance, q)
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
                        self.search.bm25_terms(i, &terms)
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
                            result = ids.iter().filter_map(|id| self.entity(id)).map(|target| json!({"entity":self.summary(target),"authority":"RuleDerived","meaning":"native USR candidate; linker selection not observed"})).collect();
                        } else {
                            result.push(self.summary(e));
                        }
                    }
                    "type_definition" => {
                        if e.type_navigation.is_empty() {
                            result = e
                                .type_definitions
                                .iter()
                                .filter_map(|id| self.entity(id))
                                .map(|e| self.summary(e))
                                .collect();
                        } else {
                            result=e.type_navigation.iter().map(|nav|json!({"source":nav.source,"extent":nav.extent,"file":self.file(&nav.source.file_revision).map(|f|f.logical_path.display()),"candidates":nav.candidates.iter().filter_map(|id|self.entity(id)).map(|e|self.summary(e)).collect::<Vec<_>>(),"meaning":"exact native navigation span; graph identities can have multiple semantic interpretations"})).collect();
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
                                let mut value = self.summary(member);
                                value["membership"] = json!(
                                    self.out
                                        .get(e.id.as_str())
                                        .into_iter()
                                        .flatten()
                                        .map(|i| &self.snapshot.relations[*i])
                                        .filter(|r| r.to == member.id
                                            && matches!(
                                                r.kind,
                                                RelationKind::Contains
                                                    | RelationKind::HasMember
                                                    | RelationKind::LexicallyContains
                                            )
                                            && self.allowed(&r.provenance, q))
                                        .collect::<Vec<_>>()
                                );
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
                        result.push(json!({"entity":self.summary(e),"counts":{"members":members,"documents":documents,"bodies":bodies,"references":references,"unresolved_calls":unresolved},"hidden":[{"category":"members","count":members,"continue_with":{"operation":"members","subject":e.id}},{"category":"documents","count":documents,"continue_with":{"operation":"docs","subject":e.id}},{"category":"references_and_calls","count":references,"continue_with":{"operation":"references","subject":e.id}},{"category":"body_boundaries","count":bodies,"continue_with":{"operation":"bodies","subject":e.id}}],"caveat":"collapsed facts still exist; unresolved calls can limit conclusions"}));
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
                        .map(|o| json!(o))
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
                        result.push(json!({"entity":self.entity(id).map(|e|self.summary(e)),"relation":r,"occurrence":r.occurrence.as_ref().and_then(|id|self.snapshot.occurrences.iter().find(|o|&o.id==id))}));
                    }
                }
            }
            "outline" => {
                let file = q
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::new("MissingFile", "outline needs a file"))?;
                for e in &self.snapshot.entities {
                    if self.selected(e, q)
                        && !matches!(
                            e.kind,
                            EntityKind::File
                                | EntityKind::Body
                                | EntityKind::Variable
                                | EntityKind::Parameter
                        )
                        && e.source
                            .as_ref()
                            .and_then(|a| self.file(&a.file_revision))
                            .is_some_and(|f| {
                                let p = f.logical_path.display();
                                p == *file || p.ends_with(&format!("/{file}"))
                            })
                    {
                        result.push(self.summary(e));
                    }
                }
                result.sort_by_key(|v| v["source"]["start"].as_u64().unwrap_or(0));
            }
            "source" | "search_code" => {
                let files: Vec<_> = if q.operation == "source" && q.subject.is_some() {
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
                            if line_no >= selected_start
                                && line_no < selected_start.saturating_add(q.max_lines)
                            {
                                result.push(json!({"file":file.logical_path.display(),"line":line_no,"text":line,"next_line":(line_no<text.lines().count()).then_some(line_no+1),"source":SourceAnchor{file_revision:file.id.clone(),start:byte_offset,end:byte_offset+line.len() as u64}}));
                            }
                        } else if re
                            .as_ref()
                            .map(|r| r.is_match(line))
                            .unwrap_or_else(|| line.contains(&q.text))
                        {
                            result.push(json!({"file":file.logical_path.display(),"line":line_no,"text":line,"next_line":(line_no<text.lines().count()).then_some(line_no+1),"source":SourceAnchor{file_revision:file.id.clone(),start:byte_offset,end:byte_offset+line.len() as u64}}));
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
                        result.push(json!({"id":d.id,"kind":d.kind,"title":d.title,"text":lines[start..end].join("\n"),"start_line":start+1,"next_line":(end<lines.len()).then_some(end+1),"total_lines":lines.len(),"owner":d.owner,"source":d.source,"links":d.links,"provenance":d.provenance}));
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
                            result.push(json!({"occurrence":o,"definition":o.declaration.resolved().and_then(|id|self.entity(id)).map(|e|self.summary(e))}));
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
                            result.push(self.summary(e));
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
                    let focus = self.entity(id).map(|e| self.summary(e));
                    let neighbors=self.out.get(id).into_iter().flatten().take(5).map(|i|{let e=&self.snapshot.relations[*i];json!({"kind":e.kind,"to":self.entity(&e.to).map(|e|self.summary(e)),"evidence":e.source})}).collect::<Vec<_>>();
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
                    rows.push(json!({"target":self.entity(&node.id).map(|e|self.summary(e)),"relation_path":node.path,"meaning":"potential static relationship path; deferred/dynamic boundaries remain in hop evidence"}));
                    found = true;
                    break;
                }
                if !path_mode {
                    rows.push(json!({"entity":self.entity(&node.id).map(|e|self.summary(e)),"hop":node.hop,"via":node.via.as_ref().and_then(|id|self.snapshot.relations.iter().find(|r|&r.id==id)),"path":node.path}));
                }
            }
            if node.hop >= q.depth {
                continue;
            }
            let inbound = q.direction == "inbound" || q.operation == "impact";
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
            let outgoing = if !inbound || q.direction == "both" {
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
                } else {
                    r.kind == RelationKind::Calls
                        || (q.include_candidates
                            && matches!(r.kind, RelationKind::MayCall | RelationKind::Dispatches))
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
                scheduled.insert(id.clone());
                queue.push_back(WalkNode {
                    id: id.clone(),
                    hop: node.hop + 1,
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
