use cq_core::{Error, Result, control::CaptureControl, model::*};
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
    let mut progress = cq_core::progress::Progress::begin(&store.root, &request.name);
    let _bar = super::progress::IndexBar::start(&store.root, &request.name);
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
    let language = if request.language == "auto" {
        if request.root.join("Cargo.toml").exists() {
            "rust"
        } else if request.root.join("go.mod").exists() || request.root.join("go.work").exists() {
            "go"
        } else if request.compdb.is_some() || request.root.join("compile_commands.json").exists() {
            "cpp"
        } else {
            return Err(Error::new(
                "MissingBuildConfiguration",
                "provide Cargo.toml, go.mod or --compdb compile_commands.json",
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
        other => return Err(Error::new("UnsupportedLanguage", other)),
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
    if matches!(language, "c" | "cpp" | "c++") {
        cq_core::semantic::stream_ops::cpp_links(&mut snapshot, records)?;
    }
    cq_core::semantic::stream_ops::topology(&mut snapshot, records)?;
    if request.include_docs {
        progress.detail("documents", "关联文档", 0, None, "文档", 1);
        cq_core::semantic::stream_ops::documents(
            &mut snapshot,
            records,
            &store.content,
            1024 * 1024 * 1024,
        )?;
    }
    let after = cq_core::source_tree::manifest(&request.root)?;
    if before
        .iter()
        .any(|(path, digest)| after.get(path) != Some(digest))
        || after.keys().any(|path| {
            !before.contains_key(path)
                && std::path::Path::new(path).extension().is_some_and(|ext| {
                    ["rs", "go", "c", "cc", "cpp", "cxx", "h", "hpp"]
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
    let prepared = store.prepare_records(snapshot, records)?;
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
    let result = store.publish_analysis(&prepared);
    if result.is_ok() {
        progress.detail("complete", "索引完成", 1, Some(1), "提交", 1);
        progress.finish("complete");
    } else if matches!(&result, Err(Error::PublicationInProgress { .. })) { /* host owns live progress */
    } else {
        progress.finish("failed");
    }
    let revision = result?;
    let mut summary = super::service::index_summary(snapshot);
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
