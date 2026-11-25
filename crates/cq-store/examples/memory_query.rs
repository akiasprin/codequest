//! Experiment: query one published revision entirely in memory.
//!
//! `export` turns a format-2 revision into a single snapshot file (no database);
//! `serve` loads that file once and answers MCP stdio requests with the existing
//! in-memory `QueryEngine`; `bench` replays JSON-lines tool calls and reports
//! latency. No StoreHost, Ladybug or paged fact reader is involved after export.
use cq_core::{
    Error, Result,
    model::*,
    ports::{
        ContentReader, ContentRepository,
        facts::{FactRef, ObjectRef, ReadStats},
    },
    query::{QueryContinuation, QueryEngine, QueryRequest, search::SearchIndex},
    semantic::production::{ProvenanceObservation, ScopeDescriptor, UnitObservation},
};
use cq_store::{
    content::BlobStore,
    facts::{objects::Objects, tree, writer},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

fn memory() -> Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb / 1024)
    };
    json!({"rss_mib":field("VmRSS:"),"peak_rss_mib":field("VmHWM:")})
}

fn log(stage: &str, started: Instant, extra: Value) {
    eprintln!(
        "{}",
        json!({"stage":stage,"seconds":started.elapsed().as_secs_f64(),"memory":memory(),"extra":extra})
    );
}

fn counts(s: &SemanticSnapshot) -> Value {
    json!({
        "entities": s.entities.len(),
        "external_entities": s.entities.iter().filter(|e| e.external).count(),
        "occurrences": s.occurrences.len(),
        "relations": s.relations.len(),
        "bodies": s.bodies.len(),
        "documents": s.documents.len(),
        "files": s.sources.files.len(),
        "gaps": s.gaps.len(),
    })
}

struct Rec {
    key: String,
    offset: u64,
    length: u64,
    digest: String,
}

/// Records decoded from one batch of pages; merged after all workers finish.
#[derive(Default)]
struct Part {
    entities: Vec<Entity>,
    relations: Vec<Relation>,
    occurrences: Vec<Occurrence>,
    bodies: Vec<Body>,
    documents: Vec<DocumentBlock>,
    runtime_traces: Vec<RuntimeTrace>,
    units: Vec<BuildUnit>,
    contexts: Vec<AnalysisContext>,
    builds: Vec<BuildObservation>,
    files: Vec<FileRevision>,
    provenance: Vec<(ProvenanceId, Provenance)>,
    ordinal: Vec<(String, usize, Value)>,
    scopes: Vec<ScopeDescriptor>,
    observed_units: Vec<(usize, UnitObservation)>,
    observed_provenance: Vec<(usize, ProvenanceObservation)>,
    working_tree: Vec<(String, String)>,
    orderings: Vec<(String, Vec<String>)>,
    metadata: Vec<(String, Value)>,
}

fn ordinal(key: &str, id: &str) -> Result<usize> {
    id.parse::<usize>()
        .map_err(|_| Error::new("InvalidFactKey", key))
}

fn route(part: &mut Part, key: &str, v: Value) -> Result<()> {
    let (family, id) = key
        .split_once(':')
        .ok_or_else(|| Error::new("InvalidFactKey", key))?;
    macro_rules! push {
        ($field:ident) => {
            part.$field.push(serde_json::from_value(v)?)
        };
    }
    match family {
        "scope" => part
            .scopes
            .push(serde_json::from_value(v["descriptor"].clone())?),
        "observation_unit" => part
            .observed_units
            .push((ordinal(key, id)?, serde_json::from_value(v)?)),
        "observation_provenance" => part
            .observed_provenance
            .push((ordinal(key, id)?, serde_json::from_value(v)?)),
        "entity" => push!(entities),
        "relation" => push!(relations),
        "occurrence" => push!(occurrences),
        "body" => push!(bodies),
        "document" => push!(documents),
        "runtime_trace" => push!(runtime_traces),
        "unit" => push!(units),
        "context" => push!(contexts),
        "build" => push!(builds),
        "file" => push!(files),
        "provenance" => part
            .provenance
            .push((ProvenanceId(id.into()), serde_json::from_value(v)?)),
        "providers"
        | "coverage"
        | "gaps"
        | "analysis_inputs"
        | "analysis_directories"
        | "build_dependencies" => part.ordinal.push((family.into(), ordinal(key, id)?, v)),
        "working_tree" => part
            .working_tree
            .push((id.into(), serde_json::from_value(v)?)),
        "term_df" | "search_stats" | "context_stats" | "entity_vector" => {}
        "order" => part.orderings.push((id.into(), serde_json::from_value(v)?)),
        "metadata" => part.metadata.push((id.into(), v)),
        _ => return Err(Error::new("UnknownFactFamily", family)),
    }
    Ok(())
}

