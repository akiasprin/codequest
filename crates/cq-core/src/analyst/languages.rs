//! Native Go and C/C++ analysts. All durable data access uses core ports.
use crate::execution::Stage;
use crate::{Error, Result, control::CaptureControl, model::*, ports::*};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
#[derive(Clone, Debug)]
pub struct NativeAnalysisRequest {
    pub allow_partial: bool,
    pub root: PathBuf,
    pub language: String,
    pub build_args: Vec<String>,
    pub compdb: Option<PathBuf>,
    pub provider: PathBuf,
    pub recorder: PathBuf,
}
pub(super) fn run_worker(
    command: Command,
    control: &CaptureControl,
    timeout_ms: u64,
) -> Result<()> {
    let output = crate::process::output(command, control, timeout_ms, 8 * 1024 * 1024)?;
    if !output.status.success() {
        return Err(Error::new(
            "ProviderFailed",
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}
pub(super) fn attach(c: &mut Command, run: &str, j: &dyn CaptureJournal) {
    c.env(cq_capture_protocol::ENDPOINT_ENV, j.endpoint())
        .env(cq_capture_protocol::RUN_ENV, run);
}
pub(super) fn frozen(
    store: &dyn AnalysisWorkspace,
    path: &Path,
    unit: &str,
    root: &Path,
) -> Result<MaterializedInput> {
    let file = store.content().freeze(
        path,
        "compiler_selected_input_prepost_checked",
        1024 * 1024 * 1024,
    )?;
    store.materialize_input(&file, root)?;
    Ok(MaterializedInput {
        path: path.to_string_lossy().into_owned(),
        file,
        unit: unit.into(),
    })
}
pub(super) fn postcheck(mapping: &[MaterializedInput]) -> Result<()> {
    for m in mapping {
        if BlobId::of(&std::fs::read(m.file.logical_path.path())?) != m.file.blob {
            return Err(Error::new("InputChangedDuringBuild", m.path.clone()));
        }
    }
    Ok(())
}
pub(super) struct NativeExport<'a> {
    pub allow_partial: bool,
    pub worker: &'a Path,
    pub output: &'a Path,
    pub input: &'a Value,
    pub mapping: &'a [MaterializedInput],
    pub provider: &'a str,
    pub language: Language,
}
pub(super) fn consume(
    s: &mut SemanticSnapshot,
    store: &dyn AnalysisWorkspace,
    export: NativeExport<'_>,
) -> Result<()> {
    consume_impl(s, store, export, None, true)
}
pub(super) fn consume_with_tracking(
    s: &mut SemanticSnapshot,
    store: &dyn AnalysisWorkspace,
    export: NativeExport<'_>,
    tracked: bool,
) -> Result<()> {
    consume_impl(s, store, export, None, tracked)
}
pub(super) fn consume_untracked(
    s: &mut SemanticSnapshot,
    store: &dyn AnalysisWorkspace,
    export: NativeExport<'_>,
    archive: crate::semantic::native_archive::Archive,
) -> Result<()> {
    consume_impl(s, store, export, Some(archive), false)
}
fn consume_impl(
    s: &mut SemanticSnapshot,
    store: &dyn AnalysisWorkspace,
    export: NativeExport<'_>,
    loaded: Option<crate::semantic::native_archive::Archive>,
    tracked: bool,
) -> Result<()> {
    let NativeExport {
        allow_partial,
        worker,
        output,
        input,
        mapping,
        provider,
        language,
    } = export;
    if std::fs::metadata(output)?.len() > 1024 * 1024 * 1024 {
        return Err(Error::new(
            "ProviderOutputLimit",
            "native export exceeds 1 GiB",
        ));
    }
    let archived = if tracked {
        store.start_stage(Stage::Archive, s)?
    } else {
        Default::default()
    };
    let archive = if loaded.is_some() {
        loaded
    } else if store.semantic_records().is_some() {
        if archived.reused {
            Some(crate::semantic::native_archive::Archive::restore(
                Path::new(
                    archived.cursor["archive"]
                        .as_str()
                        .ok_or_else(|| Error::new("MissingStageArtifact", "native archive"))?,
                ),
            )?)
        } else {
            let a = crate::semantic::native_archive::Archive::open(output)?;
            if let Some(path) = store.stage_file(Stage::Archive, "archive")? {
                a.retain(&path)?;
                store.finish_stage(
                    Stage::Archive,
                    s,
                    json!({"archive":path,"files":[path.join("rows"),path.join("index.json")]}),
                )?;
            }
            Some(a)
        }
    } else {
        None
    };
    let raw: Value = if let Some(a) = &archive {
        a.header()?
    } else {
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(output)?))?
    };
    let normalized = if tracked {
        store.start_stage(Stage::Normalize, s)?
    } else {
        Default::default()
    };
    if normalized.reused {
        return Ok(());
    }
    let version = raw["version"].as_str().unwrap_or("native-v1");
    let blob = store
        .content()
        .freeze_evidence(output, "native_export_evidence", 1024 * 1024 * 1024)?
        .blob;
    if normalized.cursor.is_null() {
        s.providers.push(ProviderRecord {
            analysis: None,
            reused_from: input["analysis_reuse"].as_str().map(str::to_owned),
            name: provider.into(),
            version: version.into(),
            binary_digest: digest(&std::fs::read(worker)?),
            input_digest: digest(&serde_json::to_vec(input)?),
            protocol: format!("native-v1; raw={}", blob.digest),
        });
    }
    let diagnostics = crate::semantic::diagnostics::read(&raw)?;
    if diagnostics.iter().any(|d| d.is_error()) {
        if !allow_partial {
            return Err(Error::new(
                "NativeAnalysisFailed",
                format!("{provider} replay reported errors; refusing strict publication"),
            ));
        }
        for unit in mapping
            .iter()
            .map(|m| m.unit.as_str())
            .collect::<std::collections::BTreeSet<_>>()
        {
            crate::semantic::production::failed(
                s,
                unit,
                "native semantic replay failed; old scope facts cannot be retained",
            );
        }
        for d in diagnostics {
            s.gaps
                .push(Gap::new("NativeDiagnostic", provider, d.message));
        }
        if tracked {
            store.finish_stage(
                Stage::Normalize,
                s,
                json!({"state":"unavailable","reason":"native diagnostics"}),
            )?;
        }
        return Ok(());
    }
    if let Some(sink) = store.semantic_records() {
        archive.as_ref().unwrap().normalize_policy(
            s,
            mapping,
            store.content(),
            provider,
            language,
            version,
            sink,
            tracked,
        )?;
    } else {
        crate::semantic::normalize::native_graph(
            s,
            &raw,
            mapping,
            store.content(),
            provider,
            language,
            version,
        )?;
    }
    for d in diagnostics {
        s.gaps
            .push(Gap::new("NativeDiagnostic", provider, d.message));
    }
    if tracked {
        store.finish_stage(Stage::Normalize, s, Value::Null)?;
    }
    Ok(())
}
