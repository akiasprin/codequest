//! Native Go and C/C++ analysts. All durable data access uses core ports.
use crate::{Error, Result, control::CaptureControl, model::*, ports::*};
use serde_json::Value;
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
    let archive = store
        .semantic_records()
        .map(|_| crate::semantic::native_archive::Archive::open(output))
        .transpose()?;
    let raw: Value = if let Some(a) = &archive {
        a.header()?
    } else {
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(output)?))?
    };
    let version = raw["version"].as_str().unwrap_or("native-v1");
    let blob = store
        .content()
        .freeze(output, "native_export_evidence", 1024 * 1024 * 1024)?
        .blob;
    s.providers.push(ProviderRecord {
        analysis: None,
        reused_from: input["analysis_reuse"].as_str().map(str::to_owned),
        name: provider.into(),
        version: version.into(),
        binary_digest: digest(&std::fs::read(worker)?),
        input_digest: digest(&serde_json::to_vec(input)?),
        protocol: format!("native-v1; raw={}", blob.digest),
    });
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
        return Ok(());
    }
    if let Some(sink) = store.semantic_records() {
        archive.as_ref().unwrap().normalize(
            s,
            mapping,
            store.content(),
            provider,
            language,
            version,
            sink,
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
    Ok(())
}