/// Same result as `writer::export`, but each payload page is read once.
/// `writer::export` decodes facts in tree (hash) order through a 32 MiB page
/// cache, so on a large revision nearly every fact re-reads a full page.
fn export_grouped(objects: &Objects, r: &writer::Revision) -> Result<SemanticSnapshot> {
    let started = Instant::now();
    let mut pages: HashMap<String, (ObjectRef, Vec<Rec>)> = HashMap::new();
    let mut facts = 0usize;
    tree::visit(objects, &r.roots.facts, |key, v| {
        let f: FactRef = serde_json::from_value(v.clone())?;
        let entry = pages
            .entry(f.object.hash.clone())
            .or_insert_with(|| (f.object.clone(), vec![]));
        entry.1.push(Rec {
            key: key.to_owned(),
            offset: f.offset,
            length: f.length,
            digest: f.digest,
        });
        facts += 1;
        Ok(())
    })?;
    log(
        "export.inventory",
        started,
        json!({"facts":facts,"pages":pages.len()}),
    );
    let decode = Instant::now();
    let mut pages: Vec<_> = pages.into_values().collect();
    pages.sort_by(|a, b| a.0.hash.cmp(&b.0.hash));
    let workers = std::thread::available_parallelism().map_or(8, |n| n.get());
    let per = pages.len().div_ceil(workers * 8).max(1);
    let mut batches = vec![];
    while !pages.is_empty() {
        let rest = pages.split_off(pages.len().min(per));
        batches.push(std::mem::replace(&mut pages, rest));
    }
    let parts = cq_core::compute::map(batches, |batch| {
        let mut part = Part::default();
        let mut stats = ReadStats::default();
        for (object, recs) in batch {
            let bytes = objects.read(&object, &mut stats)?;
            for rec in recs {
                let end = rec
                    .offset
                    .checked_add(rec.length)
                    .ok_or_else(|| Error::new("InvalidFactRef", "offset overflow"))?;
                let row = bytes
                    .get(rec.offset as usize..end as usize)
                    .ok_or_else(|| Error::new("InvalidFactRef", "page bounds"))?;
                if digest(row) != rec.digest {
                    return Err(Error::new("DigestMismatch", &rec.key));
                }
                route(&mut part, &rec.key, serde_json::from_slice(row)?)?;
            }
        }
        Ok(part)
    })?;
    log("export.decode", decode, json!({"batches":parts.len()}));

    let mut s = SemanticSnapshot::empty(
        r.project.clone(),
        serde_json::from_value(r.metadata["root"].clone())?,
    );
    s.schema_version = r.metadata["schema_version"].as_u64().unwrap_or(2) as u32;
    s.revision = r.revision.clone();
    s.previous = r.metadata["previous"].as_str().map(str::to_owned);
    s.created_ms = r.metadata["created_ms"].as_u64().unwrap_or(0);
    let mut all = Part::default();
    for mut p in parts {
        macro_rules! take {
            ($($field:ident),*) => {$(all.$field.append(&mut p.$field);)*};
        }
        take!(
            entities,
            relations,
            occurrences,
            bodies,
            documents,
            runtime_traces,
            units,
            contexts,
            builds,
            files,
            provenance,
            ordinal,
            scopes,
            observed_units,
            observed_provenance,
            working_tree,
            orderings,
            metadata
        );
    }
    let mut source_snapshot = None;
    for (id, v) in all.metadata {
        match id.as_str() {
            "analysis_request" => s.analysis_request = serde_json::from_value(v)?,
            "production" => s.production = Some(serde_json::from_value(v)?),
            "source_snapshot" => source_snapshot = Some(v),
            _ => return Err(Error::new("UnknownFactFamily", format!("metadata:{id}"))),
        }
    }
    if let Some(v) = source_snapshot {
        s.sources = serde_json::from_value(v)?;
    }
    s.sources.files = all.files;
    s.entities = all.entities;
    s.relations = all.relations;
    s.occurrences = all.occurrences;
    s.bodies = all.bodies;
    s.documents = all.documents;
    s.runtime_traces = all.runtime_traces;
    s.units = all.units;
    s.contexts = all.contexts;
    s.builds = all.builds;
    s.provenance_records = all.provenance.into_iter().collect();
    s.working_tree_manifest = all.working_tree.into_iter().collect();
    if let Some(p) = &mut s.production {
        all.scopes.sort_by(|a, b| a.key.cmp(&b.key));
        p.scopes = all.scopes;
        all.observed_units.sort_by_key(|(i, _)| *i);
        p.unit_observations = all.observed_units.into_iter().map(|(_, v)| v).collect();
        all.observed_provenance.sort_by_key(|(i, _)| *i);
        p.provenance_observations = all
            .observed_provenance
            .into_iter()
            .map(|(_, v)| v)
            .collect();
    }
    // Metadata indexed by ordinal keeps its original order, not trie order.
    all.ordinal.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let mut ordinal: HashMap<String, Vec<Value>> = HashMap::new();
    for (family, _, v) in all.ordinal {
        ordinal.entry(family).or_default().push(v);
    }
    macro_rules! ordered {
        ($($field:ident),*) => {$(
            s.$field = ordinal
                .remove(stringify!($field))
                .unwrap_or_default()
                .into_iter()
                .map(serde_json::from_value)
                .collect::<std::result::Result<_, _>>()?;
        )*};
    }
    ordered!(
        providers,
        coverage,
        gaps,
        analysis_inputs,
        analysis_directories,
        build_dependencies
    );
    s.entities.sort_by(|a, b| a.id.cmp(&b.id));
    s.relations.sort_by(|a, b| a.id.cmp(&b.id));
    s.occurrences.sort_by(|a, b| a.id.cmp(&b.id));
    s.bodies.sort_by(|a, b| a.id.cmp(&b.id));
    s.documents.sort_by(|a, b| a.id.cmp(&b.id));
    s.sources
        .files
        .sort_by(|a, b| a.logical_path.cmp(&b.logical_path));
    let orderings: HashMap<String, Vec<String>> = all.orderings.into_iter().collect();
    macro_rules! reorder {
        ($field:ident, $family:literal) => {
            if let Some(ids) = orderings.get($family) {
                let positions: HashMap<&str, usize> = ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (id.as_str(), i))
                    .collect();
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

fn export(store: &Path, project: &str, revision: &str, out: &Path) -> Result<()> {
    let started = Instant::now();
    let objects = Objects::new(store.join("format-2/objects"))?;
    let manifest = store
        .join("format-2/projects")
        .join(digest(project.as_bytes()))
        .join("commits")
        .join(revision);
    let r: writer::Revision = serde_json::from_reader(BufReader::new(File::open(manifest)?))?;
    r.verify()?;
    let s = export_grouped(&objects, &r)?;
    log("export.assemble", started, counts(&s));
    let write = Instant::now();
    let tmp = out.with_extension("partial");
    let mut w = BufWriter::with_capacity(8 << 20, File::create(&tmp)?);
    serde_json::to_writer(&mut w, &s)?;
    w.flush()?;
    w.get_ref().sync_all()?;
    std::fs::rename(&tmp, out)?;
    log(
        "export.write",
        write,
        json!({"bytes":std::fs::metadata(out)?.len(),"total_seconds":started.elapsed().as_secs_f64()}),
    );
    Ok(())
}

fn remap_id(id: &mut String, map: &HashMap<String, String>) {
    if let Some(c) = map.get(id) {
        *id = c.clone();
    }
}

fn remap_resolution(r: &mut Resolution, map: &HashMap<String, String>) {
    match r {
        Resolution::Resolved(id) => remap_id(id, map),
        Resolution::Candidates(v) => {
            for id in &mut *v {
                remap_id(id, map);
            }
            v.sort();
            v.dedup();
        }
        _ => {}
    }
}

/// Native providers observe one definition once per compilation unit, so the
/// snapshot holds several entities for the same source extent. Collapse those
/// copies onto the first id and union their edges; otherwise `callers` returns a
/// different subset per copy while claiming to be exact, and cross-crate walks
/// stop at the first hop. Copies are recognized by identical file revision,
/// byte extent, kind and name. External and sourceless entities keep their
/// per-unit identity. Production scope descriptors still describe the original
/// units; per-context filtering is not part of this experiment.
fn merge_copies(s: &mut SemanticSnapshot) -> Value {
    let started = Instant::now();
    let before = counts(s);
    let mut remap: HashMap<String, String> = HashMap::new();

    type Key = (String, u64, u64, EntityKind, String);
    let mut canonical: HashMap<Key, usize> = HashMap::new();
    let mut entities: Vec<Entity> = Vec::with_capacity(s.entities.len());
    for e in std::mem::take(&mut s.entities) {
        let key = match (&e.source, e.external) {
            (Some(a), false) => Some((
                a.file_revision.clone(),
                a.start,
                a.end,
                e.kind,
                e.name.clone(),
            )),
            _ => None,
        };
        let Some(key) = key else {
            entities.push(e);
            continue;
        };
        match canonical.get(&key) {
            Some(&i) => {
                remap.insert(e.id.clone(), entities[i].id.clone());
                let keep = &mut entities[i];
                keep.exported |= e.exported;
                keep.type_definitions.extend(e.type_definitions);
                keep.type_navigation.extend(e.type_navigation);
                if keep.signature.is_empty() {
                    keep.signature = e.signature;
                }
            }
            None => {
                canonical.insert(key, entities.len());
                entities.push(e);
            }
        }
    }
    drop(canonical);
    let entity_copies = remap.len();
    for e in &mut entities {
        if let Some(p) = &mut e.parent {
            remap_id(p, &remap);
        }
        for t in &mut e.type_definitions {
            remap_id(t, &remap);
        }
        e.type_definitions.sort();
        e.type_definitions.dedup();
        for n in &mut e.type_navigation {
            for c in &mut n.candidates {
                remap_id(c, &remap);
            }
        }
    }
    s.entities = entities;

    let mut bodies: Vec<Body> = Vec::with_capacity(s.bodies.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for mut b in std::mem::take(&mut s.bodies) {
        remap_id(&mut b.owner, &remap);
        if let Some(p) = &mut b.parent {
            remap_id(p, &remap);
        }
        let key = digest(
            format!(
                "{}:{}:{}:{:?}:{}:{:?}:{:?}",
                b.source.file_revision,
                b.source.start,
                b.source.end,
                b.kind,
                b.owner,
                b.parent,
                b.evaluation
            )
            .as_bytes(),
        );
        match seen.get(&key) {
            Some(&i) => {
                remap.insert(b.id.clone(), bodies[i].id.clone());
            }
            None => {
                seen.insert(key, bodies.len());
                bodies.push(b);
            }
        }
    }
    drop(seen);
    s.bodies = bodies;

    let mut occurrences: Vec<Occurrence> = Vec::with_capacity(s.occurrences.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for mut o in std::mem::take(&mut s.occurrences) {
        if let Some(owner) = &mut o.owner {
            remap_id(owner, &remap);
        }
        remap_resolution(&mut o.declaration, &remap);
        remap_resolution(&mut o.executable, &remap);
        for b in &mut o.argument_bindings {
            remap_resolution(&mut b.parameter, &remap);
        }
        let key = digest(
            format!(
                "{}:{}:{}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
                o.source.file_revision,
                o.source.start,
                o.source.end,
                o.kind,
                o.owner,
                o.declaration,
                o.executable,
                o.dispatch,
                o.evaluation
            )
            .as_bytes(),
        );
        match seen.get(&key) {
            Some(&i) => {
                remap.insert(o.id.clone(), occurrences[i].id.clone());
            }
            None => {
                seen.insert(key, occurrences.len());
                occurrences.push(o);
            }
        }
    }
    drop(seen);
    s.occurrences = occurrences;

    let mut relations: Vec<Relation> = Vec::with_capacity(s.relations.len());
    let mut seen: HashSet<String> = HashSet::new();
    for mut r in std::mem::take(&mut s.relations) {
        remap_id(&mut r.from, &remap);
        remap_id(&mut r.to, &remap);
        if let Some(o) = &mut r.occurrence {
            remap_id(o, &remap);
        }
        let key = digest(
            format!(
                "{}:{}:{:?}:{:?}:{:?}:{:?}",
                r.from, r.to, r.kind, r.occurrence, r.source, r.provenance
            )
            .as_bytes(),
        );
        if seen.insert(key) {
            relations.push(r);
        }
    }
    drop(seen);
    s.relations = relations;

    let mut documents: Vec<DocumentBlock> = Vec::with_capacity(s.documents.len());
    let mut seen: HashSet<String> = HashSet::new();
    for mut d in std::mem::take(&mut s.documents) {
        if let Some(o) = &mut d.owner {
            remap_id(o, &remap);
        }
        if let Some(p) = &mut d.parent {
            remap_id(p, &remap);
        }
        for l in &mut d.links {
            remap_resolution(&mut l.target, &remap);
        }
        if seen.insert(format!("{}:{:?}", d.id, d.owner)) {
            documents.push(d);
        }
    }
    s.documents = documents;

    log(
        "load.merge_copies",
        started,
        json!({"before":before,"after":counts(s),"entity_copies":entity_copies,"remapped_ids":remap.len()}),
    );
    json!({"entity_copies":entity_copies})
}

const IN_MEMORY_MAX_WORK: u64 = 20_000_000;

struct Server {
    snapshot: &'static SemanticSnapshot,
    engine: QueryEngine<'static>,
    cursors: Mutex<HashMap<String, QueryContinuation>>,
    next_cursor: AtomicU64,
}

#[path = "memory_query/replay.rs"]
mod replay;

fn decode(snapshot: &Path, started: Instant) -> Result<SemanticSnapshot> {
    let s = serde_json::from_reader(BufReader::with_capacity(8 << 20, File::open(snapshot)?))?;
    log("load.decode", started, counts(&s));
    Ok(s)
}

fn load(snapshot: &Path, blobs: &Path) -> Result<Server> {
    let started = Instant::now();
    index_snapshot(
        decode(snapshot, started)?,
        BlobStore::new(blobs.into()),
        started,
    )
}

fn index_snapshot(
    mut s: SemanticSnapshot,
    content: impl ContentReader + 'static,
    started: Instant,
) -> Result<Server> {
    merge_copies(&mut s);
    let index = Instant::now();
    let s: &'static SemanticSnapshot = Box::leak(Box::new(s));
    let content: &'static dyn ContentReader = Box::leak(Box::new(content));
    let search = Arc::new(SearchIndex::new(&s.entities));
    let engine = QueryEngine::with_search(s, content, search);
    log(
        "load.index",
        index,
        json!({"total_seconds":started.elapsed().as_secs_f64()}),
    );
    Ok(Server {
        snapshot: s,
        engine,
        cursors: Mutex::new(HashMap::new()),
        next_cursor: AtomicU64::new(1),
    })
}

/// Tool names and descriptions follow `cq mcp`; only read-only query tools are kept.
const TOOLS: &[(&str, &str)] = &[
    (
        "search_graph",
        "Search symbols by name or text; exact names precede bounded lexical windows. Results retain build context. Args: text, kind (e.g. Function, Method, Struct, Trait), file, limit.",
    ),
    (
        "query_codequest",
        "Unified query: definition, type_definition, references, callers, callees, implementations, details, members, bodies, topology, build_configuration, documents, source, outline, docs, trace, path and next. Pass operation plus subject (exact entity id preferred; ambiguous names return candidates). Every continue_with object is accepted here.",
    ),
    (
        "trace_path",
        "Explore bounded static call paths from subject (direction outbound/inbound, depth); with target, find a path. Deferred and unresolved calls are not runtime facts.",
    ),
    (
        "get_code_snippet",
        "Read indexed source of an entity (subject) or a file range (file, line, max_lines).",
    ),
    ("get_file_outline", "Declarations in a source file (file)."),
    (
        "documents",
        "Search or page repository documents and rustdoc with exact evidence (text, subject, document_kind).",
    ),
    (
        "get_architecture",
        "Overview, topology, hotspots, cycles or clusters (operation).",
    ),
];

fn schema() -> Value {
    let mut properties = json!({"operation":{"type":"string"},"subject":{"type":"string","description":"Exact entity id preferred; ambiguous names return candidates"},"target":{"type":"string"},"text":{"type":"string"},"context":{"type":"string"},"file":{"type":"string"},"kind":{"type":"string"},"relation":{"type":"string"},"direction":{"enum":["outbound","inbound","both"]},"depth":{"type":"integer","minimum":0,"maximum":4096},"limit":{"type":"integer","minimum":1,"maximum":1000},"max_bytes":{"type":"integer","minimum":128},"cursor":{"type":"string"},"line":{"type":"integer","minimum":1},"column":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1},"include_candidates":{"type":"boolean"},"include_external":{"type":"boolean"},"document_kind":{"type":"string"},"compact":{"type":"boolean","description":"Compact output with relative file:line and owner names (default: true). Pass false for full provenance and byte spans."}});
    properties["mode"] = json!({"enum":["calls","data_flow","types"]});
    json!({"type":"object","properties":properties,"additionalProperties":true})
}

impl Server {
    fn operation(&self, tool: &str, a: &Value) -> Result<String> {
        Ok(match tool {
            "search_graph" => "search".into(),
            "query_codequest" | "documents" | "get_architecture" => a["operation"]
                .as_str()
                .unwrap_or(match tool {
                    "documents" => "docs",
                    _ => "overview",
                })
                .into(),
            "trace_path" if a["operation"].as_str() == Some("impact") => "impact".into(),
            "trace_path" if a["target"].is_string() => "path".into(),
            "trace_path" => "trace".into(),
            "get_code_snippet" => "source".into(),
            "get_file_outline" => "outline".into(),
            _ => return Err(Error::new("UnknownTool", tool)),
        })
    }
    fn call(&self, tool: &str, mut a: Value) -> Result<Value> {
        if !a.is_object() {
            a = json!({});
        }
        if a.get("text").is_none()
            && let Some(text) = a.get("query").cloned()
        {
            a["text"] = text;
        }
        // Work budgets were sized for paged disk reads; an in-memory scan of the
        // whole entity table is cheap, so the default lets a search finish in one call.
        if a.get("max_work").is_none() {
            a["max_work"] = json!(IN_MEMORY_MAX_WORK);
        }
        if a.get("compact").is_none() {
            a["compact"] = json!(true);
        }
        let operation = self.operation(tool, &a)?;
        let (q, resume) = if operation == "next" {
            let token = a["cursor"]
                .as_str()
                .ok_or_else(|| Error::new("MissingCursor", "next needs cursor"))?;
            let state = self
                .cursors
                .lock()
                .unwrap()
                .get(token)
                .cloned()
                .ok_or_else(|| Error::new("UnknownCursor", token))?;
            let mut request = serde_json::to_value(&state.request)?;
            for key in ["limit", "max_bytes", "max_work", "timeout_ms"] {
                if let Some(value) = a.get(key) {
                    request[key] = value.clone();
                }
            }
            (
                serde_json::from_value::<QueryRequest>(request)?,
                Some(state),
            )
        } else {
            a["operation"] = json!(operation);
            a.as_object_mut().unwrap().remove("project");
            a.as_object_mut().unwrap().remove("revision");
            (serde_json::from_value::<QueryRequest>(a)?, None)
        };
        let result = self.engine.execute(&q, resume)?;
        let mut page = result.page;
        if let Some(c) = result.continuation {
            let id = format!("m{}", self.next_cursor.fetch_add(1, Ordering::Relaxed));
            self.cursors.lock().unwrap().insert(id.clone(), c);
            page.next_cursor = Some(id.clone());
            for hidden in &mut page.hidden {
                if hidden.category == "remaining_query_results" {
                    hidden.continue_with = json!({"operation":"next","cursor":id});
                }
            }
        }
        let mut value = if q.compact {
            page.to_compact_json()
        } else {
            json!(page)
        };
        if !q.compact {
            value["execution_path"] = json!("in_memory_snapshot");
        }
        Ok(value)
    }
    fn rpc(&self, request: &Value) -> Result<Value> {
        let params = &request["params"];
        match request["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","serverInfo":{"name":"CodeQuest-memory","version":env!("CARGO_PKG_VERSION")},"capabilities":{"tools":{"listChanged":false}}}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(
                json!({"tools":TOOLS.iter().map(|(name,description)| json!({"name":name,"description":format!("{description} Project: {}.",self.snapshot.project),"inputSchema":schema()})).collect::<Vec<_>>()}),
            ),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let started = Instant::now();
                let outcome = self.call(name, params["arguments"].clone());
                eprintln!(
                    "{}",
                    json!({"tool":name,"arguments":params["arguments"],"ms":started.elapsed().as_millis() as u64,"ok":outcome.is_ok()})
                );
                Ok(match outcome {
                    Ok(v) => {
                        json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false})
                    }
                    Err(e) => {
                        json!({"content":[{"type":"text","text":json!({"error":e.to_string()}).to_string()}],"isError":true})
                    }
                })
            }
            m => Err(Error::new("MethodNotFound", m)),
        }
    }
}

