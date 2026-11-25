//! Newline-delimited MCP JSON-RPC over stdio. Stdout is reserved for protocol.
use super::{Service, index::IndexRequest};
use cq_core::{Error, Result};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    sync::{Arc, Mutex},
};
mod cost;
#[cfg(test)]
mod cost_tests;
const AGENT_GUIDE: &str = "Use Grep/Read for literal working-tree lookup. Use CodeQuest for compiler-resolved receiver types, references and multi-hop paths. Query results may include frozen source windows: cite these directly; expand when surrounding conditions remain unknown. Batch independent operations with query_codequest.queries or get_code_snippet.ranges (up to 8). Repeated source is returned by evidence reference; force_repeat=true resends it, operation=reset_evidence clears this connection's receipts after context compaction. Lead answers with the conclusion, necessary changes/boundaries and a few decisive source citations; expand rationale on request. Disclose unresolved branches and missing coverage when they affect the current conclusion. Inspect unit/build_context on symbol results; different targets, features and test builds remain distinct. Select context explicitly instead of taking the first duplicate. build_configuration summaries use items[].target/features/test; configuration_view=full requests raw evidence. file is an exact path, directory selects a subtree, and Callable includes functions and methods. Language-not-indexed responses require source/text fallback. Index-wide diagnostics are available through gaps/coverage; do not repeat unrelated warnings. A small window is not complete control flow.";
#[cfg(test)]
use call_raw as call;

