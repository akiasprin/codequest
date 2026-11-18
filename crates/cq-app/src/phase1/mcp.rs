//! Newline-delimited MCP JSON-RPC over stdio. Stdout is reserved for protocol.
use super::{Service, index::IndexRequest};
use cq_core::{Error, Result};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    sync::{Arc, Mutex},
};
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
            "Search symbols with BM25 or offline semantic ranking; results retain build context.",
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
        properties["exact_semantic"] = json!({"type":"boolean","description":"Scan all persisted vectors instead of approximate locality-sensitive candidates"});
        properties["explicit_job"] = json!({"type":"boolean","description":"Allow an explicitly requested generation/export analysis job"});
        properties["authority"] = json!({"enum":["SemanticResolution","RuleDerived","AuthorStatement","Hypothesis","RuntimeObservation"],"description":"Filter returned relationship/search evidence; symbol identity anchors remain available"});
        properties["mode"] = json!({"enum":["calls","data_flow","types"],"description":"Static calls or positional argument-to-parameter contribution; no SSA/runtime claim"});
        if name == "query_codequest" {
            properties["operation"] = json!({"type":"string","description":"Query operation or next with an opaque cursor; retains original revision, scope and filters"});
        }
        if name == "index_repository" {
            properties["root"] =
                json!({"type":"string","description":"Absolute local repository path"});
            properties["name"] = json!({"type":"string"});
            properties["language"] = json!({"enum":["auto","rust","go","c","cpp"]});
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
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":true}}));
    }
    tools
}
fn call(s: &Arc<Service>, name: &str, mut a: Value) -> Result<Value> {
    if name == "list_projects" {
        return Ok(json!(s.store.list_projects()?));
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
            if a["target"].is_string() {
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
pub fn run(service: Arc<Service>, auto_refresh: bool) -> Result<()> {
    if auto_refresh {
        for project in service.store.list_projects()? {
            service.watch(&project.name)?;
        }
    }
    let output = Arc::new(Mutex::new(io::stdout()));
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut threads = vec![];
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
        let Some(id) = request.get("id").cloned() else {
            if request["method"] == "notifications/cancelled"
                && let Some(job) = request["params"]["job"].as_str()
            {
                let _ = service.job(job, true);
            }
            continue;
        };
        let service = service.clone();
        let output = output.clone();
        threads.push(std::thread::spawn(move||{let result=(||->Result<Value>{match request["method"].as_str().unwrap_or(""){
            "initialize"=>Ok(json!({"protocolVersion":"2024-11-05","serverInfo":{"name":"CodeQuest","version":env!("CARGO_PKG_VERSION")},"capabilities":{"tools":{"listChanged":false},"resources":{},"prompts":{}}})),
            "ping"=>Ok(json!({})),"tools/list"=>Ok(json!({"tools":tools()})),"resources/list"=>Ok(json!({"resources":[]})),"prompts/list"=>Ok(json!({"prompts":[]})),
            "tools/call"=>{let params=&request["params"];let name=params["name"].as_str().ok_or_else(||Error::new("MissingTool","name is required"))?;match call(&service,name,params["arguments"].clone()){Ok(v)=>Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false})),Err(e)=>Ok(json!({"content":[{"type":"text","text":json!({"error":e.to_string()}).to_string()}],"isError":true}))}},
            _=>Err(Error::new("MethodNotFound","unsupported MCP method")),
        }})();let response=match result{Ok(value)=>json!({"jsonrpc":"2.0","id":id,"result":value}),Err(e)=>json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":e.to_string()}})};let mut out=output.lock().unwrap();let _=writeln!(out,"{response}");let _=out.flush();}));
        if threads.len() >= 64 {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
    for thread in threads {
        let _ = thread.join();
    }
    service.shutdown();
    Ok(())
}
