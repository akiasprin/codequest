//! Live input and producer revalidation before a persisted snapshot is reused.
use super::{
    Service,
    index::{IndexRequest, worker_path},
};
use cq_core::{Error, Result, model::*};
use std::{collections::BTreeMap, path::PathBuf, process::Command};
pub(super) fn fingerprint(r: &IndexRequest) -> Result<String> {
    let mut request = r.clone();
    request.capture_run = None;
    request.reuse_hir = false;
    let mut tools = BTreeMap::new();
    tools.insert(
        "codequest".to_owned(),
        // On Linux an atomic binary upgrade unlinks the running executable's
        // path. /proc/self/exe still opens the exact image that did this work.
        digest(&std::fs::read(if cfg!(target_os = "linux") {
            PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe()?
        })?),
    );
    let language = if r.language == "auto" {
        if r.root.join("Cargo.toml").exists() {
            "rust"
        } else if r.root.join("go.mod").exists() || r.root.join("go.work").exists() {
            "go"
        } else {
            "cpp"
        }
    } else {
        r.language.as_str()
    };
    let recorder = r
        .recorder
        .clone()
        .unwrap_or(std::env::current_exe()?.with_file_name("cq-rustc-recorder"));
    tools.insert("recorder".into(), digest(&std::fs::read(recorder)?));
    let provider = match language {
        "rust" => worker_path(r.hir.as_ref(), "CQ_RUST_HIR", "cq-rust-hir")?,
        "go" => {
            if let Some(p) = std::env::var_os("CQ_GO_TYPES") {
                PathBuf::from(p)
            } else {
                std::env::current_exe()?
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                    .ok_or_else(|| Error::new("MissingProvider", "Go producer installation root"))?
                    .join(".codequest-tools/go-types/bin/cq-go-types")
            }
        }
        _ => worker_path(None, "CQ_CLANG", "cq-clang")?,
    };
    tools.insert("producer".into(), digest(&std::fs::read(provider)?));
    if language == "go" {
        let source = std::env::current_exe()?
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.join("providers/go-types/main.go"));
        if let Some(p) = source.filter(|p| p.is_file()) {
            tools.insert("producer_source".into(), digest(&std::fs::read(p)?));
        }
    }
    for (tool, args) in if language == "rust" {
        vec![("rustc", vec!["-vV"]), ("cargo", vec!["-V"])]
    } else if language == "go" {
        vec![("go", vec!["version"])]
    } else {
        vec![("clang", vec!["--version"])]
    } {
        let mut command = Command::new(tool);
        command.args(args).current_dir(&r.root);
        let output = cq_core::process::output(
            command,
            &cq_core::control::CaptureControl::default(),
            10000,
            1024 * 1024,
        )?;
        if !output.status.success() {
            return Err(Error::new("ToolchainProbeFailed", tool));
        }
        tools.insert(tool.into(), digest(&output.stdout));
    }
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| !matches!(key.to_str(), Some("_" | "SHLVL" | "PWD" | "OLDPWD")))
        .map(|(k, v)| (NativeValue::new(k), NativeValue::new(v)))
        .collect();
    env.sort();
    let database = r.compdb.clone().or_else(|| {
        matches!(language, "c" | "cpp" | "c++").then(|| r.root.join("compile_commands.json"))
    });
    let database = database
        .map(|p| std::fs::read(p).map(|b| digest(&b)))
        .transpose()?;
    Ok(digest(&serde_json::to_vec(&(
        request, tools, env, database,
    ))?))
}
pub fn saved_request(store: &cq_store::LocalStore, r: &IndexRequest) -> Result<serde_json::Value> {
    let mut saved = r.clone();
    saved.capture_run = None;
    saved.reuse_hir = false;
    if saved.build_args.is_empty()
        && let Some(run) = &r.capture_run
    {
        use cq_core::ports::CaptureReportRepository;
        saved.build_args = store.load_report(run)?.1.plan.requested_args;
    }
    Ok(serde_json::to_value(&saved)?)
}
pub fn stage(
    store: &cq_store::LocalStore,
    r: &IndexRequest,
    snapshot: &SemanticSnapshot,
) -> Result<()> {
    if store.execution.is_some()
        && let Some(previous) = store.index_inputs(&snapshot.revision)?
    {
        if snapshot.analysis_request.as_ref() != Some(&previous.request) {
            return Err(Error::new(
                "InvalidCheckpoint",
                "same semantic revision has a different build request",
            ));
        }
        // Retain the fingerprint of the implementation that produced the
        // original immutable snapshot. A later compatible resume is recorded
        // by its execution, not by overwriting that earlier observation.
        return Ok(());
    }
    let frozen = store
        .execution
        .as_ref()
        .map(|e| cq_store::execution::Execution::request(&store.root, &e.id()))
        .transpose()?
        .and_then(|v| v["_legacy_fingerprint"].as_str().map(str::to_owned));
    let fingerprint = if let Some(frozen) = frozen {
        frozen
    } else {
        fingerprint(r)?
    };
    store.stage_index_inputs(
        &snapshot.revision,
        &fingerprint,
        snapshot.analysis_request.as_ref().ok_or_else(|| {
            Error::new(
                "MissingAnalysisRequest",
                "snapshot has no repeatable build selection",
            )
        })?,
    )
}
pub fn restore(service: &Service, r: &IndexRequest) -> Result<Option<SemanticSnapshot>> {
    let Some(head) = service.store.head(&r.name)? else {
        return Ok(None);
    };
    let Some(stamp) = service.store.index_inputs(&head)? else {
        return Ok(None);
    };
    if stamp.fingerprint != fingerprint(r)? {
        return Ok(None);
    }
    let snapshot = service.store.load_semantics(&r.name, Some(&head))?;
    if snapshot.working_tree_manifest != cq_core::source_tree::manifest(&r.root)? {
        return Ok(None);
    }
    // Build scripts, proc macros and cgo may read undeclared inputs. Rebuild these
    // scopes rather than asserting that a cached compiler context is current.
    if snapshot
        .units
        .iter()
        .any(|u| matches!(u.role, UnitRole::BuildScript | UnitRole::ProcMacro))
        || snapshot
            .gaps
            .iter()
            .any(|g| g.code == "CgoExternalInputs" || g.code == "UntrackedIncludeQueries")
    {
        return Ok(None);
    }
    for f in snapshot
        .sources
        .files
        .iter()
        .chain(&snapshot.analysis_inputs)
    {
        if std::fs::read(f.logical_path.path())
            .ok()
            .is_none_or(|bytes| BlobId::of(&bytes) != f.blob)
        {
            return Ok(None);
        }
    }
    for (path, stamp) in &snapshot.analysis_directories {
        if cq_core::source_tree::directory_digest(&path.path())
            .ok()
            .as_ref()
            != Some(stamp)
        {
            return Ok(None);
        }
    }
    if service.store.load_checkpoint(&r.name, &head)?.is_none() {
        return Ok(None);
    }
    if service.store.head(&r.name)?.as_deref() != Some(head.as_str()) {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

/// Revalidate inputs without decoding semantic graph/checkpoint bodies.
pub fn restore_paged(service: &Service, r: &IndexRequest) -> Result<Option<serde_json::Value>> {
    use cq_core::ports::facts::FactReader;
    use serde_json::json;
    let start = std::time::Instant::now();
    let Some(head) = service.store.head(&r.name)? else {
        return Ok(None);
    };
    let Some(stamp) = service.store.index_inputs(&head)? else {
        return Ok(None);
    };
    if stamp.fingerprint != fingerprint(r)? {
        return Ok(None);
    }
    let reader = service.store.open_facts(&r.name, Some(&head))?;
    // Failed/partial coverage must be retried, even when source bytes did not
    // change (a transient compiler failure can disappear on the next build).
    if reader.metadata()["production"]["partial"] == true {
        return Ok(None);
    }
    let mut old = BTreeMap::new();
    let mut after = None;
    loop {
        let b = reader.scan("all:working_tree", after.as_deref(), 256)?;
        for row in b.rows {
            old.insert(
                row.key.strip_prefix("working_tree:").unwrap().to_owned(),
                serde_json::from_value::<String>(row.value)?,
            );
        }
        if b.complete {
            break;
        }
        after = b.after;
    }
    if old != cq_core::source_tree::manifest(&r.root)? {
        return Ok(None);
    }
    for (family, field, values) in [
        ("unit", "role", vec!["BuildScript", "ProcMacro"]),
        (
            "gaps",
            "code",
            vec!["CgoExternalInputs", "UntrackedIncludeQueries"],
        ),
    ] {
        let mut after = None;
        loop {
            let b = reader.scan(&format!("all:{family}"), after.as_deref(), 256)?;
            if b.rows
                .iter()
                .any(|row| values.contains(&row.value[field].as_str().unwrap_or("")))
            {
                return Ok(None);
            }
            if b.complete {
                break;
            }
            after = b.after;
        }
    }
    let mut checked = 0;
    let mut bytes = 0;
    for family in ["file", "analysis_inputs"] {
        let mut after = None;
        loop {
            let b = reader.scan(&format!("all:{family}"), after.as_deref(), 256)?;
            for row in b.rows {
                let f: FileRevision = serde_json::from_value(row.value)?;
                let input = match std::fs::read(f.logical_path.path()) {
                    Ok(v) => v,
                    Err(_) => return Ok(None),
                };
                checked += 1;
                bytes += input.len();
                if BlobId::of(&input) != f.blob {
                    return Ok(None);
                }
            }
            if b.complete {
                break;
            }
            after = b.after;
        }
    }
    let mut after = None;
    loop {
        let b = reader.scan("all:analysis_directories", after.as_deref(), 256)?;
        for row in b.rows {
            let (path, stamp): (NativeValue, String) = serde_json::from_value(row.value)?;
            if cq_core::source_tree::directory_digest(&path.path())
                .ok()
                .as_ref()
                != Some(&stamp)
            {
                return Ok(None);
            }
        }
        if b.complete {
            break;
        }
        after = b.after;
    }
    if service.store.head(&r.name)?.as_deref() != Some(&head) {
        return Ok(None);
    }
    let repaired = service
        .store
        .ensure_paged_checkpoint(&service.store.paged_manifest(&r.name, &head)?)?;
    let mut result = reader.metadata().clone();
    for (key, value) in reader.metadata()["counts"].as_object().unwrap() {
        result[key] = value.clone();
    }
    result["update"] = json!({"mode":if repaired{"checkpoint_repaired"}else{"checkpoint_unchanged"},"new_build_observed":false,"fact_pages_written":0,"projection_rows_submitted":0,"files_checked":checked,"bytes_checked":bytes,"seconds":start.elapsed().as_secs_f64()});
    result["read_stats"] = json!(reader.stats());
    Ok(Some(result))
}
