//! Rust build observation and HIR semantic replay, behind storage ports.
use super::{RustBuildObserver, RustBuildRequest};
use crate::{Error, Result, analyst::BuildObserver, control::CaptureControl, model::*, ports::*};
use serde_json::Value;
use std::path::{Path, PathBuf};
pub struct RustAnalysisRequest {
    pub allow_partial: bool,
    pub root: PathBuf,
    pub build_args: Vec<String>,
    pub capture_run: Option<String>,
    pub provider: PathBuf,
    pub recorder: PathBuf,
}
pub trait RustWorkspace: AnalysisWorkspace + CaptureReportRepository {
    fn directory(&self) -> &Path;
    fn prepare_rust(
        &self,
        report: &CaptureReport,
        control: &CaptureControl,
    ) -> Result<super::project::RustProjectPlan>;
}
pub fn analyze(
    request: &RustAnalysisRequest,
    workspace: &dyn RustWorkspace,
    control: &CaptureControl,
    snapshot: &mut SemanticSnapshot,
) -> Result<()> {
    let args = if request.build_args.is_empty() {
        vec!["check".into(), "--workspace".into()]
    } else {
        request.build_args.clone()
    };
    let mut report = if let Some(run) = &request.capture_run {
        let (_, report) = workspace.load_report(run)?;
        if report.plan.project.path() != request.root {
            return Err(Error::new(
                "WrongCaptureProject",
                "capture root differs from requested repository",
            ));
        }
        report
    } else {
        let plan = RustBuildObserver.plan(RustBuildRequest {
            project: request.root.clone(),
            output: workspace.directory().to_owned(),
            cargo: "cargo".into(),
            recorder: request.recorder.clone(),
            args,
            mode: "exclusive".into(),
            reuse_target: None,
            extra_env_keys: vec![],
            budget: CaptureBudget::default(),
        })?;
        let journal = workspace.create_capture(&plan.run_id)?;
        let report = RustBuildObserver.observe(plan, journal, workspace.content(), control)?;
        workspace.save_report(&report)?;
        report
    };
    if !matches!(&report.seal,SealOutcome::Sealed(r) if r.integrity==JournalIntegrity::Sound)
        || report.units.is_empty()
    {
        return Err(Error::new(
            "CaptureIncomplete",
            format!(
                "cannot analyze unverified capture {}; inspect its report",
                report.run.id
            ),
        ));
    }
    if report.run.outcome != BuildOutcome::Succeeded {
        if !request.allow_partial {
            return Err(Error::new(
                "RustBuildFailed",
                "strict publication requires a successful build; capture evidence was retained",
            ));
        }
        snapshot.gaps.push(Gap::new("BuildFailedPartial",report.run.id.clone(),"build did not succeed; extracted static facts cover only the available partial compiler context"));
    }
    snapshot.gaps.extend(report.gaps.clone());
    snapshot.gaps.extend(report.snapshot.gaps.clone());
    if report.snapshot.binding == InputBinding::Changed {
        return Err(Error::new(
            "InputChangedDuringBuild",
            "capture retained pre-build bytes; cannot publish semantics as the observed build",
        ));
    }
    // Macro execution must replay a retained binary rather than a mutable target path.
    let macro_paths: Vec<_> = report
        .units
        .iter()
        .filter(|u| u.role == UnitRole::ProcMacro)
        .flat_map(|u| u.artifacts.iter())
        .flat_map(|id| report.artifacts.iter().filter(move |a| &a.id == id))
        .flat_map(|a| a.paths.iter())
        .filter(|p| {
            p.path()
                .extension()
                .is_some_and(|e| e == "so" || e == "dylib" || e == "dll")
        })
        .cloned()
        .collect();
    for path in macro_paths {
        if !report.snapshot.files.iter().any(|f| f.logical_path == path) {
            let file = workspace.content().freeze(
                &path.path(),
                "semantic_proc_macro_artifact",
                1024 * 1024 * 1024,
            )?;
            report.snapshot.files.push(file);
            snapshot.gaps.push(Gap::new("ProcMacroArtifactFrozenAfterBuild",path.display(),"binary frozen before semantic replay; compiler read bytes were not attested during the earlier build"));
        }
    }
    report.snapshot = SourceSnapshot::seal(
        report.snapshot.files.clone(),
        report.snapshot.binding.clone(),
        report.snapshot.gaps.clone(),
    );
    snapshot.sources = report.snapshot.clone();
    snapshot.units = report.units.clone();
    snapshot.build_dependencies = report.dependencies.clone();
    snapshot.builds.push(BuildObservation {
        id: report.run.id.clone(),
        language: Language::Rust,
        command: report.plan.build_tool.clone(),
        arguments: report
            .plan
            .effective_args
            .iter()
            .map(|s| NativeValue::from(s.as_str()))
            .collect(),
        cwd: report.plan.project.clone(),
        outcome: report.run.outcome.clone(),
        exit: report.run.tool_exit.clone(),
        events: report
            .invocations
            .iter()
            .flat_map(|i| {
                i.started
                    .iter()
                    .chain(i.process.iter())
                    .chain(i.finished.iter())
                    .chain(i.argument_files.iter())
                    .cloned()
            })
            .collect(),
        captured_report: Some(report.run.id.clone()),
        gaps: report.gaps.clone(),
    });
    let mut replay_report = report.clone();
    if report.run.outcome != BuildOutcome::Succeeded {
        let p = snapshot.production.get_or_insert_with(Default::default);
        p.selection_complete = false;
        p.partial = true;
        let mut safe=report.units.iter().filter(|unit| {
            unit.binding==UnitBinding::ObservedInvocation && unit.invocation.value().and_then(|id|report.invocations.iter().find(|inv|&inv.id==id))
                .and_then(|inv|inv.finished.as_ref()).is_some_and(|e|matches!(&e.event,Event::InvocationFinished{exit,..} if exit.code==Some(0)&&exit.spawn_error.is_none()))
        }).map(|u|u.id.clone()).collect::<std::collections::BTreeSet<_>>();
        loop {
            let invalid = report
                .dependencies
                .iter()
                .filter(|d| {
                    safe.contains(&d.consumer)
                        && d.producer
                            .value()
                            .is_none_or(|producer| !safe.contains(producer))
                })
                .map(|d| d.consumer.clone())
                .collect::<Vec<_>>();
            if invalid.is_empty() {
                break;
            }
            for id in invalid {
                safe.remove(&id);
            }
        }
        for unit in &report.units {
            if safe.contains(&unit.id) {
                continue;
            }
            crate::semantic::production::failed(
                snapshot,
                &unit.id,
                "Rust unit failed, was not observed successful, or depends on an unavailable unit; no stale facts carried forward",
            );
        }
        replay_report.units.retain(|u| safe.contains(&u.id));
        replay_report
            .dependencies
            .retain(|d| safe.contains(&d.consumer));
        if replay_report.units.is_empty() {
            return Ok(());
        }
    }
    for unit in &report.units {
        if unit.binding != UnitBinding::ObservedInvocation {
            continue;
        }
        if snapshot
            .production
            .as_ref()
            .is_some_and(|p| p.failed_units.contains_key(&unit.id))
        {
            continue;
        }
        let ctx = crate::analyst::rust::select_context(&report, &unit.id)?;
        snapshot.contexts.push(AnalysisContext {
            id: digest(unit.id.as_bytes()),
            entry_unit: unit.id.clone(),
            units: ctx.units,
            status: Support::Partial,
            gaps: ctx.gaps,
        });
    }
    let view = workspace.prepare_rust(&replay_report, control)?;
    let worker = request.provider.clone();
    let output = workspace
        .run_directory(&report.run.id)?
        .join("hir-export.json");
    super::session::export(&worker, &view.root, &output, control)?;
    let metadata = std::fs::metadata(&output)?;
    if metadata.len() > 1024 * 1024 * 1024 {
        return Err(Error::new(
            "ProviderOutputLimit",
            "HIR export exceeds 1 GiB",
        ));
    }
    let archive = workspace
        .semantic_records()
        .map(|_| crate::semantic::native_archive::Archive::open(&output))
        .transpose()?;
    let raw: Value = if let Some(a) = &archive {
        a.header()?
    } else {
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(&output)?))?
    };
    const PRODUCER_BASE: &str = "2850ecba80311bebd4cdaa9fedc5321533b5b1e7";
    if raw["base_commit"] != PRODUCER_BASE {
        return Err(Error::new(
            "UnidentifiedProvider",
            "Rust HIR base commit differs from the supported producer",
        ));
    }
    let patch = raw["patch_digest"]
        .as_str()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            Error::new(
                "UnidentifiedProvider",
                "producer extension digest must be SHA-256",
            )
        })?;
    let version = format!(
        "{}+{}",
        raw["base_commit"].as_str().unwrap_or("unknown"),
        patch
    );
    let blob = workspace
        .content()
        .freeze(&output, "native_export_evidence", 1024 * 1024 * 1024)?
        .blob;
    snapshot.providers.push(ProviderRecord {
        analysis: raw
            .get("incremental")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?,
        reused_from: None,
        name: "rust-analyzer-hir".into(),
        version: version.clone(),
        binary_digest: digest(&std::fs::read(worker)?),
        input_digest: view.digest,
        protocol: format!("codequest-hir-export/v1; raw={}", blob.digest),
    });
    let diagnostics = crate::semantic::diagnostics::read(&raw)?;
    if diagnostics.iter().any(|d| d.is_error()) {
        if !request.allow_partial {
            return Err(Error::new(
                "NativeAnalysisFailed",
                "Rust semantic replay reported errors; HEAD unchanged",
            ));
        }
        for unit in &replay_report.units {
            crate::semantic::production::failed(
                snapshot,
                &unit.id,
                "Rust semantic replay reported errors; scope unavailable",
            );
        }
        for d in diagnostics {
            snapshot
                .gaps
                .push(Gap::new("NativeDiagnostic", "rust-analyzer-hir", d.message));
        }
        return Ok(());
    }
    if let Some(sink) = workspace.semantic_records() {
        archive.as_ref().unwrap().normalize(
            snapshot,
            &view.mapping,
            workspace.content(),
            "rust-analyzer-hir",
            Language::Rust,
            &version,
            sink,
        )?;
    } else {
        crate::semantic::normalize::rust_hir(
            snapshot,
            &raw,
            &view.mapping,
            workspace.content(),
            &version,
        )?;
    }
    for (index, diagnostic) in raw["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let raw_path = diagnostic["path"].as_str().unwrap_or("rust-analyzer");
        let scope = view
            .mapping
            .iter()
            .find(|m| m.path == raw_path)
            .map(|m| m.file.logical_path.display())
            .unwrap_or_else(|| raw_path.to_owned());
        snapshot.gaps.push(Gap {
            code: diagnostic["code"]
                .as_str()
                .unwrap_or("NativeSyntaxDiagnostic")
                .into(),
            scope,
            message: diagnostic["message"]
                .as_str()
                .unwrap_or("native diagnostic")
                .into(),
            evidence: vec![EvidenceRef {
                run_id: report.run.id.clone(),
                source: format!("provider-export:{}", blob.digest),
                index: Some(index),
            }],
        });
    }
    Ok(())
}
