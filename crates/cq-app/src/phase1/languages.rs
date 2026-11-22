use super::index::{IndexRequest, run_worker, worker_path};
use cq_core::{
    Result, analyst::languages::NativeAnalysisRequest, control::CaptureControl,
    model::SemanticSnapshot,
};
use cq_store::LocalStore;
use std::{path::PathBuf, process::Command};
pub(super) fn native_request(
    r: &IndexRequest,
    control: &CaptureControl,
    go: bool,
) -> Result<NativeAnalysisRequest> {
    let recorder = r
        .recorder
        .clone()
        .unwrap_or(std::env::current_exe()?.with_file_name("cq-rustc-recorder"));
    let provider = if go {
        if let Some(path) = std::env::var_os("CQ_GO_TYPES") {
            PathBuf::from(path)
        } else {
            let exe = std::env::current_exe()?;
            let root = exe
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .unwrap_or(std::path::Path::new("."));
            let source = root.join("providers/go-types/main.go");
            let binary = root.join(".codequest-tools/go-types/bin/cq-go-types");
            std::fs::create_dir_all(binary.parent().unwrap())?;
            if !binary.exists()
                || std::fs::metadata(&source)?.modified()?
                    > std::fs::metadata(&binary)?.modified()?
            {
                let mut c = Command::new("go");
                c.args(["build", "-o"]).arg(&binary).arg(source);
                run_worker(c, control, 120000)?;
            }
            binary
        }
    } else {
        worker_path(None, "CQ_CLANG", "cq-clang")?
    };
    Ok(NativeAnalysisRequest {
        allow_partial: r.allow_partial,
        root: r.root.clone(),
        language: r.language.clone(),
        build_args: r.build_args.clone(),
        compdb: r.compdb.clone(),
        provider,
        recorder,
    })
}
pub fn ts_provider_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CQ_TS_COMPILER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(5) {
            candidates.push(ancestor.join("providers/ts-compiler/index.mjs"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("providers/ts-compiler/index.mjs"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../providers/ts-compiler/index.mjs"),
    );
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    Err(cq_core::Error::new(
        "MissingProvider",
        "cannot locate providers/ts-compiler/index.mjs; set CQ_TS_COMPILER",
    ))
}
pub fn index_go(
    r: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
) -> Result<()> {
    cq_core::analyst::go::analyze(&native_request(r, control, true)?, store, control, s)
}
pub fn index_cpp(
    r: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
) -> Result<()> {
    cq_core::analyst::cpp::analyze(&native_request(r, control, false)?, store, control, s)
}
pub fn index_ts(
    r: &IndexRequest,
    store: &LocalStore,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
    tracked: bool,
) -> Result<()> {
    let provider = ts_provider_path()?;
    let req = NativeAnalysisRequest {
        allow_partial: r.allow_partial,
        root: r.root.clone(),
        language: "typescript".into(),
        build_args: r.build_args.clone(),
        compdb: None,
        provider,
        recorder: PathBuf::new(),
    };
    cq_core::analyst::ts::analyze(&req, store, control, s, tracked)
}