fn fields(name: &str) -> Vec<&'static str> {
    if name == "list_projects" {
        return vec![];
    }
    let mut fields = vec!["project", "revision", "max_bytes", "compact"];
    let extra: &[&str] = match name {
        "query_codequest" => &[
            "operation",
            "configuration_view",
            "subject",
            "target",
            "text",
            "context",
            "file",
            "directory",
            "file_revision",
            "kind",
            "relation",
            "direction",
            "mode",
            "depth",
            "limit",
            "max_work",
            "timeout_ms",
            "cursor",
            "line",
            "column",
            "max_lines",
            "regex",
            "include_candidates",
            "include_external",
            "document_kind",
            "authority",
            "search_view",
            "queries",
            "evidence",
            "context_lines",
            "dedup",
            "force_repeat",
            "diagnostics",
            "source_view",
            "explicit_job",
            "max_job_bytes",
            "exact_semantic",
        ],
        "search_graph" => &[
            "text",
            "kind",
            "file",
            "directory",
            "context",
            "limit",
            "max_work",
            "timeout_ms",
            "include_external",
            "semantic",
            "exact_semantic",
            "search_view",
            "authority",
            "evidence",
            "context_lines",
            "dedup",
            "force_repeat",
            "diagnostics",
        ],
        "get_code_snippet" => &[
            "subject",
            "file",
            "file_revision",
            "line",
            "max_lines",
            "limit",
            "cursor",
            "ranges",
            "dedup",
            "force_repeat",
            "source_view",
            "timeout_ms",
            "diagnostics",
        ],
        "trace_path" => &[
            "subject",
            "target",
            "operation",
            "mode",
            "direction",
            "depth",
            "limit",
            "context",
            "include_candidates",
            "authority",
            "timeout_ms",
            "max_work",
            "evidence",
            "context_lines",
            "dedup",
            "force_repeat",
        ],
        "get_file_outline" => &["file", "kind", "limit", "cursor", "context"],
        "get_architecture" => &[
            "operation",
            "context",
            "limit",
            "explicit_job",
            "max_job_bytes",
            "timeout_ms",
        ],
        "documents" => &[
            "operation",
            "subject",
            "text",
            "document_kind",
            "line",
            "max_lines",
            "limit",
            "cursor",
            "authority",
        ],
        "query_graph" => &[
            "text",
            "timeout_ms",
            "max_work",
            "explicit_job",
            "max_job_bytes",
        ],
        "index_repository" => &[
            "root",
            "name",
            "language",
            "build_args",
            "compdb",
            "include_docs",
            "allow_partial",
        ],
        "index_status" => &["job", "cancel"],
        "compare_graphs" => &["base"],
        "manage_adr" => &[
            "action", "kind", "parent", "author", "entities", "evidence", "status", "text",
        ],
        "ingest_traces" => &["events", "source"],
        "inspect_code_graph" => &["request", "evidence", "dedup", "force_repeat"],
        "search_code" => &[
            "text",
            "regex",
            "file",
            "limit",
            "max_work",
            "timeout_ms",
            "cursor",
            "include_external",
        ],
        _ => &[],
    };
    fields.extend_from_slice(extra);
    fields
}
fn tools() -> Vec<Value> {
    let mut tools = vec![];
    for (name, description) in [
        (
            "query_codequest",
            "Unified query: definition, type_definition, references, callers, callees, implementations, details, members, bodies, topology, build_configuration, documents, source, authority filters, trace and next. Every continue_with object is accepted here.",
        ),
        (
            "index_repository",
            "Observe a real build and publish immutable compiler-resolved facts. Returns an asynchronous job.",
        ),
        (
            "search_graph",
            "Search declarations by name or text. Default overview groups matches by compiler module or source file, with up to three symbols per group and expansion hints. Explicit kind/file/directory filters or search_view=symbols return flat symbols including locals. Results retain build context.",
        ),
        (
            "query_graph",
            "Execute read-only Cypher on the Ladybug projection.",
        ),
        (
            "trace_path",
            "Explore bounded static paths; deferred and unresolved calls are not runtime facts.",
        ),
        (
            "get_code_snippet",
            "Read frozen indexed source, with source anchors and pagination.",
        ),
        (
            "get_file_outline",
            "Declarations in a fixed source revision.",
        ),
        (
            "get_graph_schema",
            "Discover actual entity and relationship kinds.",
        ),
        ("compare_graphs", "Compare immutable revisions."),
        (
            "get_architecture",
            "Overview, topology, hotspots, cycles or clusters.",
        ),
        ("search_code", "Literal/regex search in indexed source."),
        ("list_projects", "List local project indexes."),
        (
            "delete_project",
            "Delete a project index; does not delete repository source.",
        ),
        (
            "index_status",
            "Poll or cancel an index job, or inspect a project.",
        ),
        (
            "check_index_coverage",
            "Provider capability, build-context and unresolved coverage.",
        ),
        (
            "detect_changes",
            "Compare indexed file digests to the working tree.",
        ),
        (
            "manage_adr",
            "Create or list evidence-backed annotations and ADRs.",
        ),
        (
            "ingest_traces",
            "Store runtime observations bound to exact revision entities, separately from static facts.",
        ),
        (
            "inspect_code_graph",
            "lookup/details/trace/overview/topology/entrypoints/tour/escape; progressive disclosure with cursors.",
        ),
        (
            "documents",
            "Search or page repository documents and rustdoc with exact evidence.",
        ),
        (
            "refresh_repository",
            "Repeat the saved build configuration and publish a new revision.",
        ),
    ] {
        let mut properties = json!({"project":{"type":"string","description":"Name returned by list_projects"},"revision":{"type":"string","description":"Pin an immutable revision; a cursor pins its own revision"},"operation":{"type":"string"},"subject":{"type":"string","description":"Exact entity id preferred; ambiguous names return candidates"},"target":{"type":"string"},"text":{"type":"string"},"context":{"type":"string"},"file":{"type":"string"},"kind":{"type":"string"},"relation":{"type":"string"},"direction":{"enum":["outbound","inbound","both"]},"depth":{"type":"integer","minimum":0,"maximum":4096},"limit":{"type":"integer","minimum":1,"maximum":1000},"max_bytes":{"type":"integer","minimum":128},"max_work":{"type":"integer","minimum":1},"timeout_ms":{"type":"integer","minimum":1},"cursor":{"type":"string"},"line":{"type":"integer","minimum":1},"column":{"type":"integer","minimum":1,"description":"One-based UTF-8 byte column"},"max_lines":{"type":"integer","minimum":1},"regex":{"type":"boolean"},"include_candidates":{"type":"boolean"},"include_external":{"type":"boolean"},"semantic":{"type":"boolean"},"document_kind":{"type":"string"}});
        properties["max_job_bytes"] = json!({"type":"integer","minimum":1,"description":"Explicit global/export job encoded-data budget; default 64 MiB"});
        properties["exact_semantic"] = json!({"type":"boolean","description":"Experimental vector retrieval, disabled by default; LSH is not part of default acceptance"});
        properties["explicit_job"] = json!({"type":"boolean","description":"Allow an explicitly requested generation/export analysis job"});
        properties["authority"] = json!({"enum":["SemanticResolution","RuleDerived","AuthorStatement","Hypothesis","RuntimeObservation"],"description":"Filter returned relationship/search evidence; symbol identity anchors remain available"});
        properties["mode"] = json!({"enum":["calls","data_flow","types"],"description":"Static calls or positional argument-to-parameter contribution; no SSA/runtime claim"});
        properties["compact"] = json!({"type":"boolean","description":"Omit verbose provenance records, raw byte spans, and repetitive metadata; returns relative file:line locations and owner symbol names."});
        properties["compact"]["default"] = json!(true);
        properties["search_view"] = json!({"enum":["overview","symbols"],"description":"Search overview groups declarations by compiler module or source file, with local symbols collapsed. symbols returns flat results including locals. Explicit kind/file/directory filters default to symbols."});
        properties["file"] = json!({"type":"string","description":"Exact file path or file suffix, not a directory. Use directory for a subtree."});
        properties["directory"] = json!({"type":"string","description":"Explicit source subtree; mutually exclusive with file."});
        properties["kind"] = json!({"type":"string","description":"Exact entity kind. Function excludes Method; Callable includes both. Omit when uncertain."});
        properties["context"] = json!({"type":"string","description":"Analysis context ID or build entry-unit ID. Inspect build_context before selecting a target/feature variant."});
        properties["configuration_view"] = json!({"enum":["summary","full"],"default":"summary","description":"build_configuration summary returns flat id/name/target/features/test fields; full returns raw unit evidence and arguments."});
        properties["file_revision"] = json!({"type":"string","description":"Exact frozen file identity returned by the graph"});
        properties["evidence"] = json!({"type":"boolean","default":true,"description":"Attach up to two small frozen source windows; expand for conditions outside the window"});
        properties["context_lines"] = json!({"type":"integer","minimum":0,"maximum":20});
        properties["dedup"] = json!({"type":"boolean","default":true});
        properties["force_repeat"] = json!({"type":"boolean","description":"Resend source/gaps already delivered on this connection"});
        properties["diagnostics"] =
            json!({"type":"boolean","description":"Include read statistics in compact output"});
        properties["source_view"] = json!({"enum":["blocks","lines"],"default":"blocks"});
        let query_item = json!({"type":"object","properties":{"operation":{"type":"string"},"subject":{"type":"string"},"target":{"type":"string"},"text":{"type":"string"},"file":{"type":"string"},"file_revision":{"type":"string"},"line":{"type":"integer"},"max_lines":{"type":"integer"},"kind":{"type":"string"},"depth":{"type":"integer"},"mode":{"enum":["calls","types","data_flow"]}},"additionalProperties":true});
        properties["queries"] = json!({"type":"array","minItems":1,"maxItems":8,"items":query_item,"description":"Independent read-only operations, pinned to the batch revision; per-entry errors/deferred work are explicit"});
        properties["ranges"] = json!({"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{"subject":{"type":"string"},"file":{"type":"string"},"file_revision":{"type":"string"},"line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1,"maximum":1000}},"additionalProperties":false}});
        if name == "query_codequest" {
            properties["operation"] = json!({"type":"string","description":"Query operation or next with an opaque cursor; retains original revision, scope and filters"});
        }
        if name == "index_repository" {
            properties["root"] =
                json!({"type":"string","description":"Absolute local repository path"});
            properties["name"] = json!({"type":"string"});
            properties["language"] =
                json!({"enum":["auto","rust","go","c","cpp","typescript","ts"]});
            properties["build_args"] = json!({"type":"array","items":{"type":"string"},"description":"Cargo/Go command and arguments, e.g. [check,--workspace,--all-targets,--offline]"});
            properties["compdb"] =
                json!({"type":"string","description":"C/C++ compile_commands.json path"});
            properties["include_docs"] = json!({"type":"boolean","default":true});
            properties["allow_partial"] = json!({"type":"boolean","default":false,"description":"Explicitly publish verified scopes with unavailable scopes and evidence gaps; strict failure is the default"});
        }
        if name == "index_status" {
            properties["job"] = json!({"type":"string"});
            properties["cancel"] = json!({"type":"boolean"});
        }
        if name == "compare_graphs" {
            properties["base"] = json!({"type":"string"});
        }
        if name == "manage_adr" {
            properties["action"] = json!({"enum":["create","list","revise"]});
            properties["kind"] = json!({"enum":["Responsibility","Mechanism","Protocol","AuthorRationale","AuthorReflection","Hypothesis","Question","ArchitectureDecision"]});
            properties["parent"] = json!({"type":"string"});
            properties["author"] = json!({"type":"string"});
            properties["entities"] = json!({"type":"array","items":{"type":"string"}});
            properties["evidence"] = json!({"type":"array","items":{"type":"object","required":["file_revision","start","end"],"properties":{"file_revision":{"type":"string"},"start":{"type":"integer"},"end":{"type":"integer"}}}});
            properties["status"] = json!({"enum":["Proposed","Confirmed","Rejected"]});
        }
        if name == "ingest_traces" {
            properties["events"] = json!({"type":"array","items":{"type":"object","required":["from","to"],"properties":{"from":{"type":"string"},"to":{"type":"string"},"count":{"type":"integer","minimum":1}}}});
            properties["source"] =
                json!({"type":"string","description":"Trace origin, not static confidence"});
        }
        if name == "inspect_code_graph" {
            properties["request"] = json!({"type":"object","properties":{"type":{"enum":["lookup","details","trace","overview","topology","entrypoints","tour","escape"]},"text":{"type":"string"},"subject":{"type":"string"},"target":{"type":"string"},"depth":{"type":"integer"},"limit":{"type":"integer"},"cursor":{"type":"string"}},"description":"Small query; no private reasoning required"});
        }
        let required = if name == "index_repository" {
            vec!["root"]
        } else if ["list_projects", "index_status"].contains(&name) {
            vec![]
        } else {
            vec!["project"]
        };
        let allowed = fields(name);
        properties
            .as_object_mut()
            .unwrap()
            .retain(|k, _| allowed.contains(&k.as_str()));
        let description = match name {
            "search_graph" => {
                "Compiler symbol/type discovery with grouped disclosure and frozen source previews. Use Grep for plain text lookup. Narrow by kind/file or choose search_view=symbols for exact navigation."
            }
            "query_codequest" => {
                "Compiler-resolved navigation with frozen source windows. Use operation+subject, queries[] for up to 8 independent reads, or next+cursor. reset_evidence clears this connection's delivery receipts. Candidate edges and missing coverage remain explicit."
            }
            "get_code_snippet" => {
                "Read immutable source as bounded blocks (24 lines by default). Accepts file_revision/file/subject, or ranges[] for up to 8 reads. Overlap returns only new lines plus evidence references; force_repeat resends text. source_view=lines preserves line rows."
            }
            _ => description,
        };
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":true}}));
    }
    tools
}
fn call_raw(s: &Arc<Service>, name: &str, mut a: Value, compact_default: bool) -> Result<Value> {
    if name == "list_projects" {
        return Ok(json!(s.store.list_projects()?));
    }
    if compact_default && a.is_object() && a.get("compact").is_none() {
        a["compact"] = json!(true);
    }
    if name == "search_graph" && a.get("search_view").is_none() {
        a["search_view"] = json!(if a["kind"].is_string()
            || a["file"].is_string()
            || a["directory"].is_string()
        {
            "symbols"
        } else {
            "overview"
        });
    }
    if a.get("text").is_none() {
        if let Some(text) = a.get("query").cloned() {
            a["text"] = text;
        } else if let Some(terms) = a["semantic_query"].as_array() {
            a["text"] = json!(
                terms
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            a["semantic"] = json!(true);
        }
    }
    if name == "index_repository" {
        if a.get("root").is_none() {
            a["root"] = a["repo_path"].clone();
        }
        if a.get("name").is_none() {
            a["name"] = a.get("project").cloned().unwrap_or_else(|| {
                json!(
                    std::path::Path::new(a["root"].as_str().unwrap_or("project"))
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                )
            });
        }
        for (k, v) in [
            ("language", json!("auto")),
            ("build_args", json!([])),
            ("compdb", Value::Null),
            ("include_docs", json!(true)),
            ("hir", Value::Null),
            ("recorder", Value::Null),
        ] {
            if a.get(k).is_none() {
                a[k] = v;
            }
        }
        let r: IndexRequest = serde_json::from_value(a)?;
        return s.start(r);
    }
    if name == "index_status" && a["job"].is_string() {
        return s.job(
            a["job"].as_str().unwrap(),
            a["cancel"].as_bool().unwrap_or(false),
        );
    }
    let project = a["project"]
        .as_str()
        .ok_or_else(|| Error::new("MissingProject", "project is required"))?
        .to_owned();
    let revision = a["revision"].as_str().map(str::to_owned);
    if name == "refresh_repository" {
        return s.refresh(&project);
    }
    let operation = match name {
        "query_codequest" => a["operation"].as_str().unwrap_or("overview"),
        "search_graph" => {
            if a["semantic"].as_bool() == Some(true) {
                "semantic"
            } else {
                "search"
            }
        }
        "query_graph" => "cypher",
        "trace_path" => {
            if a["operation"].as_str() == Some("impact") {
                "impact"
            } else if a["target"].is_string() {
                "path"
            } else {
                "trace"
            }
        }
        "get_code_snippet" => "source",
        "get_file_outline" => "outline",
        "get_graph_schema" => "schema",
        "compare_graphs" => "compare",
        "get_architecture" => a["operation"].as_str().unwrap_or("overview"),
        "search_code" => "search_code",
        "delete_project" => "delete",
        "index_status" => "status",
        "check_index_coverage" => "coverage",
        "detect_changes" => "changes",
        "manage_adr" => {
            if a["action"] == "create" || a["action"] == "revise" {
                "annotate"
            } else {
                "annotations"
            }
        }
        "ingest_traces" => "traces",
        "documents" => a["operation"].as_str().unwrap_or("docs"),
        "inspect_code_graph" => {
            if a["request"].is_object() {
                let mut request = a["request"].clone();
                request["project"] = json!(project);
                a = request;
            }
            match a["type"].as_str().unwrap_or("overview") {
                "lookup" => "search",
                "escape" => "source",
                "details" => "details",
                "trace" => "trace",
                "overview" => "overview",
                "topology" => "topology",
                "entrypoints" => "entrypoints",
                "tour" => "tour",
                _ => return Err(Error::new("UnsupportedOperation", "inspect request type")),
            }
        }
        _ => return Err(Error::new("UnknownTool", name)),
    }
    .to_owned();
    s.dispatch(&project, &operation, a, revision.as_deref())
}
fn parallel_navigation(request: &Value) -> bool {
    if request["method"] != "tools/call" {
        return false;
    }
    let p = &request["params"];
    let a = &p["arguments"];
    a["evidence"] == false
        && a.get("queries").is_none()
        && a.get("ranges").is_none()
        && (p["name"] == "search_graph"
            || p["name"] == "query_codequest" && a["operation"] == "search")
}

fn rpc_response(
    service: &Arc<Service>,
    receipts: &mut cost::Session,
    request: &Value,
    compact: bool,
) -> Value {
    let result = (|| -> Result<Value> {
        match request["method"].as_str().unwrap_or("") {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","serverInfo":{"name":"CodeQuest","version":env!("CARGO_PKG_VERSION")},"instructions":AGENT_GUIDE,"capabilities":{"tools":{"listChanged":false},"resources":{},"prompts":{}}}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools":tools()})),
            "resources/list" => Ok(json!({"resources":[]})),
            "prompts/list" => Ok(
                json!({"prompts":[{"name":"investigate","description":"Bounded evidence-first investigation","arguments":[{"name":"question","required":true}]}]}),
            ),
            "prompts/get" if request["params"]["name"] == "investigate" => Ok(
                json!({"messages":[{"role":"user","content":{"type":"text","text":format!("{}\n\n{}",AGENT_GUIDE,request["params"]["arguments"]["question"].as_str().unwrap_or(""))}}]}),
            ),
            "tools/call" => {
                let p = &request["params"];
                let name = p["name"]
                    .as_str()
                    .ok_or_else(|| Error::new("MissingTool", "name is required"))?;
                match receipts.call(service, name, p["arguments"].clone(), compact) {
                    Ok(v) => Ok(
                        json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false}),
                    ),
                    Err(e) => Ok(
                        json!({"content":[{"type":"text","text":json!({"error":e.to_string()}).to_string()}],"isError":true}),
                    ),
                }
            }
            _ => Err(Error::new("MethodNotFound", "unsupported MCP method")),
        }
    })();
    match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":request["id"],"result":value}),
        Err(e) => {
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":e.to_string()}})
        }
    }
}