/// One resident process serves many MCP sessions over a unix socket, so the
/// snapshot is loaded once; `bridge` connects a session's stdio to it.
fn listen(server: &'static Server, socket: &Path) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = std::os::unix::net::UnixListener::bind(socket)?;
    eprintln!("{}", json!({"ready":true,"socket":socket}));
    for stream in listener.incoming() {
        let stream = stream?;
        std::thread::spawn(move || {
            let input = BufReader::new(stream.try_clone().expect("clone socket"));
            if let Err(e) = serve_stream(server, input, stream) {
                eprintln!("{}", json!({"session_error":e.to_string()}));
            }
        });
    }
    Ok(())
}

fn pump_stream(mut reader: impl std::io::Read, mut writer: impl Write) -> std::io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        writer.write_all(&buf[..n])?;
        writer.flush()?;
    }
}

fn bridge(socket: &Path) -> Result<()> {
    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    let reader = stream.try_clone()?;
    let pump = std::thread::spawn(move || {
        let _ = pump_stream(reader, std::io::stdout().lock());
    });
    let writer = stream.try_clone()?;
    pump_stream(std::io::stdin().lock(), writer)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let _ = pump.join();
    Ok(())
}

fn serve(server: &Server) -> Result<()> {
    serve_stream(server, std::io::stdin().lock(), std::io::stdout().lock())
}

