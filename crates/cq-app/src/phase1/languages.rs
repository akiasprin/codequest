use super::index::{IndexRequest, run_worker, worker_path};
use cq_core::{
    Result, analyst::languages::NativeAnalysisRequest, control::CaptureControl,
    model::SemanticSnapshot,
};
use cq_store::LocalStore;
use std::{path::PathBuf, process::Command};
fn native_request(
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