pub fn run(service: Arc<Service>, auto_refresh: bool, compact_default: bool) -> Result<()> {
    if auto_refresh {
        for project in service.store.list_projects()? {
            service.watch(&project.name)?;
        }
    }
    let output = Arc::new(Mutex::new(io::stdout()));
    let delivery = Arc::new(Mutex::new(cost::Session::default()));
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut threads: std::collections::VecDeque<std::thread::JoinHandle<()>> = Default::default();
    loop {
        let mut bytes = vec![];
        let n = reader
            .by_ref()
            .take(8 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(Error::new("FrameTooLarge", "MCP request exceeds 8 MiB"));
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                let mut out = output.lock().unwrap();
                writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}})
                )?;
                out.flush()?;
                continue;
            }
        };
        if request.get("id").is_none() {
            if request["method"] == "notifications/cancelled"
                && let Some(job) = request["params"]["job"].as_str()
            {
                let _ = service.job(job, true);
            }
            continue;
        }
        if parallel_navigation(&request) {
            // Source deliveries, receipt resets and writes are barriers. Pure
            // navigation may finish out of order, identified by JSON-RPC ID.
            if threads.len() >= 4 {
                let _ = threads.pop_front().unwrap().join();
            }
            let service = service.clone();
            let output = output.clone();
            let delivery = delivery.clone();
            threads.push_back(std::thread::spawn(move || {
                let mut local = cost::Session::default();
                let response = rpc_response(&service, &mut local, &request, compact_default);
                if response["result"]["isError"] != true {
                    delivery.lock().unwrap().merge_navigation(local);
                }
                let mut out = output.lock().unwrap();
                let _ = writeln!(out, "{response}");
                let _ = out.flush();
            }));
        } else {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
            let response = rpc_response(
                &service,
                &mut delivery.lock().unwrap(),
                &request,
                compact_default,
            );
            let mut out = output.lock().unwrap();
            writeln!(out, "{response}")?;
            out.flush()?;
        }
    }
    for thread in threads {
        let _ = thread.join();
    }
    service.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cq_core::{model::*, ports::ContentRepository};
    use std::collections::BTreeMap;

    pub(super) fn sample_service() -> (tempfile::TempDir, Arc<Service>) {
        sample_service_with_extras(0)
    }
    pub(super) fn sample_service_with_extras(extras: usize) -> (tempfile::TempDir, Arc<Service>) {
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(Service::open(&dir.path().join("store")).unwrap());
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let file_path = repo.join("lib.rs");
        let text = "pub fn leaf() {}\npub fn caller() { leaf(); }\n";
        std::fs::write(&file_path, text).unwrap();
        let f = service
            .store
            .content
            .freeze(&file_path, "test", 64 * 1024)
            .unwrap();
        let fid = f.id.clone();
        let span = |needle: &str| {
            let start = text.find(needle).unwrap() as u64;
            SourceAnchor {
                file_revision: fid.clone(),
                start,
                end: start + needle.len() as u64,
            }
        };
        let mut s = SemanticSnapshot::empty("mcp-demo".into(), NativeValue::new(&repo));
        s.sources = SourceSnapshot::seal(vec![f], InputBinding::PrePostChecked, vec![]);
        let pid = s.intern_provenance(Provenance {
            provider: "test".into(),
            version: "1".into(),
            authority: Authority::SemanticResolution,
            build_run: None,
            note: "mcp test".into(),
        });
        let leaf_s = span("leaf");
        let caller_s = span("caller");
        let call_pos = text.find("leaf();").unwrap() as u64;
        let call_s = SourceAnchor {
            file_revision: fid.clone(),
            start: call_pos,
            end: call_pos + "leaf".len() as u64,
        };
        for (id, name, src) in [
            ("leaf", "leaf", leaf_s.clone()),
            ("caller", "caller", caller_s.clone()),
        ] {
            s.entities.push(Entity {
                stable_handle: None,
                locations: Default::default(),
                id: id.into(),
                unit: "u".into(),
                provider_id: id.into(),
                kind: EntityKind::Function,
                name: name.into(),
                qualified_name: format!("demo::{name}"),
                source: Some(src.clone()),
                extent: Some(src),
                signature: format!("fn {name}()"),
                parent: None,
                external: false,
                exported: true,
                attributes: BTreeMap::new(),
                type_definitions: vec![],
                type_navigation: vec![],
                provenance: pid.clone(),
            });
        }
        s.relations.push(Relation {
            id: "r1".into(),
            from: "caller".into(),
            to: "leaf".into(),
            kind: RelationKind::Calls,
            source: Some(call_s.clone()),
            occurrence: Some("o1".into()),
            provenance: pid.clone(),
        });
        s.occurrences.push(Occurrence {
            locations: Default::default(),
            id: "o1".into(),
            unit: "u".into(),
            owner: Some("caller".into()),
            source: call_s,
            kind: RelationKind::Calls,
            declaration: Resolution::Resolved("leaf".into()),
            executable: Resolution::Resolved("leaf".into()),
            dispatch: Dispatch::Direct,
            evaluation: Evaluation::Immediate,
            arguments: vec![],
            argument_bindings: vec![],
            provenance: pid,
        });
        for i in 0..extras {
            let mut entity = s.entities[0].clone();
            entity.id = format!("needle-{i}");
            entity.provider_id = entity.id.clone();
            entity.unit = format!("config-{i}");
            entity.name = format!("needle_{i}");
            entity.qualified_name = format!("demo::{}", entity.name);
            entity.signature = format!("fn {}()", entity.name);
            s.entities.push(entity);
        }
        s.seal().unwrap();
        service.store.publish_paged_semantics(&s, None).unwrap();
        (dir, service)
    }

    #[test]
    fn mcp_tools_expose_compact_and_typescript() {
        let list = tools();
        let search = list.iter().find(|t| t["name"] == "search_graph").unwrap();
        assert!(search["inputSchema"]["properties"]["compact"].is_object());
        assert_eq!(
            search["inputSchema"]["properties"]["compact"]["default"],
            true
        );
        assert_eq!(
            search["inputSchema"]["properties"]["search_view"]["enum"],
            json!(["overview", "symbols"])
        );
        let idx = list
            .iter()
            .find(|t| t["name"] == "index_repository")
            .unwrap();
        let langs = idx["inputSchema"]["properties"]["language"]["enum"]
            .as_array()
            .unwrap();
        assert!(langs.iter().any(|v| v == "typescript"));
    }

    #[test]
    fn search_defaults_to_grouped_declarations_and_accepts_explicit_flat_full_output() {
        let (_dir, service) = sample_service();
        let page = call(
            &service,
            "search_graph",
            json!({"project":"mcp-demo","text":"leaf"}),
            true,
        )
        .unwrap();
        assert_eq!(page["items"][0]["kind"], "FileMatches");
        assert_eq!(page["items"][0]["symbols"][0]["name"], "leaf");
        assert!(page["items"][0]["symbols"][0].get("provenance").is_none());
        assert_eq!(page["total_kind"], "unknown");
        let flat = call(
            &service,
            "search_graph",
            json!({"project":"mcp-demo","text":"leaf","search_view":"symbols","compact":false}),
            true,
        )
        .unwrap();
        assert_eq!(flat["items"][0]["name"], "leaf");
        assert!(flat["items"][0]["provenance"].is_string());
        let scoped = call(
            &service,
            "search_graph",
            json!({"project":"mcp-demo","text":"leaf","kind":"Function"}),
            true,
        )
        .unwrap();
        assert_eq!(scoped["items"][0]["name"], "leaf");
        assert!(
            call(
                &service,
                "search_graph",
                json!({"project":"mcp-demo","text":"leaf","search_view":"typo"}),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn grouped_service_budget_includes_cursor_and_preserves_all_pages() {
        let (_dir, service) = sample_service_with_extras(80);
        let mut page = call(
            &service,
            "search_graph",
            json!({"project":"mcp-demo","text":"needle","limit":60,"max_bytes":8192}),
            true,
        )
        .unwrap();
        let mut ids = std::collections::BTreeSet::new();
        let mut count = 0;
        for _ in 0..100 {
            assert!(serde_json::to_vec(&page).unwrap().len() <= 8192);
            for group in page["items"].as_array().unwrap() {
                for entity in group["symbols"].as_array().unwrap() {
                    count += 1;
                    ids.insert(entity["id"].as_str().unwrap().to_owned());
                }
            }
            let Some(cursor) = page["next_cursor"].as_str().map(str::to_owned) else {
                break;
            };
            // A continuation without a new budget must not reserve the envelope again
            // from an already reduced budget on every page.
            assert_eq!(
                service
                    .store
                    .load_cursor(&cursor)
                    .unwrap()
                    .request
                    .max_bytes,
                8192
            );
            page = call(
                &service,
                "query_codequest",
                json!({"project":"mcp-demo","operation":"next","cursor":cursor}),
                true,
            )
            .unwrap();
        }
        assert!(page["next_cursor"].is_null());
        assert_eq!((count, ids.len()), (80, 80));
    }

    #[test]
    fn mcp_trace_path_dispatches_impact_and_respects_compact_default() {
        let (_dir, service) = sample_service();
        let imp = call(
            &service,
            "trace_path",
            json!({
                "project": "mcp-demo",
                "operation": "impact",
                "subject": "leaf",
                "depth": 2
            }),
            true,
        )
        .unwrap();
        let names: Vec<&str> = imp["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["entity"]["name"].as_str())
            .collect();
        assert!(
            names.contains(&"caller"),
            "trace_path(operation=impact) must return inbound caller; got {names:?}"
        );
        // Compact default omits provenance and includes relative file/line.
        let first = &imp["items"][0]["entity"];
        assert_eq!(first["file"], "lib.rs");
        assert!(first.get("provenance").is_none());

        // Explicit compact: false restores full provenance and raw source anchor.
        let full = call(
            &service,
            "query_codequest",
            json!({
                "project": "mcp-demo",
                "operation": "references",
                "subject": "leaf",
                "compact": false
            }),
            true,
        )
        .unwrap();
        assert!(full["items"][0]["provenance"].is_string());
        assert!(full["items"][0]["source"].is_object());
        assert_eq!(full["items"][0]["owner_name"], "demo::caller");

        // Compact default omits provenance and raw source anchor while preserving owner & file:line.
        let compact_refs = call(
            &service,
            "query_codequest",
            json!({
                "project": "mcp-demo",
                "operation": "references",
                "subject": "leaf"
            }),
            true,
        )
        .unwrap();
        assert!(compact_refs["items"][0].get("provenance").is_none());
        assert!(compact_refs["items"][0].get("source").is_none());
        assert_eq!(compact_refs["items"][0]["owner"], "demo::caller");
        assert_eq!(compact_refs["items"][0]["file"], "lib.rs");
        assert_eq!(compact_refs["items"][0]["line"], 2);
    }
}
