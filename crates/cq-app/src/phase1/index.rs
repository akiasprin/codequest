use cq_core::{Error, Result, control::CaptureControl, model::*};
use cq_core::{execution::Stage, ports::AnalysisWorkspace};
use cq_store::LocalStore;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexRequest {
    #[serde(default)]
    pub capture_run: Option<String>,
    #[serde(default)]
    pub reuse_hir: bool,
    pub root: PathBuf,
    pub name: String,
    pub language: String,
    pub build_args: Vec<String>,
    pub compdb: Option<PathBuf>,
    pub include_docs: bool,
    #[serde(default)]
    pub allow_partial: bool,
    pub hir: Option<PathBuf>,
    pub recorder: Option<PathBuf>,
}
impl IndexRequest {
    pub fn from_options(o: &super::Options) -> Result<Self> {
        let root = o
            .path("--root")
            .or_else(|| o.path("--project"))
            .ok_or_else(|| Error::new("MissingRoot", "index needs --root DIR"))?
            .canonicalize()?;
        let name = o.text("--name").unwrap_or_else(|| {
            root.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let mut build_args = o
            .trailing
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if build_args.first().is_some_and(|s| s == "cargo") {
            build_args.remove(0);
        }
        Ok(Self {
            capture_run: o.text("--capture-run"),
            reuse_hir: o.yes("--reuse-hir"),
            root,
            name,
            language: o.text("--language").unwrap_or_else(|| "auto".into()),
            build_args,
            compdb: o.path("--compdb"),
            include_docs: !o.yes("--no-docs"),
            allow_partial: o.yes("--allow-partial"),
            hir: o.path("--hir"),
            recorder: o.path("--recorder"),
        })
    }
}
pub fn index_repository(
    request: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
) -> Result<serde_json::Value> {
    workflow(request, store, control, None, None, None)
}
fn tool_binding(
    request: &IndexRequest,
    control: &CaptureControl,
    expected: Option<&serde_json::Value>,
    needed: Option<Stage>,
) -> Result<serde_json::Value> {
    let language = if request.language == "auto" {
        if request.root.join("Cargo.toml").exists() {
            "rust"
        } else if request.root.join("go.mod").exists() || request.root.join("go.work").exists() {
            "go"
        } else {
            "cpp"
        }
    } else {
        &request.language
    };
    let need_provider = expected.is_none() || needed.is_some_and(|s| s <= Stage::Normalize);
    let need_recorder = expected.is_none() || needed.is_some_and(|s| s <= Stage::Capture);
    let (provider, recorder) = if need_provider || need_recorder {
        if language == "rust" {
            (
                worker_path(request.hir.as_ref(), "CQ_RUST_HIR", "cq-rust-hir")?,
                request
                    .recorder
                    .clone()
                    .unwrap_or(sibling("cq-rustc-recorder")?),
            )
        } else {
            let r = super::languages::native_request(request, control, language == "go")?;
            (r.provider, r.recorder)
        }
    } else {
        (PathBuf::new(), PathBuf::new())
    };
    let provider = if need_provider {
        digest(&std::fs::read(provider)?)
    } else {
        expected.unwrap()["provider"]
            .as_str()
            .ok_or_else(|| Error::new("InvalidCheckpoint", "provider binding"))?
            .to_owned()
    };
    let recorder = if need_recorder {
        digest(&std::fs::read(recorder)?)
    } else {
        expected.unwrap()["recorder"]
            .as_str()
            .ok_or_else(|| Error::new("InvalidCheckpoint", "recorder binding"))?
            .to_owned()
    };
    let semantic_env = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
        "RUSTC",
        "RUSTC_BOOTSTRAP",
        "CC",
        "CXX",
        "CPPFLAGS",
        "CFLAGS",
        "CXXFLAGS",
        "GOOS",
        "GOARCH",
        "GOFLAGS",
        "GOEXPERIMENT",
        "GOWORK",
        "CGO_ENABLED",
        "CGO_CFLAGS",
        "CGO_CXXFLAGS",
        "CGO_LDFLAGS",
    ]
    .into_iter()
    .map(|key| (key, std::env::var_os(key).map(NativeValue::new)))
    .collect::<std::collections::BTreeMap<_, _>>();
    Ok(
        serde_json::json!({"search_model":cq_core::query::embeddings::TokenVectors::configured_identity()?,"provider":provider,"recorder":recorder,"environment":semantic_env,"compdb":request.compdb.as_ref().map(|p|std::fs::read(p).map(|b|digest(&b))).transpose()?}),
    )
}
pub fn workflow(
    request: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
    run: Option<&str>,
    rerun: Option<Stage>,
    until: Option<Stage>,
) -> Result<serde_json::Value> {
    let mut normalized = request.clone();
    normalized.root = request.root.canonicalize()?;
    if let Some(path) = &normalized.compdb {
        normalized.compdb = Some(path.canonicalize()?);
    }
    if normalized
        .build_args
        .first()
        .is_some_and(|a| a == "cargo" || a == "go")
    {
        normalized.build_args.remove(0);
    }
    let request = &normalized;
    if rerun.zip(until).is_some_and(|(from, to)| to < from) {
        return Err(Error::new(
            "InvalidStageRange",
            "--until-stage must not precede the selected rerun stage",
        ));
    }
    let source = serde_json::to_value(cq_core::source_tree::manifest(&request.root)?)?;
    let plan = vec![
        Stage::Capture,
        Stage::Materialize,
        Stage::NativeExport,
        Stage::Archive,
        Stage::Normalize,
        Stage::Relations,
        Stage::Documents,
        Stage::Canonicalize,
        Stage::Statistics,
        Stage::Encode,
        Stage::Directories,
        Stage::AuditInventory,
        Stage::AuditPayloads,
        Stage::AuditDirectories,
        Stage::AuditScopes,
        Stage::GraphInventory,
        Stage::GraphPages,
        Stage::GraphFacts,
        Stage::Publish,
        Stage::Validate,
    ];
    if until.is_some_and(|s| !plan.contains(&s)) || rerun.is_some_and(|s| !plan.contains(&s)) {
        return Err(Error::new(
            "UnknownStage",
            "use run-status to list executable stages",
        ));
    }
    let saved = run
        .map(|id| cq_store::execution::Execution::request(&store.root, id))
        .transpose()?;
    let expected = saved.as_ref().map(|v| &v["_tools"]);
    let needed = if let Some(stage) = rerun {
        Some(stage)
    } else if let Some(id) = run {
        let status = cq_store::execution::Execution::status(&store.root, id)?;
        status["stages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["attempt"]["state"] != "completed")
            .map(|s| Stage::parse(s["stage"].as_str().unwrap()))
            .transpose()?
    } else {
        Some(Stage::Capture)
    };
    let binding = tool_binding(request, control, expected, needed)?;
    let mut saved_request = serde_json::to_value(request)?;
    saved_request["_tools"] = binding.clone();
    if run.is_none() {
        saved_request["_legacy_fingerprint"] =
            serde_json::json!(super::checkpoint::fingerprint(request)?);
    }
    if let Some(id) = run {
        let expected = cq_store::execution::Execution::request(&store.root, id)?["_tools"].clone();
        let mut actual = binding.clone();
        if expected.get("search_model").is_none() {
            // Development-era executions used the same native model but did
            // not record it here. Only a published, verifiable model label
            // permits their continuation; unknown bindings fail closed.
            let head = store.head(&request.name)?.ok_or_else(|| {
                Error::new(
                    "MissingExecutionModelBinding",
                    "legacy execution has no published search model; start a new execution",
                )
            })?;
            if store.paged_manifest(&request.name, &head)?.metadata["search_index"]["vector_model"]
                != actual["search_model"]["label"]
            {
                return Err(Error::new(
                    "ExecutionToolChanged",
                    "search model differs from the execution's published model; restore CQ_TOKEN_VECTORS or start a new execution",
                ));
            }
            actual.as_object_mut().unwrap().remove("search_model");
        }
        if expected != actual {
            return Err(Error::new(
                "ExecutionToolChanged",
                "provider, recorder, search model, compiler configuration or compilation database changed; start a new execution",
            ));
        }
    }
    let execution = std::sync::Arc::new(if let Some(id) = run {
        cq_store::execution::Execution::open(&store.root, id, &source, rerun, until)?
    } else {
        cq_store::execution::Execution::create(
            &store.root,
            &request.name,
            &source,
            saved_request,
            plan,
            until,
        )?
    });
    if until.is_some_and(|s| execution.completed(s)) {
        return Ok(
            serde_json::json!({"state":"paused","reason":"stage_already_complete","execution":execution.id(),"stage":until.unwrap().name()}),
        );
    }
    execution.attach_control(control);
    let id = execution.id();
    let scoped = store.execution_scope(execution.clone())?;
    let result = index_repository_inner(request, &scoped, control);
    if let Err(error) = &result
        && error.code() != "StagePaused"
        && !matches!(error, Error::PublicationInProgress { .. })
    {
        execution.record_error(error)?;
    }
    match result {
        Ok(mut value) => {
            value["execution"] = serde_json::json!(id);
            Ok(value)
        }
        Err(e) if e.code() == "StagePaused" => {
            cq_core::progress::finish(&store.root, &request.name, "paused");
            Ok(
                serde_json::json!({"state":"paused","execution":id,"continue_with":{"command":"resume","run":id},"detail":e.to_string()}),
            )
        }
        Err(Error::PublicationInProgress { project, revision }) => Ok(
            serde_json::json!({"state":"publication_in_progress","project":project,"revision":revision,"execution":id,"continue_with":{"command":"resume","run":id}}),
        ),
        Err(e) => Err(Error::new(
            "ExecutionFailed",
            format!(
                "execution {id}; {}; resume with cq resume --store {} --run {id}",
                e,
                store.root.display()
            ),
        )),
    }
}
fn index_repository_inner(
    request: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
) -> Result<serde_json::Value> {
    let mut progress = cq_core::progress::Progress::begin(&store.root, &request.name);
    let _bar = super::progress::IndexBar::start(&store.root, &request.name);
    if let Some(candidate) = store
        .execution
        .as_ref()
        .map(|e| e.candidate())
        .transpose()?
        .flatten()
    {
        return finish_candidate(
            request,
            store,
            control,
            &candidate.header,
            &candidate.revision,
            candidate.transport,
            &mut progress,
        );
    }
    if let Some(template) = store
        .execution
        .as_ref()
        .map(|e| e.candidate_template())
        .transpose()?
        .flatten()
    {
        let revision = store.resume_candidate_directories(&template)?;
        return finish_candidate(
            request,
            store,
            control,
            &template.header,
            &revision,
            template.transport,
            &mut progress,
        );
    }
    let started = std::time::Instant::now();
    let scoped = store.streaming_scope()?;
    let store = &scoped;
    let mut normalized = request.clone();
    normalized.root = request.root.canonicalize()?;
    if let Some(path) = &normalized.compdb {
        normalized.compdb = Some(path.canonicalize()?);
    }
    if normalized
        .build_args
        .first()
        .is_some_and(|a| a == "cargo" || a == "go")
    {
        normalized.build_args.remove(0);
    }
    let request = &normalized;
    let has_ts = !cq_core::analyst::ts::discover_tsconfigs(&request.root).is_empty();
    let language = if request.language == "auto" {
        if request.root.join("Cargo.toml").exists() {
            "rust"
        } else if request.root.join("go.mod").exists() || request.root.join("go.work").exists() {
            "go"
        } else if request.compdb.is_some() || request.root.join("compile_commands.json").exists() {
            "cpp"
        } else if has_ts {
            "typescript"
        } else {
            return Err(Error::new(
                "MissingBuildConfiguration",
                "provide Cargo.toml, go.mod, tsconfig.json or --compdb compile_commands.json",
            ));
        }
    } else {
        request.language.as_str()
    };
    let before = cq_core::source_tree::manifest(&request.root)?;
    let mut snapshot =
        SemanticSnapshot::empty(request.name.clone(), NativeValue::new(&request.root));
    snapshot.production = Some(Default::default());
    snapshot.previous = store.head(&request.name)?;
    progress.detail("native", "采集编译并分析", 0, None, "编译任务", 1);
    match language {
        "rust" => rust(request, store, control, &mut snapshot)?,
        "go" => super::service::index_go(request, store, control, &mut snapshot)?,
        "c" | "cpp" | "c++" => super::service::index_cpp(request, store, control, &mut snapshot)?,
        "ts" | "tsx" | "typescript" => {
            super::service::index_ts(request, store, control, &mut snapshot, true)?
        }
        other => return Err(Error::new("UnsupportedLanguage", other)),
    }
    // Polyglot auto-indexing: when a Rust/Go/C++ repo also contains frontend
    // tsconfig.json project(s), analyze TypeScript/TSX into the same snapshot.
    if request.language == "auto"
        && !matches!(language, "ts" | "tsx" | "typescript")
        && has_ts
        && let Err(err) = super::service::index_ts(request, store, control, &mut snapshot, false)
    {
        snapshot.gaps.push(cq_core::model::Gap::new(
            "TypeScriptSecondaryUnavailable",
            "ts-compiler",
            format!("frontend TypeScript indexing skipped: {err}"),
        ));
    }
    let native_ms = started.elapsed().as_millis() as u64;
    release_scratch_memory();
    if control.is_cancelled() {
        return Err(Error::new(
            "Cancelled",
            "index cancelled before publication",
        ));
    }
    let records = store.analysis_records().unwrap();
    progress.detail("relations", "整理跨文件关系", 0, None, "关系", 1);
    if !store.start_stage(Stage::Relations, &mut snapshot)?.reused {
        if matches!(language, "c" | "cpp" | "c++") {
            cq_core::semantic::stream_ops::cpp_links(&mut snapshot, records)?;
        }
        cq_core::semantic::stream_ops::topology(&mut snapshot, records)?;
        store.finish_stage(Stage::Relations, &snapshot, serde_json::Value::Null)?;
    }
    if !store.start_stage(Stage::Documents, &mut snapshot)?.reused {
        if request.include_docs {
            progress.detail("documents", "关联文档", 0, None, "文档", 1);
            cq_core::semantic::stream_ops::documents(
                &mut snapshot,
                records,
                &store.content,
                1024 * 1024 * 1024,
            )?;
        }
        store.finish_stage(Stage::Documents, &snapshot, serde_json::Value::Null)?;
    }
    let after = cq_core::source_tree::manifest(&request.root)?;
    if before
        .iter()
        .any(|(path, digest)| after.get(path) != Some(digest))
        || after.keys().any(|path| {
            !before.contains_key(path)
                && std::path::Path::new(path).extension().is_some_and(|ext| {
                    [
                        "rs", "go", "c", "cc", "cpp", "cxx", "h", "hpp", "ts", "tsx", "mts", "cts",
                    ]
                    .iter()
                    .any(|e| ext == *e)
                })
                && !snapshot
                    .sources
                    .files
                    .iter()
                    .any(|f| f.logical_path.display() == *path)
        })
    {
        return Err(Error::new(
            "InputChangedDuringAnalysis",
            "working tree changed outside the captured semantic inputs",
        ));
    }
    snapshot.working_tree_manifest = after;
    let inputs: std::collections::BTreeMap<_, _> = snapshot
        .analysis_inputs
        .drain(..)
        .map(|f| (f.logical_path.clone(), f))
        .collect();
    snapshot.analysis_inputs = inputs.into_values().collect();
    snapshot.analysis_request = Some(super::checkpoint::saved_request(store, request)?);
    snapshot
        .production
        .as_mut()
        .unwrap()
        .stage_ms
        .insert("capture_and_native_normalization".into(), native_ms);
    let transport = records.stats();
    progress.detail(
        "prepare",
        "准备事实分片",
        0,
        transport["records"].as_u64(),
        "事实",
        1,
    );
    let entry = store.start_stage(Stage::Canonicalize, &mut snapshot)?;
    let prepared = if entry.reused {
        cq_store::analysis::Prepared::restore(
            store,
            &PathBuf::from(
                entry.cursor["spool"]
                    .as_str()
                    .ok_or_else(|| Error::new("MissingStageArtifact", "canonical spool"))?,
            ),
        )?
    } else {
        let path = store.stage_file(Stage::Canonicalize, "spool")?.unwrap();
        let prepared = if path.join("seal.json").exists() {
            let p = cq_store::analysis::Prepared::restore(store, &path)?;
            if p.header.project != snapshot.project
                || p.header.root != snapshot.root
                || p.header.previous != snapshot.previous
            {
                return Err(Error::new(
                    "InvalidStageArtifact",
                    "canonical spool binding",
                ));
            }
            p
        } else {
            let p = store.prepare_records(snapshot, records)?;
            p.retain(&path)?;
            p
        };
        store.finish_stage(
            Stage::Canonicalize,
            &prepared.header,
            serde_json::json!({"spool":path}),
        )?;
        prepared
    };
    let scoped = scoped.finish_streaming();
    let store = &scoped;
    release_scratch_memory();
    let snapshot = &prepared.header;
    if control.is_cancelled() {
        return Err(Error::new(
            "Cancelled",
            "index cancelled before publication",
        ));
    }
    super::checkpoint::stage(store, request, snapshot)?;
    #[cfg(debug_assertions)]
    {
        let hooks = store.root.join("format-2/test-hooks");
        if hooks.join("before-publication").is_file() {
            std::fs::write(hooks.join("ready-before-publication"), b"ready")?;
            while !hooks.join("release-before-publication").is_file() {
                if control.is_cancelled() {
                    return Err(Error::new("Cancelled", "cancelled before publication"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    let candidate = store.encode_analysis(&prepared)?;
    if let Some(e) = &store.execution {
        e.save_candidate(&prepared.header, &candidate, transport.clone())?;
    }
    finish_candidate(
        request,
        store,
        control,
        &prepared.header,
        &candidate,
        transport,
        &mut progress,
    )
}
fn finish_candidate(
    request: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
    snapshot: &SemanticSnapshot,
    candidate: &cq_store::facts::writer::Revision,
    transport: serde_json::Value,
    progress: &mut cq_core::progress::Progress,
) -> Result<serde_json::Value> {
    let mut saved_header = snapshot.clone();

    for stage in [
        Stage::AuditInventory,
        Stage::AuditPayloads,
        Stage::AuditDirectories,
        Stage::AuditScopes,
        Stage::GraphInventory,
        Stage::GraphPages,
        Stage::GraphFacts,
    ] {
        if !store.start_stage(stage, &mut saved_header)?.reused {
            let task = store.execution.as_ref().unwrap().task(stage)?;
            let receipt = cq_store::runtime::Client::new(&store.root)?.stage_controlled(
                candidate,
                stage,
                &task,
                Some(control),
            )?;
            store.finish_stage(stage, snapshot, receipt)?;
        }
    }
    let publication = store.start_stage(Stage::Publish, &mut saved_header)?;
    let result = if publication.reused {
        Ok(candidate.clone())
    } else {
        let r = store.publish_candidate(candidate)?;
        store.finish_stage(
            Stage::Publish,
            snapshot,
            serde_json::json!({"revision":r.revision}),
        )?;
        Ok(r)
    };
    if result.is_ok() {
        progress.detail("complete", "索引完成", 1, Some(1), "提交", 1);
        progress.finish("complete");
    } else if matches!(&result, Err(Error::PublicationInProgress { .. })) { /* host owns live progress */
    } else {
        progress.finish("failed");
    }
    let revision = result?;
    if !store
        .start_stage(Stage::Validate, &mut saved_header)?
        .reused
    {
        let status = store.resolve_publication(&request.name, &revision.revision)?;
        if status.outcome != cq_core::ports::facts::CommitOutcome::Committed {
            return Err(Error::new("ValidationFailed", "candidate is not committed"));
        }
        let view = store.open_facts(&request.name, Some(&revision.revision))?;
        use cq_core::ports::facts::FactReader;
        let _ = view.count("all:entity")?;
        store.finish_stage(
            Stage::Validate,
            snapshot,
            serde_json::json!({"revision":revision.revision,"query":"all:entity"}),
        )?;
    }
    let mut summary = super::service::index_summary(snapshot);
    summary["state"] = serde_json::json!("complete");
    for (key, value) in revision.metadata["counts"].as_object().unwrap() {
        summary[key] = value.clone();
    }
    summary["transfer"] = revision.metadata["transfer"].clone();
    summary["analysis_transport"] = transport;
    if let Some(rejections) = revision.metadata.get("storage_rejections") {
        summary["state"] = serde_json::json!("complete_with_warnings");
        summary["storage_rejections"] = rejections.clone();
    }
    Ok(summary)
}
fn release_scratch_memory() {
    // Native export/catalogue processing allocates many short-lived records.
    // Return free GNU allocator arenas before the next large phase competes
    // with them for the file cache. Live facts and frontend state stay owned.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}
fn sibling(name: &str) -> Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(name))
}
pub fn worker_path(explicit: Option<&PathBuf>, env: &str, name: &str) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.clone());
    }
    if let Some(p) = std::env::var_os(env) {
        return Ok(PathBuf::from(p));
    }
    let sibling = sibling(name)?;
    if sibling.is_file() {
        return Ok(sibling);
    }
    let exe = std::env::current_exe()?;
    let product = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));
    let tool = product.join(".codequest-tools/rust-hir/bin").join(name);
    if tool.is_file() {
        return Ok(tool);
    }
    Err(Error::new(
        "MissingProvider",
        format!("{name} unavailable; run scripts/setup_rust_hir.py or set {env}"),
    ))
}
fn rust(
    request: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
    snapshot: &mut SemanticSnapshot,
) -> Result<()> {
    let provider = worker_path(request.hir.as_ref(), "CQ_RUST_HIR", "cq-rust-hir")?;
    let recorder = request
        .recorder
        .clone()
        .unwrap_or(sibling("cq-rustc-recorder")?);
    cq_core::analyst::rust::semantic::analyze(
        &cq_core::analyst::rust::semantic::RustAnalysisRequest {
            root: request.root.clone(),
            build_args: request.build_args.clone(),
            capture_run: request.capture_run.clone(),
            reuse_hir: request.reuse_hir,
            provider,
            recorder,
            allow_partial: request.allow_partial,
        },
        store,
        control,
        snapshot,
    )
}
pub fn run_worker(command: Command, control: &CaptureControl, timeout_ms: u64) -> Result<()> {
    let output = cq_core::process::output(command, control, timeout_ms, 8 * 1024 * 1024)?;
    if !output.status.success() {
        return Err(Error::new(
            "ProviderFailed",
            format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }
    Ok(())
}
