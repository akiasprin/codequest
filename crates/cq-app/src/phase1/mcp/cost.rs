//! Delivery receipts belong to one MCP connection, never to the fact store.
use super::*;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub(super) struct Session {
    lines: HashMap<(String, String, String), BTreeMap<u64, String>>,
    gaps: HashSet<String>,
    aliases: HashMap<(String, String, String), String>,
    ends: HashMap<(String, String, String), u64>,
    cursors: HashMap<String, Value>,
    line_count: usize,
}
const READS: &[&str] = &[
    "status",
    "generation",
    "cypher",
    "changes",
    "compare",
    "search",
    "definition",
    "type_definition",
    "references",
    "callers",
    "callees",
    "implementations",
    "details",
    "members",
    "bodies",
    "outline",
    "source",
    "docs",
    "document",
    "doc_search",
    "trace",
    "impact",
    "path",
    "coverage",
    "contexts",
    "topology",
    "build_configuration",
];

impl Session {
    /// Stateless navigation never consumes source receipts. Only its cursor
    /// display options need to be delivered to this connection's later reads.
    pub(super) fn merge_navigation(&mut self, other: Self) {
        for (cursor, options) in other.cursors {
            if self.cursors.len() >= 128 {
                self.cursors.clear();
            }
            self.cursors.insert(cursor, options);
        }
    }
    pub(super) fn call(
        &mut self,
        service: &Arc<Service>,
        name: &str,
        mut args: Value,
        compact: bool,
    ) -> Result<Value> {
        if args.is_null() {
            args = json!({});
        }
        if !args.is_object() {
            return Err(Error::new(
                "InvalidArguments",
                "MCP arguments must be an object",
            ));
        }
        if args["operation"] == "reset_evidence" && name == "query_codequest" {
            *self = Self::default();
            return Ok(json!({"state":"evidence_receipts_reset"}));
        }
        let tracked = [
            "list_projects",
            "detect_changes",
            "compare_graphs",
            "check_index_coverage",
            "query_graph",
            "query_codequest",
            "get_code_snippet",
            "search_graph",
            "trace_path",
            "get_file_outline",
            "get_architecture",
            "documents",
            "search_code",
            "inspect_code_graph",
        ]
        .contains(&name);
        let op = args["operation"].as_str().unwrap_or("overview");
        let read = READS.contains(&op)
            || [
                "next",
                "overview",
                "gaps",
                "capabilities",
                "entrypoints",
                "schema",
                "annotations",
                "scopes",
                "observations",
                "provenance_observations",
                "runtime_traces",
                "hotspots",
                "cycles",
                "clusters",
            ]
            .contains(&op);
        if !tracked || name == "query_codequest" && !read && args.get("queries").is_none() {
            return super::call_raw(service, name, args, compact);
        }
        if let Some(options) = args["cursor"].as_str().and_then(|id| self.cursors.get(id)) {
            for (key, value) in options.as_object().unwrap() {
                if args.get(key).is_none() {
                    args[key] = value.clone();
                }
            }
        }
        let budget = args["max_bytes"].as_u64().unwrap_or(32768) as usize;
        if !(2048..=1024 * 1024).contains(&budget)
            && (args.get("queries").is_some() || args.get("ranges").is_some())
        {
            return Err(Error::new(
                "BatchBudget",
                "batch max_bytes must be 2048..1048576",
            ));
        }
        let deadline = Instant::now()
            + Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(5000).clamp(1, 300000));
        let mut trial = self.clone();
        let value = if name == "query_codequest" && args.get("queries").is_some() {
            trial.batch(service, &args, false, compact, deadline)?
        } else if name == "get_code_snippet" && args.get("ranges").is_some() {
            trial.batch(service, &args, true, compact, deadline)?
        } else {
            trial.single(service, name, args, compact, deadline)?
        };
        if serde_json::to_vec(&value)?.len() > budget {
            return Err(Error::new(
                "McpByteBudget",
                "response exceeds max_bytes; use a smaller page/range or a larger budget; no delivery receipts committed",
            ));
        }
        *self = trial;
        Ok(value)
    }

    fn batch(
        &mut self,
        s: &Arc<Service>,
        a: &Value,
        source: bool,
        compact: bool,
        deadline: Instant,
    ) -> Result<Value> {
        let entries = a[if source { "ranges" } else { "queries" }]
            .as_array()
            .ok_or_else(|| Error::new("InvalidBatch", "expected an array"))?;
        if entries.is_empty() || entries.len() > 8 {
            return Err(Error::new("BatchLimit", "provide 1..8 independent reads"));
        }
        let project = a["project"]
            .as_str()
            .ok_or_else(|| Error::new("MissingProject", "project is required"))?;
        let revision = a["revision"]
            .as_str()
            .map(str::to_owned)
            .or(s.store.head(project)?)
            .ok_or_else(|| Error::new("MissingRevision", "index this project first"))?;
        // Keep the entire batch's version admitted, including between child reads.
        let _pin = if s.store.paged_enabled(project) {
            Some(s.store.open_facts(project, Some(&revision))?)
        } else {
            None
        };
        let budget = a["max_bytes"].as_u64().unwrap_or(32768) as usize;
        let mut out = json!({"revision":revision,"items":[],"gaps":[],"state":"batch_complete"});
        let mut shared = HashSet::new();
        let mut deferred = 0;
        let mut errors = 0;
        for (index, entry) in entries.iter().enumerate() {
            let mut request = a.clone();
            request.as_object_mut().unwrap().remove("queries");
            request.as_object_mut().unwrap().remove("ranges");
            let Some(fields) = entry.as_object() else {
                return Err(Error::new("InvalidBatch", "every entry must be an object"));
            };
            for (k, v) in fields {
                request[k] = v.clone();
            }
            if entry.get("queries").is_some()
                || entry.get("ranges").is_some()
                || entry["project"].as_str().is_some_and(|p| p != project)
                || entry["revision"].as_str().is_some_and(|r| r != revision)
                || (!source && !READS.contains(&request["operation"].as_str().unwrap_or("")))
            {
                out["items"].as_array_mut().unwrap().push(json!({"index":index,"status":"error","error":"batch entries must be read-only and use the batch project/revision"}));
                errors += 1;
                continue;
            }
            request["project"] = json!(project);
            request["revision"] = json!(revision);
            request["max_bytes"] = json!(
                request["max_bytes"]
                    .as_u64()
                    .unwrap_or(budget as u64)
                    .min(budget as u64)
            );
            if request.get("limit").is_none() && !source {
                request["limit"] = json!(3);
            }
            let next_hint = request.clone();
            let mut trial = self.clone();
            let candidate = if Instant::now() >= deadline {
                None
            } else {
                match trial.single(
                    s,
                    if source {
                        "get_code_snippet"
                    } else {
                        "query_codequest"
                    },
                    request,
                    compact,
                    deadline,
                ) {
                    Ok(mut value) => {
                        if value["revision"] != revision {
                            return Err(Error::new(
                                "RevisionMismatch",
                                "batch child changed revision",
                            ));
                        }
                        value.as_object_mut().unwrap().remove("revision");
                        Some(json!({"index":index,"status":"ok","result":value}))
                    }
                    Err(e) => Some(json!({"index":index,"status":"error","error":e.to_string()})),
                }
            };
            // Trial state and response are admitted together. An omitted child
            // is explicitly retryable and cannot consume a delivery receipt.
            let mut proposed = out.clone();
            if let Some(mut candidate) = candidate {
                let mut shared_trial = shared.clone();
                if let Some(gaps) = candidate["result"]
                    .as_object_mut()
                    .and_then(|m| m.remove("gaps"))
                {
                    for gap in gaps.as_array().into_iter().flatten() {
                        let key = gap.to_string();
                        if shared_trial.insert(key) {
                            proposed["gaps"].as_array_mut().unwrap().push(gap.clone());
                        }
                    }
                }
                proposed["items"]
                    .as_array_mut()
                    .unwrap()
                    .push(candidate.clone());
                let reserve = (entries.len() - index - 1) * 320 + 128;
                if serde_json::to_vec(&proposed)?.len() + reserve <= budget {
                    if candidate["status"] == "error" {
                        errors += 1;
                    } else {
                        *self = trial;
                    }
                    out = proposed;
                    shared = shared_trial;
                    continue;
                }
            }
            deferred += 1;
            out["items"]
                .as_array_mut()
                .unwrap()
                .push(json!({"index":index,"status":"deferred","continue_with":next_hint}));
        }
        out["deferred"] = json!(deferred);
        out["errors"] = json!(errors);
        if deferred > 0 || errors > 0 {
            out["state"] = json!("batch_partial");
        }
        Ok(out)
    }

    fn single(
        &mut self,
        s: &Arc<Service>,
        name: &str,
        mut a: Value,
        compact_default: bool,
        deadline: Instant,
    ) -> Result<Value> {
        let initial = self.clone();
        if Instant::now() >= deadline {
            return Err(Error::new("Timeout", "MCP request deadline exhausted"));
        }
        let compact = a["compact"].as_bool().unwrap_or(compact_default);
        let dedup = a["dedup"].as_bool().unwrap_or(true);
        let force = a["force_repeat"].as_bool().unwrap_or(false)
            || matches!(a["operation"].as_str(), Some("gaps" | "coverage"));
        let project = a["project"].as_str().unwrap_or("").to_owned();
        let mut is_source =
            name == "get_code_snippet" || name == "query_codequest" && a["operation"] == "source";
        if is_source {
            if a.get("max_lines").is_none() {
                a["max_lines"] = json!(24);
            }
            if a.get("limit").is_none() {
                a["limit"] = a["max_lines"].clone();
            }
            a["compact"] = json!(false); // Exact anchors are internal receipt coordinates.
            if dedup && !force && a["source_view"] != "lines" {
                let revision = a["revision"]
                    .as_str()
                    .map(str::to_owned)
                    .or(s.store.head(&project)?);
                if let Some(revision) = revision
                    && let Some(reused) = self.reused_window(&a, &project, &revision)
                {
                    return Ok(reused);
                }
            }
        }
        a["timeout_ms"] = json!(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1) as u64
        );
        let mut result = super::call_raw(s, name, a.clone(), compact_default)?;
        if !result.is_object() {
            return Ok(result);
        }
        let original = result.clone();
        is_source |= result["items"]
            .as_array()
            .and_then(|v| v.first())
            .is_some_and(|r| r["text"].is_string() && r["source"]["file_revision"].is_string());
        if is_source
            && let (Some(file), Some(revision), Some(id)) = (
                a["file"].as_str(),
                result["revision"].as_str(),
                result["items"][0]["source"]["file_revision"].as_str(),
            )
        {
            if self.aliases.len() >= 256 {
                self.aliases.clear();
            }
            self.aliases
                .insert((project.clone(), revision.into(), file.into()), id.into());
        }
        if is_source && a["source_view"] != "lines" {
            result = self.source_blocks(result, &project, dedup, force)?;
        }
        if compact
            && a["diagnostics"] != true
            && let Some(m) = result.as_object_mut()
        {
            m.remove("read_stats");
            m.remove("execution_path");
            m.remove("read_stats_scope");
            m.remove("work");
            m.remove("elapsed_ms");
            for key in ["provenance_records", "coverage", "hidden"] {
                if m.get(key).is_some_and(|v| {
                    v.as_array().is_some_and(Vec::is_empty)
                        || v.as_object().is_some_and(serde_json::Map::is_empty)
                }) {
                    m.remove(key);
                }
            }
            for key in ["context", "ranking", "next_cursor"] {
                if m.get(key).is_some_and(Value::is_null) {
                    m.remove(key);
                }
            }
            if m.get("truncated") == Some(&json!(false)) {
                m.remove("truncated");
            }
        }
        if compact && dedup {
            self.fold_gaps(&mut result, &project, force);
        }
        let enrich = !is_source
            && compact
            && a["evidence"].as_bool().unwrap_or(true)
            && [
                "search_graph",
                "query_codequest",
                "trace_path",
                "inspect_code_graph",
            ]
            .contains(&name);
        if enrich {
            let mut points = vec![];
            collect_points(&result["items"], None, &mut points);
            let revision = result["revision"].as_str().unwrap_or("").to_owned();
            let mut seen = HashSet::new();
            for (file, line) in points
                .into_iter()
                .filter(|p| seen.insert(p.clone()))
                .take(2)
            {
                if Instant::now() >= deadline {
                    break;
                }
                let before = self.clone();
                let context = a["context_lines"].as_u64().unwrap_or(4).min(20);
                let request = json!({"project":project,"revision":revision,"file_revision":file,
                    "line":line.saturating_sub(context).max(1),"max_lines":context*2+1,"limit":context*2+1,
                    "max_bytes":16384,"compact":false,"include_external":a["include_external"],
                    "timeout_ms":deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as u64});
                // Source retrieval is optional evidence, never authority for a new edge.
                let mut request = request;
                if request["include_external"].is_null() {
                    request["include_external"] = json!(false);
                }
                let fetched = if dedup && !force {
                    self.reused_window(&request, &project, &revision)
                } else {
                    None
                };
                let fetched = if let Some(value) = fetched {
                    Ok(value)
                } else {
                    super::call_raw(s, "get_code_snippet", request, compact_default)
                        .and_then(|raw| self.source_blocks(raw, &project, dedup, force))
                };
                match fetched {
                    Ok(mut evidence) => {
                        for key in [
                            "gaps",
                            "coverage",
                            "provenance_records",
                            "total",
                            "total_kind",
                            "work",
                            "elapsed_ms",
                            "context",
                            "read_stats",
                            "execution_path",
                            "read_stats_scope",
                        ] {
                            evidence.as_object_mut().unwrap().remove(key);
                        }
                        evidence["scope"] = json!("line_window");
                        evidence["complete_control_flow"] = json!(false);
                        evidence["continue_with"] = json!({"operation":"source","project":project,"revision":revision,"file_revision":file,"line":line.saturating_sub(context).max(1),"max_lines":24,"force_repeat":true});
                        let mut candidate = result.clone();
                        candidate
                            .as_object_mut()
                            .unwrap()
                            .entry("evidence")
                            .or_insert(json!([]))
                            .as_array_mut()
                            .unwrap()
                            .push(evidence);
                        let budget = a["max_bytes"].as_u64().unwrap_or(32768) as usize;
                        let evidence_bytes = serde_json::to_vec(&candidate["evidence"])?.len();
                        if evidence_bytes <= 4096 && serde_json::to_vec(&candidate)?.len() <= budget
                        {
                            result = candidate;
                        } else {
                            *self = before;
                        }
                    }
                    Err(e) => {
                        *self = before;
                        result["evidence_unavailable"] = json!(e.to_string());
                        break;
                    }
                }
            }
        }
        let budget = a["max_bytes"].as_u64().unwrap_or(32768) as usize;
        if serde_json::to_vec(&result)?.len() > budget {
            // The outer trial will be discarded if even the unchanged page
            // cannot fit. No receipt may refer to bytes never delivered.
            if serde_json::to_vec(&original)?.len() <= budget {
                *self = initial;
                self.remember_cursor(&original, &a, compact);
                return Ok(original);
            }
            return Err(Error::new(
                "McpByteBudget",
                "source window exceeds response budget",
            ));
        }
        self.remember_cursor(&result, &a, compact);
        Ok(result)
    }

    fn fold_gaps(&mut self, result: &mut Value, project: &str, force: bool) {
        let revision = result["revision"].as_str().unwrap_or("").to_owned();
        if let Some(gaps) = result.get_mut("gaps").and_then(Value::as_array_mut) {
            for gap in gaps {
                let id = format!(
                    "gap:{}",
                    cq_core::model::digest(format!("{project}:{revision}:{gap}").as_bytes())
                );
                if self.gaps.contains(&id) && !force {
                    *gap = json!({"ref":id,"code":gap["code"],"scope":gap["scope"]});
                } else {
                    gap["id"] = json!(id);
                    self.gaps.insert(id);
                }
            }
        }
        if self.gaps.len() > 256 {
            self.gaps.clear();
        }
    }

    fn source_blocks(
        &mut self,
        mut raw: Value,
        project: &str,
        dedup: bool,
        force: bool,
    ) -> Result<Value> {
        let revision = raw["revision"]
            .as_str()
            .ok_or_else(|| Error::new("MissingRevision", "frozen source needs a revision"))?
            .to_owned();
        let rows = raw["items"]
            .as_array()
            .ok_or_else(|| Error::new("InvalidSource", "expected source rows"))?
            .clone();
        let mut blocks: Vec<Value> = vec![];
        let mut reused: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut new_lines = 0;
        if self.line_count + rows.len() > 4096 {
            self.lines.clear();
            self.ends.clear();
            self.line_count = 0;
            raw["receipts_reset"] = json!(true);
        }
        for row in rows {
            let anchor = &row["source"];
            let file = anchor["file_revision"]
                .as_str()
                .ok_or_else(|| Error::new("MissingEvidence", "source row lacks a frozen identity"))?
                .to_owned();
            let line = row["line"]
                .as_u64()
                .ok_or_else(|| Error::new("InvalidSource", "source row lacks a line"))?;
            let key = (project.to_owned(), revision.clone(), file.clone());
            if row.get("next_line").is_some_and(Value::is_null) {
                self.ends.insert(key.clone(), line);
            }
            if dedup
                && !force
                && let Some(id) = self.lines.get(&key).and_then(|m| m.get(&line))
            {
                reused.entry(id.clone()).or_default().push(line);
                continue;
            }
            let append = blocks.last().is_some_and(|b| {
                b["file_revision"] == file
                    && b["end_line"].as_u64() == Some(line - 1)
                    && b["source"]["end"] == anchor["start"]
            });
            if append {
                let b = blocks.last_mut().unwrap();
                b["end_line"] = json!(line);
                b["source"]["end"] = anchor["end"].clone();
                b["text"] = json!(format!(
                    "{}{}",
                    b["text"].as_str().unwrap(),
                    row["text"].as_str().unwrap_or("")
                ));
            } else {
                blocks.push(
                    json!({"file":row["file"],"file_revision":file,"line":line,"end_line":line,
                    "source":anchor,"text":row["text"]}),
                );
            }
            new_lines += 1;
        }
        for b in &mut blocks {
            let id = format!(
                "ev:{}",
                cq_core::model::digest(format!("{project}:{revision}:{}", b["source"]).as_bytes())
            );
            b["id"] = json!(id);
            if dedup {
                let key = (
                    project.to_owned(),
                    revision.clone(),
                    b["file_revision"].as_str().unwrap().to_owned(),
                );
                let lines = self.lines.entry(key).or_default();
                for line in b["line"].as_u64().unwrap()..=b["end_line"].as_u64().unwrap() {
                    if lines.insert(line, id.clone()).is_none() {
                        self.line_count += 1;
                    }
                }
            }
        }
        raw["items"] = json!(blocks);
        raw["total_unit"] = json!("source_lines");
        if !reused.is_empty() {
            raw["reused"] = json!(
                reused
                    .iter()
                    .map(|(id, lines)| json!({"evidence":id,"lines":lines}))
                    .collect::<Vec<_>>()
            );
        }
        raw["source_delivery"] = json!({"new_lines":new_lines,"reused_lines":reused.values().map(Vec::len).sum::<usize>()});
        if new_lines == 0 && !reused.is_empty() {
            raw["state"] = json!("already_delivered");
        }
        Ok(raw)
    }

    fn reused_window(&self, a: &Value, project: &str, revision: &str) -> Option<Value> {
        let file = a["file_revision"].as_str().or_else(|| {
            a["file"].as_str().and_then(|p| {
                self.aliases
                    .get(&(project.into(), revision.into(), p.into()))
                    .map(String::as_str)
            })
        })?;
        let start = a["line"].as_u64()?;
        let count = a["max_lines"].as_u64()?;
        if start == 0 || count == 0 || count > 1000 {
            return None;
        }
        let lines = self
            .lines
            .get(&(project.into(), revision.into(), file.into()))?;
        let mut refs: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let end = start.checked_add(count)?.min(
            self.ends
                .get(&(project.into(), revision.into(), file.into()))
                .map_or(u64::MAX, |n| n + 1),
        );
        if start >= end {
            return None;
        }
        for line in start..end {
            refs.entry(lines.get(&line)?.clone())
                .or_default()
                .push(line);
        }
        Some(
            json!({"revision":revision,"items":[],"state":"already_delivered","receipt_hit":true,
            "reused":refs.into_iter().map(|(id,lines)|json!({"evidence":id,"lines":lines})).collect::<Vec<_>>(),
            "source_delivery":{"new_lines":0,"reused_lines":end-start},"continue_with":{"operation":"source","project":project,"revision":revision,"file_revision":file,"line":start,"max_lines":count,"force_repeat":true}}),
        )
    }

    fn remember_cursor(&mut self, result: &Value, a: &Value, compact: bool) {
        if let Some(cursor) = result["next_cursor"].as_str() {
            if self.cursors.len() >= 128 {
                self.cursors.clear();
            }
            let mut options = json!({"compact":compact});
            for key in [
                "source_view",
                "dedup",
                "force_repeat",
                "evidence",
                "diagnostics",
                "max_bytes",
            ] {
                if let Some(value) = a.get(key) {
                    options[key] = value.clone();
                }
            }
            self.cursors.insert(cursor.into(), options);
        }
    }
}

