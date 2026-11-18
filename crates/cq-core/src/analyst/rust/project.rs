use crate::{Error, Result, model::*, ports::ContentReader};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
pub struct RustProjectPlan {
    pub root: PathBuf,
    pub mapping: Vec<MaterializedInput>,
    pub digest: String,
    pub manifest: Vec<u8>,
    pub lease: Option<Box<dyn crate::ports::AnalysisLease>>,
}
pub fn plan(
    report: &CaptureReport,
    root: PathBuf,
    content: &dyn ContentReader,
) -> Result<RustProjectPlan> {
    let mut units: Vec<_> = report
        .units
        .iter()
        .filter(|u| u.binding == UnitBinding::ObservedInvocation)
        .collect();
    let project = report.plan.project.path();
    let logical: std::collections::HashMap<_, _> = units
        .iter()
        .map(|u| {
            Ok((
                u.id.clone(),
                crate::semantic::identity::unit_key(u, &report.plan.project.display())?,
            ))
        })
        .collect::<Result<_>>()?;
    units.sort_by_key(|u| logical[&u.id].clone());
    let mut mapping = vec![];
    let mut crates = vec![];
    let destination = |base: &Path, path: &Path| -> PathBuf {
        if let Ok(relative) = path.strip_prefix(&project) {
            base.join("workspace").join(relative)
        } else if let Ok(relative) = path.strip_prefix(report.plan.target.path()) {
            base.join("build-output").join(relative)
        } else {
            base.join("external")
                .join(path.strip_prefix("/").unwrap_or(path))
        }
    };
    for unit in &units {
        let base = root.join("units").join(&logical[&unit.id]);
        let mut paths = BTreeMap::new();
        // Use this invocation's emitted dep-info when available. This is a
        // compiler-reported input hint; it does not upgrade build-read proof.
        let selected = unit
            .invocation
            .value()
            .and_then(|id| report.invocations.iter().find(|i| &i.id == id))
            .and_then(|inv| {
                let args = super::args::inspect(inv);
                let dep =
                    args.out_dir
                        .as_ref()?
                        .join(format!("{}{}.d", args.name.as_ref()?, args.extra));
                let file = report
                    .snapshot
                    .files
                    .iter()
                    .find(|f| f.logical_path.path() == dep)?;
                let bytes = content.read_blob(&file.blob).ok()?;
                let inputs = super::inputs::dep_inputs(&bytes)?;
                Some(
                    inputs
                        .into_iter()
                        .map(|p| super::args::joined(&args.cwd, &NativeValue(p).path()))
                        .collect::<std::collections::BTreeSet<_>>(),
                )
            });
        for file in &report.snapshot.files {
            if selected
                .as_ref()
                .is_some_and(|paths| !paths.contains(&file.logical_path.path()))
            {
                continue;
            }
            let path = file.logical_path.path();
            if path.extension().is_some_and(|e| e == "d") {
                continue;
            }
            let target = destination(&base, &path);
            paths.insert(path, target.clone());
            mapping.push(MaterializedInput {
                path: target.to_string_lossy().into_owned(),
                file: file.clone(),
                unit: unit.id.clone(),
            });
        }
        let raw_root = unit
            .root
            .value()
            .ok_or_else(|| Error::new("MissingRoot", &unit.id))?
            .path();
        let module = paths
            .get(&raw_root)
            .ok_or_else(|| Error::new("MissingContent", raw_root.display().to_string()))?;
        let config = unit
            .configuration
            .rust()
            .ok_or_else(|| Error::new("InvalidConfiguration", "not Rust"))?;
        let mut cfg = config.compiler_cfg.value().cloned().ok_or_else(|| {
            Error::new(
                "MissingCompilerConfiguration",
                format!(
                    "compiler cfg not observed for unit {}; replay would guess",
                    unit.id
                ),
            )
        })?;
        cfg.extend(config.explicit_cfg.clone());
        if config.test {
            cfg.push("test".into());
        }
        cfg.sort();
        cfg.dedup();
        let mut env = BTreeMap::new();
        if let Some(inv) = unit
            .invocation
            .value()
            .and_then(|id| report.invocations.iter().find(|i| &i.id == id))
            .and_then(|i| i.started.as_ref())
            && let Event::InvocationStarted { environment, .. } = &inv.event
        {
            for e in environment {
                if let (Some(key), Some(value)) = (e.key.text(), e.value.text()) {
                    let value = if ["OUT_DIR", "CARGO_MANIFEST_DIR"].contains(&key) {
                        destination(&base, Path::new(value))
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        value.into()
                    };
                    env.insert(key.to_owned(), value);
                }
            }
        }
        let deps: Vec<_> = report
            .dependencies
            .iter()
            .filter(|d| d.consumer == unit.id)
            .filter_map(|d| {
                let target = d.producer.value()?;
                let index = units.iter().position(|u| &u.id == target)?;
                Some(serde_json::json!({"crate":index,"name":d.alias}))
            })
            .collect();
        let dylib = unit
            .artifacts
            .iter()
            .flat_map(|id| report.artifacts.iter().filter(move |a| &a.id == id))
            .flat_map(|a| &a.paths)
            .find(|p| {
                p.path()
                    .extension()
                    .is_some_and(|s| s == "so" || s == "dylib" || s == "dll")
            })
            .map(|p| -> Result<String> {
                let file = report
                    .snapshot
                    .files
                    .iter()
                    .find(|f| f.logical_path == *p)
                    .ok_or_else(|| Error::new("MissingMacroArtifact", p.display()))?;
                let target = base
                    .join("artifacts")
                    .join(&file.id)
                    .join(p.path().file_name().unwrap_or_default());
                mapping.push(MaterializedInput {
                    path: target.to_string_lossy().into_owned(),
                    file: file.clone(),
                    unit: unit.id.clone(),
                });
                Ok(target.to_string_lossy().into_owned())
            })
            .transpose()?;
        crates.push(serde_json::json!({"display_name":unit.name.value(),"root_module":module,"edition":config.edition.value().map(String::as_str).unwrap_or("2015"),"version":env.get("CARGO_PKG_VERSION"),"deps":deps,"cfg":cfg,"target":unit.target.value(),"env":env,"is_workspace_member":true,"is_proc_macro":unit.role==UnitRole::ProcMacro,"proc_macro_dylib_path":dylib,"source":{"include_dirs":[base],"exclude_dirs":[]}}));
    }
    let sysroot = report
        .toolchains
        .iter()
        .find_map(|t| t.sysroot.value())
        .or_else(|| report.preflight_toolchain.sysroot.value())
        .ok_or_else(|| Error::new("MissingSysroot", "Rust toolchain sysroot unavailable"))?
        .path();
    let json = serde_json::json!({"sysroot":sysroot,"sysroot_src":sysroot.join("lib/rustlib/src/rust/library"),"crates":crates});
    let bytes = serde_json::to_vec_pretty(&json)?;
    if units.is_empty() {
        return Err(Error::new(
            "NoObservedCompilation",
            "a semantic replay requires observed unit configurations, not reused artifact guesses",
        ));
    }
    Ok(RustProjectPlan {
        root,
        lease: None,
        mapping,
        digest: digest(&bytes),
        manifest: bytes,
    })
}