fn serve_stream(server: &Server, input: impl BufRead, mut out: impl Write) -> Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}})
                )?;
                out.flush()?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = match server.rpc(&request) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(e) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":e.to_string()}})
            }
        };
        writeln!(out, "{response}")?;
        out.flush()?;
    }
    Ok(())
}

/// Each input line: {"name": "...", "tool": "...", "arguments": {...}}.
fn bench(server: &Server, calls: &Path, output: &Path) -> Result<()> {
    let mut w = BufWriter::new(File::create(output)?);
    for line in BufReader::new(File::open(calls)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let c: Value = serde_json::from_str(&line)?;
        let started = Instant::now();
        let outcome = server.call(
            c["tool"].as_str().unwrap_or_default(),
            c["arguments"].clone(),
        );
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let record = match outcome {
            Ok(v) => {
                json!({"name":c["name"],"ms":ms,"ok":true,"state":v["state"],"items":v["items"].as_array().map(Vec::len),"total":v["total"],"truncated":v["truncated"],"result":v})
            }
            Err(e) => json!({"name":c["name"],"ms":ms,"ok":false,"error":e.to_string()}),
        };
        eprintln!(
            "{:>10.1} ms  {:<28} {}",
            ms,
            c["name"].as_str().unwrap_or_default(),
            if record["ok"] == true {
                format!(
                    "{} items={}",
                    record["state"].as_str().unwrap_or_default(),
                    record["items"]
                )
            } else {
                record["error"].as_str().unwrap_or_default().to_owned()
            }
        );
        writeln!(w, "{record}")?;
    }
    w.flush()?;
    log("bench.done", Instant::now(), json!({}));
    Ok(())
}

