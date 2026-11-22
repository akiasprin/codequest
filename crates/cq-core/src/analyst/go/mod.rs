mod selection;
use super::languages::*;
use super::native;
use crate::execution::Stage;
use crate::{Error, Result, control::CaptureControl, model::*, ports::*};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};
pub fn analyze(
    r: &NativeAnalysisRequest,
    store: &dyn AnalysisWorkspace,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
) -> Result<()> {
    if store.reuse_stage(Stage::Normalize, s)? {
        return Ok(());
    }
    let capture = store.start_stage(Stage::Capture, s)?;
    let captured = if capture.reused {
        let path = PathBuf::from(
            capture.cursor["capture"]
                .as_str()
                .ok_or_else(|| Error::new("MissingStageArtifact", "go capture"))?,
        );
        serde_json::from_reader(std::fs::File::open(path)?)?
    } else {
        let before = crate::source_tree::manifest(&r.root)?;
        let run = cq_capture_protocol::new_id()?;
        let dir = store.run_directory(&run)?;
        let journal = store.create_capture(&run)?;
        let rec = r.recorder.clone();
        let mut environment = Command::new("go");
        environment.current_dir(&r.root).args([
            "env",
            "-json",
            "GOOS",
            "GOARCH",
            "GOHOSTOS",
            "GOHOSTARCH",
            "GOROOT",
            "GOVERSION",
            "CGO_ENABLED",
            "GOFLAGS",
            "GOEXPERIMENT",
            "GOWORK",
            "GOMOD",
        ]);
        let environment = crate::process::output(environment, control, 30000, 1024 * 1024)?;
        if !environment.status.success() {
            return Err(Error::new(
                "GoEnvironmentFailed",
                String::from_utf8_lossy(&environment.stderr),
            ));
        }
        let environment: Value = serde_json::from_slice(&environment.stdout)?;
        let args = if r.build_args.is_empty() {
            let mut args = vec!["build".into(), "-a".into()];
            if environment["GOWORK"]
                .as_str()
                .is_some_and(|p| !p.is_empty() && p != "off")
            {
                // Ask Go for active workspace modules; the workspace root itself
                // need not be a module and ./... there is not a valid selection.
                let mut modules = Command::new("go");
                modules.current_dir(&r.root).args(["list", "-m", "-json"]);
                let output = crate::process::output(modules, control, 30000, 16 * 1024 * 1024)?;
                if !output.status.success() {
                    return Err(Error::new(
                        "GoWorkspaceFailed",
                        String::from_utf8_lossy(&output.stderr),
                    ));
                }
                for module in
                    serde_json::Deserializer::from_slice(&output.stdout).into_iter::<Value>()
                {
                    let module = module?;
                    if module["Main"] == true
                        && let Some(path) = module["Dir"].as_str()
                        && Path::new(path).starts_with(&r.root)
                    {
                        args.push(format!("{path}/..."));
                    }
                }
                if args.len() == 2 {
                    return Err(Error::new(
                        "GoWorkspaceFailed",
                        "no active workspace modules inside requested root",
                    ));
                }
            } else {
                args.push("./...".into());
            }
            args
        } else {
            r.build_args.clone()
        };
        let selection_args = selection::list_args(&args)?;
        let mut list_args = selection_args.clone();
        if r.allow_partial {
            list_args.insert(1, "-e".into());
        }
        let tests = args.first().is_some_and(|a| a == "test");
        let mut command = Command::new("go");
        command
            .current_dir(&r.root)
            .arg(&args[0])
            .arg("-toolexec")
            .arg(&rec)
            .args(&args[1..]);
        attach(&mut command, &run, journal.as_ref());
        let build = native::observe_command(command, Language::Go, &run, journal.clone(), control)?;
        if build.outcome != BuildOutcome::Succeeded && !r.allow_partial {
            let _ = journal.seal();
            return Err(Error::new(
                "GoBuildFailed",
                format!("inspect {}", dir.display()),
            ));
        }
        // go list uses the same build selection and compiler export archives.
        let mut list = Command::new("go");
        list.current_dir(&r.root).args(&list_args);
        let output = crate::process::output(list, control, 900000, 256 * 1024 * 1024)?;
        if !output.status.success() && !r.allow_partial {
            return Err(Error::new(
                "GoListFailed",
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        if output.stdout.len() > 256 * 1024 * 1024 {
            return Err(Error::new(
                "ProviderOutputLimit",
                "go list output exceeds limit",
            ));
        }
        journal.write_output(
        OutputStream::Probes,
        &serde_json::to_vec(
            &json!({"go_env":environment,"go_list_output":String::from_utf8_lossy(&output.stdout)}),
        )?,
    )?;
        let packages: Vec<Value> = serde_json::Deserializer::from_slice(&output.stdout)
            .into_iter::<Value>()
            .collect::<std::result::Result<_, _>>()?;
        let events = native::seal_events(journal.as_ref())?;
        let mut build = build;
        build.events = events.clone();
        s.builds.push(build);
        let after = crate::source_tree::manifest(&r.root)?;
        if before.iter().any(|(p, h)| after.get(p) != Some(h)) {
            return Err(Error::new(
                "InputChangedDuringBuild",
                "working tree changed while Go compiled",
            ));
        }
        let captured = json!({"run":run,"environment":environment,"packages":packages,"selection_args":selection_args,"args":args,"tests":tests,"events":events});
        let path = store
            .stage_file(Stage::Capture, "go-capture.json")?
            .unwrap_or(dir.join("go-capture.json"));
        std::fs::write(&path, serde_json::to_vec(&captured)?)?;
        std::fs::File::open(&path)?.sync_all()?;
        store.finish_stage(Stage::Capture, s, json!({"capture":path}))?;
        captured
    };
    let run = captured["run"]
        .as_str()
        .ok_or_else(|| Error::new("MissingStageArtifact", "go run"))?
        .to_owned();
    let dir = store.run_directory(&run)?;
    let environment = captured["environment"].clone();
    let packages: Vec<Value> = serde_json::from_value(captured["packages"].clone())?;
    let selection_args: Vec<String> = serde_json::from_value(captured["selection_args"].clone())?;
    let args: Vec<String> = serde_json::from_value(captured["args"].clone())?;
    let tests = captured["tests"] == true;
    let events: Vec<Envelope> = serde_json::from_value(captured["events"].clone())?;
    let materialized = store.start_stage(Stage::Materialize, s)?;
    let material = if materialized.reused {
        let path = PathBuf::from(
            materialized.cursor["material"]
                .as_str()
                .ok_or_else(|| Error::new("MissingStageArtifact", "go inputs"))?,
        );
        serde_json::from_reader(std::fs::File::open(path)?)?
    } else {
        let mut mapping = vec![];
        let mut file_map = BTreeMap::new();
        let mut archives = BTreeMap::new();
        let mut package_units = BTreeMap::new();
        if packages
            .iter()
            .any(|p| p["CgoFiles"].as_array().is_some_and(|a| !a.is_empty()))
        {
            s.gaps.push(Gap::new("CgoExternalInputs","Go compilation","cgo may read undeclared native headers; unchanged checkpoint reuse requires a fresh build"));
        }
        for p in &packages {
            let import = p["ImportPath"].as_str().unwrap_or("");
            if let Some(export) = p["Export"].as_str()
                && !export.is_empty()
            {
                let file = store.content().freeze(
                    Path::new(export),
                    "go_compiler_export_archive",
                    1024 * 1024 * 1024,
                )?;
                let target = dir.join(format!("export-{}", file.blob.digest));
                std::fs::write(&target, store.content().read_blob(&file.blob)?)?;
                archives.insert(import.to_owned(), target);
            }
            let input_directory = PathBuf::from(p["Dir"].as_str().unwrap_or(""));
            for field in [
                "CompiledGoFiles",
                "GoFiles",
                "CgoFiles",
                "CFiles",
                "CXXFiles",
                "HFiles",
                "SFiles",
                "EmbedFiles",
            ] {
                for name in p[field]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    let path = if Path::new(name).is_absolute() {
                        PathBuf::from(name)
                    } else {
                        input_directory.join(name)
                    };
                    if path.is_file() {
                        s.analysis_inputs.push(store.content().freeze(
                            &path,
                            "go_selected_input",
                            1024 * 1024 * 1024,
                        )?);
                    }
                }
            }
            let package_dir = PathBuf::from(p["Dir"].as_str().unwrap_or(""));
            if p["Standard"] == true || !package_dir.starts_with(&r.root) {
                continue;
            }
            let selected = p["CompiledGoFiles"]
                .as_array()
                .or(p["GoFiles"].as_array())
                .cloned()
                .unwrap_or_default();
            let files: Vec<_> = selected
                .iter()
                .filter_map(Value::as_str)
                .map(|f| {
                    if Path::new(f).is_absolute() {
                        PathBuf::from(f)
                    } else {
                        package_dir.join(f)
                    }
                })
                .filter(|f| f.starts_with(&r.root))
                .collect();
            if files.is_empty() {
                continue;
            }
            let compiler_package = import.split(" [").next().unwrap_or(import);
            let expected_files: std::collections::BTreeSet<_> = files.iter().cloned().collect();
            let invocations: Vec<_> = events
                .iter()
                .filter_map(|e| {
                    let Event::InvocationStarted { args, cwd, .. } = &e.event else {
                        return None;
                    };
                    let package_matches = args.windows(2).any(|pair| {
                        pair[0].text() == Some("-p") && pair[1].text() == Some(compiler_package)
                    });
                    let selected: std::collections::BTreeSet<_> = args
                        .iter()
                        .filter_map(|a| a.text())
                        .filter(|a| a.ends_with(".go"))
                        .map(|a| {
                            let p = PathBuf::from(a);
                            let p = if p.is_absolute() {
                                p
                            } else {
                                cwd.path().join(p)
                            };
                            p.canonicalize().unwrap_or(p)
                        })
                        .collect();
                    (package_matches && selected == expected_files).then(|| e.invocation_id.clone())
                })
                .collect();
            let invocation = if invocations.len() == 1 {
                invocations.into_iter().next()
            } else {
                None
            };
            if invocation.is_none() {
                s.gaps.push(Gap::new("GoCompilerInvocationNotBound",import,"go list selected these source bytes, but no unique observed compiler job matched both canonical package path and exact source set"));
            }
            let mut unit = native::unit(native::NativeUnitInput {
                language: Language::Go,
                run: run.clone(),
                name: import.into(),
                root: NativeValue::new(&files[0]),
                arguments: args.iter().map(|v| NativeValue::from(v.as_str())).collect(),
                cwd: NativeValue::new(&package_dir),
                files: files.iter().map(NativeValue::new).collect(),
                invocation: invocation.map(Observed::Value).unwrap_or(Observed::Unknown(
                    "compiler job could not be uniquely bound by package and source set".into(),
                )),
            });
            if !p["ForTest"].is_null() || import.ends_with(".test") {
                unit.role = UnitRole::Test;
            }
            unit.host = Observed::Value(format!(
                "{}-{}",
                environment["GOHOSTOS"].as_str().unwrap_or("unknown"),
                environment["GOHOSTARCH"].as_str().unwrap_or("unknown")
            ));
            unit.target = Observed::Value(format!(
                "{}-{}",
                environment["GOOS"].as_str().unwrap_or("unknown"),
                environment["GOARCH"].as_str().unwrap_or("unknown")
            ));
            if let ConfigurationObservation::Native(config) = &mut unit.configuration {
                config.environment = environment
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect();
                if let Some(inv) = unit.invocation.value().and_then(|id| {
                    events.iter().find(|e| {
                        &e.invocation_id == id && matches!(e.event, Event::InvocationStarted { .. })
                    })
                }) && let Event::InvocationStarted { args, cwd, .. } = &inv.event
                {
                    config.arguments = args.clone();
                    config.working_directory = cwd.clone();
                }
            }
            if let ConfigurationObservation::Native(c) = &mut unit.configuration {
                c.environment.insert(
                    "CQ_SELECTION".into(),
                    serde_json::to_string(&selection_args)?,
                );
            }
            let failed =
                p["Error"].is_object() || p["DepsErrors"].as_array().is_some_and(|v| !v.is_empty());
            if failed {
                crate::semantic::production::failed(
                    s,
                    &unit.id,
                    "Go package or dependency compilation failed; package facts are unavailable",
                );
                s.units.push(unit);
                continue;
            }
            package_units.insert(import.to_owned(), unit.id.clone());
            for file in files {
                let m = frozen(store, &file, &unit.id, &dir)?;
                let actual = dir
                    .join("inputs")
                    .join(&m.file.id)
                    .join(file.file_name().unwrap());
                file_map.insert(file.to_string_lossy().into_owned(), actual);
                mapping.push(m);
            }
            s.units.push(unit);
        }
        postcheck(&mapping)?;
        let files: BTreeMap<_, _> = mapping
            .iter()
            .map(|m| (m.file.id.clone(), m.file.clone()))
            .collect();
        s.sources = SourceSnapshot::seal(
            files.into_values().collect(),
            InputBinding::PrePostChecked,
            vec![],
        );
        for (package, unit) in &package_units {
            let mut pending = vec![package.clone()];
            let mut seen = std::collections::BTreeSet::new();
            while let Some(id) = pending.pop() {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(p) = packages
                    .iter()
                    .find(|p| p["ImportPath"].as_str() == Some(&id))
                {
                    for import in p["Imports"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        pending.push(p["ImportMap"][import].as_str().unwrap_or(import).to_owned());
                    }
                }
            }
            let context_units: Vec<_> = seen
                .iter()
                .filter_map(|id| package_units.get(id).cloned())
                .collect();
            let bound = context_units.iter().all(|id| {
                s.units
                    .iter()
                    .any(|u| &u.id == id && u.binding == UnitBinding::ObservedInvocation)
            });
            s.contexts.push(AnalysisContext {
                id: digest(unit.as_bytes()),
                entry_unit: unit.clone(),
                units: context_units,
                status: if bound {
                    Support::Supported
                } else {
                    Support::Partial
                },
                gaps: if bound {
                    vec![]
                } else {
                    vec![Gap::new(
                        "GoCompilerInvocationNotBound",
                        package,
                        "one or more selected package instances lack a unique compiler job binding",
                    )]
                },
            });
        }
        let healthy = packages
            .iter()
            .filter(|p| {
                !p["Error"].is_object() && p["DepsErrors"].as_array().is_none_or(|v| v.is_empty())
            })
            .collect::<Vec<_>>();
        if s.builds
            .last()
            .is_some_and(|b| b.outcome != BuildOutcome::Succeeded)
            && !s.production.as_ref().is_some_and(|p| p.partial)
        {
            return Err(Error::new(
                "IncompleteSelection",
                "failed Go build has no attributable failed package; cannot safely publish partial",
            ));
        }
        let input = json!({"Root":r.root,"Packages":healthy,"Files":file_map,"Archives":archives,"Units":package_units,"GOARCH":environment["GOARCH"]});
        let input_path = dir.join("go-input.json");
        std::fs::write(&input_path, serde_json::to_vec(&input)?)?;
        s.coverage.push(CapabilityCoverage {
        language: Some(Language::Go),
        capability: "go_test_variants".into(),
        state: if tests {
            Support::Supported
        } else {
            Support::Partial
        },
        reason: if tests {
            "actual go test compilation; production/internal/external test units remain distinct"
        } else {
            "supported via an explicit go test selection; this build selected production packages"
        }
        .into(),
    });
        let retained_files = std::iter::once(input_path.clone())
            .chain(file_map.values().cloned())
            .chain(archives.values().cloned())
            .collect::<Vec<_>>();
        let material = json!({"input":input,"input_path":input_path,"mapping":mapping});
        let path = store
            .stage_file(Stage::Materialize, "go-material.json")?
            .unwrap_or(dir.join("go-material.json"));
        std::fs::write(&path, serde_json::to_vec(&material)?)?;
        std::fs::File::open(&path)?.sync_all()?;
        store.finish_stage(
            Stage::Materialize,
            s,
            json!({"material":path,"files":retained_files}),
        )?;
        material
    };
    let input = material["input"].clone();
    let input_path = PathBuf::from(
        material["input_path"]
            .as_str()
            .ok_or_else(|| Error::new("MissingStageArtifact", "go input path"))?,
    );
    let mapping: Vec<MaterializedInput> = serde_json::from_value(material["mapping"].clone())?;
    let worker = r.provider.clone();
    let exported = store.start_stage(Stage::NativeExport, s)?;
    let output = if exported.reused {
        PathBuf::from(
            exported.cursor["output"]
                .as_str()
                .ok_or_else(|| Error::new("MissingStageArtifact", "Go export"))?,
        )
    } else {
        store
            .stage_file(Stage::NativeExport, "native.json")?
            .unwrap_or(dir.join("native.json"))
    };
    let mut command = Command::new(&worker);
    command.arg(input_path).arg(&output);
    if !exported.reused {
        run_worker(command, control, 900000)?;
        store.finish_stage(Stage::NativeExport, s, json!({"output":output}))?;
    }
    consume(
        s,
        store,
        NativeExport {
            allow_partial: r.allow_partial,
            worker: &worker,
            output: &output,
            input: &input,
            mapping: &mapping,
            provider: "go/types",
            language: Language::Go,
        },
    )
}
