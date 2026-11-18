//! Normalize a provider export by source ownership, closing each frame over
//! native reference targets, lexical parents and formal parameters. Foreign
//! declarations are resolution inputs, not competing partial fact publications.
use crate::{
    Error, Result,
    model::*,
    ports::ContentRepository,
    semantic::records::{self, Store},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
fn rows<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn span(v: &Value) -> Option<(u64, u64, u64)> {
    Some((
        v["file"].as_u64()?,
        v["start"].as_u64()?,
        v["end"].as_u64()?,
    ))
}
fn source_file(v: &Value) -> Option<u64> {
    v["file"].as_u64()
}
fn file_refs(v: &Value, out: &mut BTreeSet<u64>) {
    match v {
        Value::Object(m) => {
            if let Some(f) = v["file"].as_u64() {
                out.insert(f);
            }
            if let Some(f) = v["to_file"].as_u64() {
                out.insert(f);
            }
            for v in m.values() {
                file_refs(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                file_refs(v, out);
            }
        }
        _ => {}
    }
}
#[allow(clippy::too_many_arguments)]
pub fn normalize(
    s: &mut SemanticSnapshot,
    raw: &Value,
    mapping: &[MaterializedInput],
    content: &dyn ContentRepository,
    provider: &str,
    language: Language,
    version: &str,
    sink: &dyn Store,
) -> Result<()> {
    super::provider_contract::validate(raw)?;
    if !s.entities.is_empty()
        || !s.relations.is_empty()
        || !s.occurrences.is_empty()
        || !s.bodies.is_empty()
        || !s.documents.is_empty()
    {
        return Err(Error::new(
            "NonStreamingInput",
            "provider frame must not accumulate a project snapshot",
        ));
    }
    let mut primary = rows(raw, "files")
        .iter()
        .map(|f| f["id"].as_u64())
        .collect::<Vec<_>>();
    primary.push(None);
    let mut peak = 0;
    let mut gap_keys = HashSet::new();
    let mut first = true;
    let catalogue = Catalogue::new(raw);
    let mut source_state = FrameSources::new(s, mapping);
    let result: Result<()> = (|| {
        for file in primary {
            let frame = catalogue.frame(file)?;
            emit_frame(
                s,
                &frame,
                mapping,
                content,
                provider,
                language,
                version,
                sink,
                file,
                &mut peak,
                &mut first,
                &mut gap_keys,
                &mut source_state,
            )?;
        }
        Ok(())
    })();
    source_state.finish(s);
    result?;
    record_transport(s, peak, rows(raw, "files").len() + 1);

    Ok(())
}
pub(super) fn record_transport(s: &mut SemanticSnapshot, peak: usize, frames: usize) {
    if let Some(p) = s.production.as_mut() {
        let maximum = p
            .transport
            .get("maximum_file_frame_records")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .max(peak as u64);
        let count = p
            .transport
            .get("native_file_frames")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + frames as u64;
        let exports = p
            .transport
            .get("native_exports")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        p.transport
            .insert("maximum_file_frame_records".into(), json!(maximum));
        p.transport
            .insert("native_file_frames".into(), json!(count));
        p.transport.insert("native_exports".into(), json!(exports));
    }
}
/// Source metadata is merged once after an export, rather than cloned and
/// rehashed in full for every source frame.
pub(super) struct FrameSources<'a> {
    known: BTreeMap<NativeValue, FileRevision>,
    mapping: &'a [MaterializedInput],
    exact: HashMap<(&'a str, &'a str), usize>,
    by_path: HashMap<&'a str, usize>,
    binding: InputBinding,
    gaps: Vec<Gap>,
}
impl<'a> FrameSources<'a> {
    pub fn new(s: &SemanticSnapshot, mapping: &'a [MaterializedInput]) -> Self {
        Self {
            known: s
                .sources
                .files
                .iter()
                .chain(mapping.iter().map(|m| &m.file))
                .map(|f| (f.logical_path.clone(), f.clone()))
                .collect(),
            mapping,
            exact: mapping
                .iter()
                .enumerate()
                .map(|(i, m)| ((m.path.as_str(), m.unit.as_str()), i))
                .collect(),
            by_path: mapping
                .iter()
                .enumerate()
                .map(|(i, m)| (m.path.as_str(), i))
                .collect(),
            binding: s.sources.binding.clone(),
            gaps: s.sources.gaps.clone(),
        }
    }
    fn fragment(
        &self,
        s: &SemanticSnapshot,
        frame: &Value,
    ) -> (SemanticSnapshot, Vec<MaterializedInput>) {
        let mut local = BTreeMap::new();
        let mut selected = vec![];
        for file in rows(frame, "files") {
            let path = file["path"].as_str().unwrap();
            let i = if let Some(unit) = file["unit"].as_str() {
                self.exact.get(&(path, unit))
            } else {
                self.by_path.get(path)
            };
            if let Some(i) = i {
                let m = &self.mapping[*i];
                local.insert(m.file.logical_path.clone(), m.file.clone());
                selected.push(m.clone());
            } else if let Some(f) = self.known.get(&NativeValue::from(path)) {
                local.insert(f.logical_path.clone(), f.clone());
            }
        }
        let mut fragment = SemanticSnapshot::empty(s.project.clone(), s.root.clone());
        fragment.builds.extend(s.builds.last().cloned());
        fragment.providers = s.providers.clone();
        fragment.provenance_records = s.provenance_records.clone();
        fragment.sources =
            SourceSnapshot::seal(local.into_values().collect(), self.binding.clone(), vec![]);
        (fragment, selected)
    }
    pub(super) fn merge(&mut self, files: &[FileRevision]) -> Result<()> {
        for f in files {
            if self
                .known
                .get(&f.logical_path)
                .is_some_and(|old| old.blob != f.blob)
            {
                return Err(Error::new(
                    "ProducerInputMismatch",
                    f.logical_path.display(),
                ));
            }
            self.known.insert(f.logical_path.clone(), f.clone());
        }
        Ok(())
    }
    pub fn finish(self, s: &mut SemanticSnapshot) {
        s.sources =
            SourceSnapshot::seal(self.known.into_values().collect(), self.binding, self.gaps);
    }
}
type Span = (u64, u64, u64);
/// Planning retains row numbers only. JSON payloads are materialized after
/// ordered memory admission, not while other source plans are still running.
pub(super) struct FramePlan {
    rows: BTreeMap<&'static str, Vec<usize>>,
}
/// Built once per native export; indexes contain row offsets and identities,
/// never another copy of provider payloads.
pub(super) struct Catalogue<'a> {
    raw: &'a Value,
    tokens: HashMap<u64, usize>,
    keys: HashMap<&'a str, Vec<u64>>,
    spans: BTreeMap<Span, Vec<u64>>,
    owned: HashMap<Option<u64>, Vec<u64>>,
    file_rows: BTreeMap<u64, usize>,
    by_file: HashMap<&'static str, HashMap<u64, Vec<usize>>>,
    by_child: HashMap<&'static str, HashMap<u64, Vec<usize>>>,
    parameters: BTreeMap<Span, Vec<usize>>,
}
impl<'a> Catalogue<'a> {
    pub fn new(raw: &'a Value) -> Self {
        let mut c = Self {
            raw,
            tokens: HashMap::new(),
            keys: HashMap::new(),
            spans: BTreeMap::new(),
            owned: HashMap::new(),
            file_rows: BTreeMap::new(),
            by_file: HashMap::new(),
            by_child: HashMap::new(),
            parameters: BTreeMap::new(),
        };
        for (i, v) in rows(raw, "symbols").iter().enumerate() {
            let t = v["token"].as_u64().unwrap();
            c.tokens.insert(t, i);
            c.keys
                .entry(v["key"].as_str().unwrap())
                .or_default()
                .push(t);
            if let Some(p) = span(&v["definition"]) {
                c.spans.entry(p).or_default().push(t);
            }
            let owner = source_file(&v["definition"]);
            c.owned.entry(owner).or_default().push(t);
            if let Some(f) = source_file(&v["documentation_source"]).filter(|f| Some(*f) != owner) {
                c.owned.entry(Some(f)).or_default().push(t);
            }
        }
        for (i, v) in rows(raw, "files").iter().enumerate() {
            c.file_rows.insert(v["id"].as_u64().unwrap(), i);
        }
        for (name, field) in [
            ("references", "source"),
            ("relations", ""),
            ("doc_links", "source"),
            ("calls", "callee"),
            ("bodies", "source"),
            ("comments", "source"),
            ("scoped_documentation", "source"),
            ("lexical_parents", "child"),
        ] {
            let mut index = HashMap::<u64, Vec<usize>>::new();
            for (i, v) in rows(raw, name).iter().enumerate() {
                if let Some(f) = source_file(if field.is_empty() { v } else { &v[field] }) {
                    index.entry(f).or_default().push(i);
                }
            }
            c.by_file.insert(name, index);
        }
        for name in ["parents", "nominal_members"] {
            let mut index = HashMap::<u64, Vec<usize>>::new();
            for (i, v) in rows(raw, name).iter().enumerate() {
                if let Some(child) = v["child"].as_u64() {
                    index.entry(child).or_default().push(i);
                }
            }
            c.by_child.insert(name, index);
        }
        for (i, v) in rows(raw, "parameters").iter().enumerate() {
            if let Some(p) = span(&v["owner"]) {
                c.parameters.entry(p).or_default().push(i);
            }
        }
        c
    }
    fn file_indices(&self, name: &str, file: Option<u64>) -> &[usize] {
        file.and_then(|f| self.by_file.get(name)?.get(&f))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn child_indices(&self, name: &str, token: u64) -> &[usize] {
        self.by_child
            .get(name)
            .and_then(|m| m.get(&token))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn frame(&self, file: Option<u64>) -> Result<Value> {
        self.render_with(&self.plan(file)?, |row| Ok(row.clone()))
    }
    pub fn plan_rows<'b>(&'b self, plan: &'b FramePlan) -> impl Iterator<Item = &'b Value> {
        plan.rows
            .iter()
            .flat_map(move |(name, indices)| indices.iter().map(move |i| &rows(self.raw, name)[*i]))
    }
    pub fn render_with(
        &self,
        plan: &FramePlan,
        mut hydrate: impl FnMut(&Value) -> Result<Value>,
    ) -> Result<Value> {
        let mut frame = self
            .raw
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| !v.is_array())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>();
        let calls = &plan.rows["calls"];
        for (name, indices) in &plan.rows {
            let mut values = Vec::with_capacity(indices.len());
            for i in indices {
                let mut row = hydrate(&rows(self.raw, name)[*i])?;
                if *name == "references"
                    && let Some(old) = row["call_index"].as_u64()
                {
                    row["call_index"] = calls
                        .binary_search(&(old as usize))
                        .ok()
                        .map_or(Value::Null, |i| json!(i));
                }
                values.push(row);
            }
            frame.insert((*name).into(), Value::Array(values));
        }
        Ok(Value::Object(frame))
    }
    pub fn plan(&self, file: Option<u64>) -> Result<FramePlan> {
        use std::collections::VecDeque;
        let raw = self.raw;
        let mut selected = BTreeSet::<u64>::new();
        let mut pending = VecDeque::new();
        fn add(t: u64, selected: &mut BTreeSet<u64>, pending: &mut VecDeque<u64>) {
            if selected.insert(t) {
                pending.push_back(t);
            }
        }
        let add_span = |p: Span, selected: &mut BTreeSet<u64>, pending: &mut VecDeque<u64>| {
            for t in self.spans.get(&p).into_iter().flatten() {
                add(*t, selected, pending);
            }
        };
        for t in self.owned.get(&file).into_iter().flatten() {
            add(*t, &mut selected, &mut pending);
        }
        let references = self.file_indices("references", file);
        for i in references {
            if let Some(t) = rows(raw, "references")[*i]["target"].as_u64() {
                add(t, &mut selected, &mut pending);
            }
        }
        for i in self.file_indices("relations", file) {
            for field in ["from", "to"] {
                if let Some(k) = rows(raw, "relations")[*i][field].as_str() {
                    for t in self.keys.get(k).into_iter().flatten() {
                        add(*t, &mut selected, &mut pending);
                    }
                }
            }
        }
        for i in self.file_indices("doc_links", file) {
            for target in rows(&rows(raw, "doc_links")[*i], "targets") {
                if let Some(p) = span(target) {
                    add_span(p, &mut selected, &mut pending);
                }
            }
        }
        for name in ["scoped_documentation", "lexical_parents"] {
            for i in self.file_indices(name, file) {
                for field in ["owner", "child"] {
                    if let Some(p) = span(&rows(raw, name)[*i][field]) {
                        add_span(p, &mut selected, &mut pending);
                    }
                }
            }
        }
        while let Some(t) = pending.pop_front() {
            for i in self.child_indices("parents", t) {
                let p = &rows(raw, "parents")[*i];
                if let Some(parent) = p["parent_token"].as_u64() {
                    add(parent, &mut selected, &mut pending);
                } else if let Some(k) = p["key"].as_str() {
                    for parent in self.keys.get(k).into_iter().flatten() {
                        add(*parent, &mut selected, &mut pending);
                    }
                }
            }
            for i in self.child_indices("nominal_members", t) {
                if let Some(owner) = rows(raw, "nominal_members")[*i]["owner"].as_u64() {
                    add(owner, &mut selected, &mut pending);
                }
            }
            if let Some(symbol) = self.tokens.get(&t).map(|i| &rows(raw, "symbols")[*i]) {
                if let Some(p) = span(&symbol["definition"]) {
                    for i in self.parameters.get(&p).into_iter().flatten() {
                        for slot in rows(&rows(raw, "parameters")[*i], "slots") {
                            for b in rows(slot, "bindings") {
                                if let Some(p) = span(b) {
                                    add_span(p, &mut selected, &mut pending);
                                }
                            }
                        }
                    }
                }
                for nav in rows(symbol, "type_definitions") {
                    if let Some(p) = span(&nav["source"]) {
                        add_span(p, &mut selected, &mut pending);
                    }
                }
            }
        }
        let sorted = |indices: Vec<usize>| {
            indices
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        let mut plan = FramePlan {
            rows: BTreeMap::new(),
        };
        plan.rows.insert(
            "symbols",
            sorted(
                selected
                    .iter()
                    .filter_map(|t| self.tokens.get(t))
                    .copied()
                    .collect(),
            ),
        );
        let mut calls = self
            .file_indices("calls", file)
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for i in references {
            if let Some(c) = rows(raw, "references")[*i]["call_index"].as_u64() {
                calls.insert(c as usize);
            }
        }
        plan.rows.insert("calls", calls.into_iter().collect());
        plan.rows.insert("references", references.to_vec());
        for name in [
            "relations",
            "doc_links",
            "bodies",
            "comments",
            "scoped_documentation",
            "lexical_parents",
        ] {
            plan.rows
                .insert(name, self.file_indices(name, file).to_vec());
        }
        for name in ["parents", "nominal_members"] {
            plan.rows.insert(
                name,
                sorted(
                    selected
                        .iter()
                        .flat_map(|t| self.child_indices(name, *t).iter().copied())
                        .filter(|i| {
                            name != "nominal_members"
                                || rows(raw, name)[*i]["owner"]
                                    .as_u64()
                                    .is_some_and(|t| selected.contains(&t))
                        })
                        .collect(),
                ),
            );
        }
        plan.rows.insert(
            "parameters",
            sorted(
                selected
                    .iter()
                    .filter_map(|t| self.tokens.get(t))
                    .filter_map(|i| span(&rows(raw, "symbols")[*i]["definition"]))
                    .flat_map(|p| self.parameters.get(&p).into_iter().flatten().copied())
                    .collect(),
            ),
        );
        let mut used = BTreeSet::new();
        for (_, v) in raw
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| !v.is_array())
        {
            file_refs(v, &mut used);
        }
        for row in self.plan_rows(&plan) {
            file_refs(row, &mut used);
        }
        if let Some(f) = file {
            used.insert(f);
        }
        plan.rows.insert(
            "files",
            sorted(
                used.iter()
                    .filter_map(|f| self.file_rows.get(f))
                    .copied()
                    .collect(),
            ),
        );
        Ok(plan)
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_frame(
    s: &mut SemanticSnapshot,
    frame: &Value,
    mapping: &[MaterializedInput],
    content: &dyn ContentRepository,
    provider: &str,
    language: Language,
    version: &str,
    sink: &dyn Store,
    file: Option<u64>,
    peak: &mut usize,
    first: &mut bool,
    gap_keys: &mut HashSet<String>,
    source_state: &mut FrameSources,
) -> Result<()> {
    let output = prepare_frame(
        s,
        frame,
        mapping,
        content,
        provider,
        language,
        version,
        file,
        source_state,
    )?;
    *peak = (*peak).max(output.peak);
    merge_frame(s, output, sink, first, gap_keys, source_state)
}
pub(super) struct FrameOutput {
    pub fragment: SemanticSnapshot,
    pub peak: usize,
}
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_frame(
    s: &SemanticSnapshot,
    frame: &Value,
    mapping: &[MaterializedInput],
    content: &dyn ContentRepository,
    provider: &str,
    language: Language,
    version: &str,
    file: Option<u64>,
    source_state: &FrameSources,
) -> Result<FrameOutput> {
    let raw = frame;
    let (mut fragment, selected_mapping) = source_state.fragment(s, frame);
    super::normalize::native_graph(
        &mut fragment,
        frame,
        &selected_mapping,
        content,
        provider,
        language,
        version,
    )?;
    let peak = fragment.entities.len()
        + fragment.bodies.len()
        + fragment.occurrences.len()
        + fragment.relations.len()
        + fragment.documents.len();
    let primary_file = file.and_then(|id| rows(raw, "files").iter().find(|f| f["id"] == id));
    let wanted = primary_file.and_then(|f| {
        let path = f["path"].as_str()?;
        if let Some(input) = mapping
            .iter()
            .rev()
            .find(|m| m.path == path && f["unit"].as_str().is_none_or(|u| u == m.unit))
        {
            return Some((input.unit.clone(), input.file.id.clone()));
        }
        let unit = f["unit"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                mapping
                    .iter()
                    .find(|m| m.path == path)
                    .map(|m| m.unit.clone())
            })
            .unwrap_or_else(|| "external".into());
        fragment
            .entities
            .iter()
            .find(|e| e.kind == EntityKind::File && e.qualified_name == path && e.unit == unit)
            .and_then(|e| e.source.as_ref().map(|a| (unit, a.file_revision.clone())))
    });
    if file.is_some() && wanted.is_none() {
        return Err(Error::new(
            "InvalidSourceFrame",
            "native source frame has no mapped owner; refusing to omit source-backed facts",
        ));
    }
    let belongs = |unit: &str, source: Option<&SourceAnchor>| {
        source.is_none()
            || wanted
                .as_ref()
                .is_some_and(|(u, f)| unit == u && source.is_some_and(|s| &s.file_revision == f))
    };
    // A relationship may carry a source-backed target without a symbol row
    // (e.g. a rustc sysroot trait method). No declaration frame can emit that
    // synthesized target: the relationship's frame owns its publication. The
    // same applies to a parent supplied without a standalone declaration row.
    let synthetic_targets = fragment
        .entities
        .iter()
        .map(|e| {
            let parent = digest(&serde_json::to_vec(&(&e.unit, &e.provider_id, &e.source))?);
            super::normalize::native_relation_target_id(&e.unit, &e.provider_id, &e.source)
                .map(|id| (e.id.clone(), id == e.id || parent == e.id))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(id, synthetic)| synthetic.then_some(id))
        .collect::<HashSet<_>>();
    let owned = fragment
        .entities
        .iter()
        .filter(|e| belongs(&e.unit, e.source.as_ref()) || synthetic_targets.contains(&e.id))
        .map(|e| e.id.clone())
        .collect::<HashSet<_>>();
    let all_units = fragment
        .entities
        .iter()
        .map(|e| (e.id.clone(), e.unit.clone()))
        .collect::<HashMap<_, _>>();
    // Explicit native relationships belong to their use/impl file. Both ends
    // can be declarations in other files (e.g. an impl of an imported trait
    // for a type declared in another module). Endpoint ownership alone would
    // silently discard those relationships from every source frame.
    let native_relations = rows(frame, "relations")
        .iter()
        .filter_map(|r| {
            Some((
                r["from"].as_str()?,
                r["to"].as_str()?,
                RelationKind::parse(r["kind"].as_str()?)?,
            ))
        })
        .collect::<HashSet<_>>();
    let identities = fragment
        .entities
        .iter()
        .map(|e| (e.id.as_str(), e.provider_id.as_str()))
        .collect::<HashMap<_, _>>();
    let explicit_edges = fragment
        .relations
        .iter()
        .filter_map(|r| {
            let from = *identities.get(r.from.as_str())?;
            let to = *identities.get(r.to.as_str())?;
            native_relations
                .contains(&(from, to, r.kind))
                .then(|| r.id.clone())
        })
        .collect::<HashSet<_>>();
    fragment.entities.retain(|e| owned.contains(&e.id));
    fragment
        .bodies
        .retain(|b| belongs(&b.unit, Some(&b.source)));
    fragment
        .occurrences
        .retain(|o| belongs(&o.unit, Some(&o.source)));
    let occurrences = fragment
        .occurrences
        .iter()
        .map(|o| o.id.clone())
        .collect::<HashSet<_>>();
    fragment.relations.retain(|r| {
        r.occurrence.as_ref().map_or_else(
            || owned.contains(&r.from) || owned.contains(&r.to) || explicit_edges.contains(&r.id),
            |o| occurrences.contains(o),
        )
    });
    fragment.documents.retain(|d| {
        wanted.as_ref().is_some_and(|(u, f)| {
            d.file_revision == *f
                && (u == "external"
                    || d.owner
                        .as_ref()
                        .and_then(|id| all_units.get(id))
                        .is_none_or(|unit| unit == u))
        })
    });
    Ok(FrameOutput { fragment, peak })
}
pub(super) fn merge_frame(
    s: &mut SemanticSnapshot,
    output: FrameOutput,
    sink: &dyn Store,
    first: &mut bool,
    gap_keys: &mut HashSet<String>,
    source_state: &mut FrameSources,
) -> Result<()> {
    let mut fragment = output.fragment;
    source_state.merge(&fragment.sources.files)?;
    s.provenance_records
        .extend(fragment.provenance_records.clone());
    if *first {
        s.coverage.append(&mut fragment.coverage);
        *first = false;
    }
    for g in fragment.gaps.drain(..) {
        if gap_keys.insert(serde_json::to_string(&g)?) {
            s.gaps.push(g);
        }
    }
    records::drain(&mut fragment, sink)?;
    Ok(())
}