#[derive(Default)]
struct MemoryContent(Mutex<HashMap<String, Vec<u8>>>);

impl ContentReader for MemoryContent {
    fn read_blob(&self, id: &BlobId) -> Result<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .get(&id.digest)
            .cloned()
            .ok_or_else(|| Error::new("SourceUnavailable", "blob not in memory content store"))
    }
}

impl ContentRepository for MemoryContent {
    fn freeze(&self, path: &Path, origin: &str, budget: u64) -> Result<FileRevision> {
        let bytes = std::fs::read(path)?;
        if bytes.len() as u64 > budget {
            return Err(Error::new("ContentLimit", path.display().to_string()));
        }
        let blob = self.put(&bytes, budget)?;
        let logical_path = NativeValue::new(path);
        let id = digest(&serde_json::to_vec(&(&logical_path, &blob))?);
        Ok(FileRevision {
            id,
            logical_path,
            blob,
            origin: origin.into(),
        })
    }
    fn put(&self, bytes: &[u8], budget: u64) -> Result<BlobId> {
        if bytes.len() as u64 > budget {
            return Err(Error::new("ContentLimit", "blob exceeds budget"));
        }
        let blob = BlobId::of(bytes);
        self.0
            .lock()
            .unwrap()
            .entry(blob.digest.clone())
            .or_insert_with(|| bytes.to_vec());
        Ok(blob)
    }
}