fn collect_points(v: &Value, file: Option<&str>, out: &mut Vec<(String, u64)>) {
    match v {
        Value::Array(a) => {
            for x in a {
                collect_points(x, file, out)
            }
        }
        Value::Object(m) => {
            let file = m.get("file_revision").and_then(Value::as_str).or(file);
            let skip = m.get("kind").and_then(Value::as_str).is_some_and(|k| {
                [
                    "Module",
                    "File",
                    "Parameter",
                    "Variable",
                    "CompilationUnit",
                    "Body",
                ]
                .contains(&k)
            });
            if !skip
                && m.get("text").is_none()
                && let (Some(file), Some(line)) = (file, m.get("line").and_then(Value::as_u64))
            {
                out.push((file.into(), line));
            }
            for x in m.values() {
                if x.is_array() || x.is_object() {
                    collect_points(x, file, out);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    fn source(revision: &str, file: &str, start: u64, count: u64) -> Value {
        json!({"revision":revision,"items":(start..start+count).map(|line|json!({
            "file":"same.rs","line":line,"text":format!("line {line}\n"),"next_line":line+1,
            "source":{"file_revision":file,"start":line*32,"end":(line+1)*32}})).collect::<Vec<_>>()})
    }
    #[test]
    fn receipts_are_revision_project_and_file_scoped_and_bounded() {
        let mut session = Session::default();
        let first = session
            .source_blocks(source("v1", "f1", 1, 4), "p1", true, false)
            .unwrap();
        assert_eq!(first["source_delivery"]["new_lines"], 4);
        let overlap = session
            .source_blocks(source("v1", "f1", 3, 4), "p1", true, false)
            .unwrap();
        assert_eq!(overlap["source_delivery"]["new_lines"], 2);
        assert_eq!(overlap["source_delivery"]["reused_lines"], 2);
        for (project, revision, file) in
            [("p2", "v1", "f1"), ("p1", "v2", "f1"), ("p1", "v1", "f2")]
        {
            assert_eq!(
                session
                    .source_blocks(source(revision, file, 1, 4), project, true, false)
                    .unwrap()["source_delivery"]["new_lines"],
                4
            );
        }
        for i in 0..6 {
            session
                .source_blocks(
                    source("v1", &format!("large-{i}"), 1, 1000),
                    "p1",
                    true,
                    false,
                )
                .unwrap();
            assert!(session.line_count <= 4096);
        }
        assert!(
            session
                .reused_window(
                    &json!({"file_revision":"f1","line":1,"max_lines":1}),
                    "p1",
                    "v1"
                )
                .is_none()
        );
    }
    #[test]
    fn repeated_gap_retains_code_scope_and_explicit_refresh() {
        let mut s = Session::default();
        let make = || json!({"revision":"v1","gaps":[{"code":"PartialCompilerCoverage","scope":"unit","message":"details"}]});
        let mut a = make();
        s.fold_gaps(&mut a, "p", false);
        let mut b = make();
        s.fold_gaps(&mut b, "p", false);
        assert_eq!(a["gaps"][0]["id"], b["gaps"][0]["ref"]);
        assert_eq!(b["gaps"][0]["code"], "PartialCompilerCoverage");
        assert_eq!(b["gaps"][0]["scope"], "unit");
        let mut c = make();
        s.fold_gaps(&mut c, "p", true);
        assert_eq!(c["gaps"][0]["message"], "details");
    }
}