fn find_ts_provider() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("CQ_TS_COMPILER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(6) {
            candidates.push(ancestor.join("providers/ts-compiler/index.mjs"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("providers/ts-compiler/index.mjs"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../providers/ts-compiler/index.mjs"),
    );
    for c in candidates {
        if c.is_file() {
            return Ok(c.canonicalize().unwrap_or(c));
        }
    }
    Err(Error::new(
        "MissingProvider",
        "cannot locate providers/ts-compiler/index.mjs; set CQ_TS_COMPILER",
    ))
}

fn augment_ts(
    base_snapshot: &Path,
    disk_root: &Path,
    out: &Path,
    ts_module: Option<&Path>,
) -> Result<()> {
    let started = Instant::now();
    let mut snapshot = decode(base_snapshot, started)?;
    let logical_root = snapshot.root.path();
    let provider = find_ts_provider()?;
    let content = MemoryContent::default();
    let stats = cq_core::analyst::ts::analyze_in_memory(
        disk_root,
        &logical_root,
        &provider,
        ts_module,
        &mut snapshot,
        &content,
    )?;
    log("augment_ts.normalized", started, json!(stats));
    let mut writer = BufWriter::with_capacity(8 << 20, File::create(out)?);
    serde_json::to_writer(&mut writer, &snapshot)?;
    writer.flush()?;
    log(
        "augment_ts.written",
        started,
        json!({"out": out, "counts": counts(&snapshot)}),
    );
    Ok(())
}

fn index_ts(disk_root: &Path, out: &Path, ts_module: Option<&Path>) -> Result<()> {
    let started = Instant::now();
    let disk_root = disk_root.canonicalize()?;
    let project = disk_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ts-project".into());
    let mut snapshot = SemanticSnapshot::empty(project, NativeValue::new(&disk_root));
    snapshot.revision = digest(&serde_json::to_vec(&disk_root.to_string_lossy())?);
    let provider = find_ts_provider()?;
    let content = MemoryContent::default();
    let stats = cq_core::analyst::ts::analyze_in_memory(
        &disk_root,
        &disk_root,
        &provider,
        ts_module,
        &mut snapshot,
        &content,
    )?;
    log("index_ts.normalized", started, json!(stats));
    let mut writer = BufWriter::with_capacity(8 << 20, File::create(out)?);
    serde_json::to_writer(&mut writer, &snapshot)?;
    writer.flush()?;
    log(
        "index_ts.written",
        started,
        json!({"out": out, "counts": counts(&snapshot)}),
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = |i: usize| -> Result<PathBuf> {
        args.get(i)
            .map(PathBuf::from)
            .ok_or_else(|| Error::new("Usage", "missing argument"))
    };
    match args.first().map(String::as_str) {
        Some("export") => export(
            &path(1)?,
            args.get(2).ok_or_else(|| Error::new("Usage", "project"))?,
            args.get(3).ok_or_else(|| Error::new("Usage", "revision"))?,
            &path(4)?,
        ),
        Some("augment-ts") => augment_ts(
            &path(1)?,
            &path(2)?,
            &path(3)?,
            args.get(4).map(PathBuf::from).as_deref(),
        ),
        Some("index-ts") => index_ts(
            &path(1)?,
            &path(2)?,
            args.get(3).map(PathBuf::from).as_deref(),
        ),
        Some("serve") => serve(&load(&path(1)?, &path(2)?)?),
        Some("listen") => listen(Box::leak(Box::new(load(&path(1)?, &path(2)?)?)), &path(3)?),
        Some("listen-replay") => replay::listen_verified(&path(1)?, &path(2)?, &path(3)?),
        Some("bench-replay") => replay::bench_verified(&path(1)?, &path(2)?, &path(3)?, &path(4)?),
        Some("bridge") => bridge(&path(1)?),
        Some("bench") => bench(&load(&path(1)?, &path(2)?)?, &path(3)?, &path(4)?),
        _ => Err(Error::new(
            "Usage",
            "memory_query export|augment-ts|index-ts|serve|listen|listen-replay|bench-replay|bridge|bench ...",
        )),
    }
}
