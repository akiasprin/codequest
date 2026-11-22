//! C/C++ phase adapters retain the selected translation units and native
//! evidence separately; a later-stage replay never invokes the compiler.
use super::*;
use crate::execution::Stage;
fn read(cursor: &Value, key: &str) -> Result<Value> {
    Ok(serde_json::from_reader(std::fs::File::open(
        cursor[key]
            .as_str()
            .ok_or_else(|| Error::new("MissingStageArtifact", key))?,
    )?)?)
}
fn save_stage(
    store: &dyn AnalysisWorkspace,
    stage: Stage,
    s: &SemanticSnapshot,
    dir: &std::path::Path,
    name: &str,
    value: &Value,
) -> Result<()> {
    let path = store
        .stage_file(stage, name)?
        .unwrap_or_else(|| dir.join(name));
    std::fs::write(&path, serde_json::to_vec(value)?)?;
    std::fs::File::open(&path)?.sync_all()?;
    let mut evidence = vec![];
    collect_files(value, &mut evidence);
    store.finish_stage(stage, s, json!({"manifest":path,"files":evidence}))
}
fn collect_files(value: &Value, out: &mut Vec<PathBuf>) {
    match value {
        Value::Array(a) => {
            for v in a {
                collect_files(v, out);
            }
        }
        Value::Object(m) => {
            for (key, v) in m {
                if key == "output" {
                    if let Some(p) = v.as_str() {
                        out.push(PathBuf::from(p));
                    }
                } else if key == "files" {
                    if let Some(a) = v.as_array() {
                        out.extend(a.iter().filter_map(Value::as_str).map(PathBuf::from));
                    }
                } else {
                    collect_files(v, out);
                }
            }
        }
        _ => (),
    }
}
pub(super) fn analyze(
    r: &NativeAnalysisRequest,
    store: &dyn AnalysisWorkspace,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
) -> Result<()> {
    if store.reuse_stage(Stage::Normalize, s)? {
        return Ok(());
    }
    let before = crate::source_tree::manifest(&r.root)?;
    let compdb = r
        .compdb
        .clone()
        .unwrap_or_else(|| r.root.join("compile_commands.json"));
    let commands: Vec<Value> = serde_json::from_reader(std::fs::File::open(compdb)?)?;
    let worker = r.provider.clone();
    let rec = r.recorder.clone();
    let work = store.run_directory(&cq_capture_protocol::new_id()?)?;
    std::fs::create_dir_all(&work)?;
    let capture = store.start_stage(Stage::Capture, s)?;
    let tus = if capture.reused {
        read(&capture.cursor, "manifest")?
    } else {
        let mut tus = vec![];
        for (number, entry) in commands.iter().enumerate() {
            let cwd = PathBuf::from(
                entry["directory"]
                    .as_str()
                    .unwrap_or(r.root.to_str().unwrap_or(".")),
            );
            let cwd = if cwd.is_absolute() {
                cwd
            } else {
                r.root.join(cwd)
            }
            .canonicalize()?;
            let file = PathBuf::from(
                entry["file"]
                    .as_str()
                    .ok_or_else(|| Error::new("InvalidCompilationDatabase", "missing file"))?,
            );
            let file = if file.is_absolute() {
                file
            } else {
                cwd.join(file)
            }
            .canonicalize()?;
            let args: Vec<String> = if let Some(args) = entry["arguments"].as_array() {
                args.iter()
                    .map(|a| {
                        a.as_str().map(str::to_owned).ok_or_else(|| {
                            Error::new("InvalidCompilationDatabase", "non-string argument")
                        })
                    })
                    .collect::<Result<_>>()?
            } else {
                shlex::split(entry["command"].as_str().unwrap_or("")).ok_or_else(|| {
                    Error::new("InvalidCompilationDatabase", "invalid command quoting")
                })?
            };
            if args.is_empty() {
                return Err(Error::new("InvalidCompilationDatabase", "empty command"));
            }
            let replay = command::replay(&args, &cwd, &file)?;
            let language = replay.language;
            let response_inputs = replay
                .response_inputs
                .iter()
                .map(|path| {
                    store
                        .content()
                        .freeze(path, "compiler_response_input", 8 * 1024 * 1024)
                })
                .collect::<Result<Vec<_>>>()?;
            let run = cq_capture_protocol::new_id()?;
            let dir = store.run_directory(&run)?;
            let journal = store.create_capture(&run)?;
            // Preserve the compilation database command; the recorder delegates it unchanged.
            let mut command = Command::new(&rec);
            command.current_dir(&cwd).args(&args);
            attach(&mut command, &run, journal.as_ref());
            let mut build =
                native::observe_command(command, language, &run, journal.clone(), control)?;
            let events = native::seal_events(journal.as_ref())?;
            for input in &response_inputs {
                if BlobId::of(&std::fs::read(input.logical_path.path())?) != input.blob {
                    return Err(Error::new(
                        "InputChangedDuringBuild",
                        "response file changed during compilation",
                    ));
                }
            }
            s.analysis_inputs.extend(response_inputs);
            build.events = events.clone();
            if build.outcome != BuildOutcome::Succeeded && !r.allow_partial {
                return Err(Error::new(
                    "CxxBuildFailed",
                    format!("translation unit {number}: {}", dir.display()),
                ));
            }
            let invocation = events
                .iter()
                .find(|e| matches!(e.event, Event::InvocationStarted { .. }))
                .map(|e| Observed::Value(e.invocation_id.clone()))
                .unwrap_or(Observed::Unknown("no observed compiler".into()));
            let unit = native::unit(native::NativeUnitInput {
                language,
                run: run.clone(),
                name: file.to_string_lossy().into_owned(),
                root: NativeValue::new(&file),
                arguments: args.iter().map(|a| NativeValue::from(a.as_str())).collect(),
                cwd: NativeValue::new(&cwd),
                files: vec![NativeValue::new(&file)],
                invocation,
            });
            s.builds.push(build);
            if s.builds.last().unwrap().outcome != BuildOutcome::Succeeded {
                crate::semantic::production::failed(
                    s,
                    &unit.id,
                    "C/C++ translation unit failed; its old facts and dependent resolved edges are unavailable",
                );
                s.analysis_inputs.push(store.content().freeze(
                    &file,
                    "failed_compiler_input",
                    1024 * 1024 * 1024,
                )?);
                s.units.push(unit);
                continue;
            }
            tus.push(json!({"dir":dir,"file":file,"cwd":cwd,"args":args,"analysis_arguments":replay.arguments,"unit":unit,"language":language}));
        }
        let tus = json!(tus);
        save_stage(
            store,
            Stage::Capture,
            s,
            &work,
            "translation-units.json",
            &tus,
        )?;
        tus
    };
    let materialized = store.start_stage(Stage::Materialize, s)?;
    if !materialized.reused {
        for tu in tus
            .as_array()
            .ok_or_else(|| Error::new("InvalidStageArtifact", "translation units"))?
        {
            s.analysis_inputs.push(store.content().freeze(
                std::path::Path::new(
                    tu["file"].as_str().ok_or_else(|| {
                        Error::new("InvalidStageArtifact", "translation unit file")
                    })?,
                ),
                "compiler_selected_translation_unit",
                1024 * 1024 * 1024,
            )?);
        }
        store.finish_stage(Stage::Materialize, s, Value::Null)?;
    }
    let native = store.start_stage(Stage::NativeExport, s)?;
    let exports = if native.reused {
        read(&native.cursor, "manifest")?
    } else {
        let mut exports = vec![];
        for tu in tus.as_array().unwrap() {
            let dir = PathBuf::from(tu["dir"].as_str().unwrap());
            let file = PathBuf::from(tu["file"].as_str().unwrap());
            let cwd = PathBuf::from(tu["cwd"].as_str().unwrap());
            let args: Vec<String> = serde_json::from_value(tu["args"].clone())?;
            let unit: BuildUnit = serde_json::from_value(tu["unit"].clone())?;
            let language: Language = serde_json::from_value(tu["language"].clone())?;
            let analysis_arguments = tu["analysis_arguments"].clone();
            // libclang discovers included files and reports their actual input bytes;
            // these are semantic replay inputs, distinct from build-read proof.
            let mut input = json!({"file":file,"directory":cwd,"arguments":args,"analysis_arguments":analysis_arguments,"effective_language":language,"unit":unit.id});
            let mut version = Command::new(&worker);
            version.arg("--version");
            let version = crate::process::output(version, control, 10000, 1024 * 1024)?;
            if !version.status.success() {
                return Err(Error::new(
                    "ProviderFailed",
                    "cannot identify Clang producer",
                ));
            }
            let cache_key = digest(&serde_json::to_vec(&(
                &file,
                &cwd,
                &args,
                &input["analysis_arguments"],
                digest(&std::fs::read(&worker)?),
                version.stdout,
                std::env::vars_os()
                    .filter(|(k, _)| !matches!(k.to_str(), Some("_" | "SHLVL" | "PWD" | "OLDPWD")))
                    .map(|(k, v)| (NativeValue::new(k), NativeValue::new(v)))
                    .collect::<Vec<_>>(),
            ))?);
            let input_path = dir.join("clang-input.json");
            std::fs::write(&input_path, serde_json::to_vec(&input)?)?;
            let output = store
                .stage_file(Stage::NativeExport, &format!("tu-{}-native.json", unit.id))?
                .unwrap_or(dir.join("native.json"));
            let mut command = Command::new(&worker);
            command.arg(input_path).arg(&output);
            let eligible = !args.iter().any(|a| {
                a.starts_with('@')
                    || a.starts_with("-fplugin")
                    || a.starts_with("-fmodule")
                    || a == "-include-pch"
            });
            let cached = store
                .load_native_cache(&cache_key)
                .ok()
                .flatten()
                .filter(|c| {
                    eligible
                        && !c.include_queries
                        && c.version == 1
                        && c.directories.iter().all(|(path, stamp)| {
                            crate::source_tree::directory_digest(&path.path())
                                .ok()
                                .as_ref()
                                == Some(stamp)
                        })
                        && c.sources.iter().all(|f| {
                            std::fs::read(f.logical_path.path())
                                .ok()
                                .is_some_and(|b| BlobId::of(&b) == f.blob)
                        })
                });
            let reused = if let Some(cache) = cached {
                if let Ok(bytes) = store.content().read_blob(&cache.raw) {
                    if let Ok(mut raw) = serde_json::from_slice::<Value>(&bytes) {
                        raw["unit"] = serde_json::to_value(&unit.id)?;
                        std::fs::write(&output, serde_json::to_vec(&raw)?)?;
                        input["analysis_reuse"] = serde_json::to_value(cache.raw.digest)?;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if !reused {
                run_worker(command, control, 900000)?;
            }
            let mut raw: Value = serde_json::from_slice(&std::fs::read(&output)?)?;
            if raw["effective_language"] != serde_json::to_value(language)? {
                return Err(Error::new(
                    "ReplayConfigurationMismatch",
                    "producer did not attest the effective compilation language",
                ));
            }
            let diagnostics = crate::semantic::diagnostics::read(&raw)?;
            if diagnostics.iter().any(|d| d.is_error()) {
                let blob = store
                    .content()
                    .put(&std::fs::read(&output)?, 1024 * 1024 * 1024)?;
                s.providers.push(ProviderRecord {
                    analysis: None,
                    reused_from: None,
                    name: "libclang".into(),
                    version: raw["version"].as_str().unwrap_or("unknown").into(),
                    binary_digest: digest(&std::fs::read(&worker)?),
                    input_digest: digest(&serde_json::to_vec(&input)?),
                    protocol: format!("native-v1; failed replay raw={}", blob.digest),
                });
                if !r.allow_partial {
                    return Err(Error::new(
                        "NativeAnalysisFailed",
                        format!(
                            "semantic replay failed for {}; raw={}; HEAD unchanged",
                            file.display(),
                            blob.digest
                        ),
                    ));
                }
                for d in diagnostics {
                    s.gaps.push(Gap::new(
                        "NativeDiagnostic",
                        file.display().to_string(),
                        d.message,
                    ));
                }
                crate::semantic::production::failed(
                    s,
                    &unit.id,
                    "semantic replay reported error/fatal diagnostics; scope unavailable",
                );
                s.units.push(unit);
                continue;
            }
            for f in raw["files"].as_array_mut().into_iter().flatten() {
                if let Some(path) = f["path"].as_str() {
                    f["path"] = json!(cwd.join(path).canonicalize()?);
                }
            }
            std::fs::write(&output, serde_json::to_vec(&raw)?)?;
            let mut mapping = vec![];
            for f in raw["files"].as_array().into_iter().flatten() {
                let path = PathBuf::from(f["path"].as_str().unwrap_or(""));
                if path.is_file() {
                    let m = frozen(store, &path, &unit.id, &dir)?;
                    if store.content().read_blob(&m.file.blob)?
                        != f["text"].as_str().unwrap_or("").as_bytes()
                    {
                        return Err(Error::new(
                            "ProducerInputMismatch",
                            "libclang replay input changed",
                        ));
                    }
                    mapping.push(m);
                }
            }
            postcheck(&mapping)?;
            let after = crate::source_tree::manifest(&r.root)?;
            if before.iter().any(|(p, h)| after.get(p) != Some(h)) {
                return Err(Error::new(
                    "InputChangedDuringBuild",
                    "working tree changed while C/C++ compiled",
                ));
            }
            let mut files = s.sources.files.clone();
            files.extend(mapping.iter().map(|m| m.file.clone()));
            files.sort_by(|a, b| a.id.cmp(&b.id));
            files.dedup_by(|a, b| a.id == b.id);
            s.sources = SourceSnapshot::seal(files, InputBinding::PrePostChecked, vec![]);
            s.contexts.push(AnalysisContext {
                id: digest(unit.id.as_bytes()),
                entry_unit: unit.id.clone(),
                units: vec![unit.id.clone()],
                status: Support::Partial,
                gaps: vec![],
            });
            s.analysis_inputs
                .extend(mapping.iter().map(|m| m.file.clone()));
            let cache_sources = mapping.iter().map(|m| m.file.clone()).collect();
            s.units.push(unit);
            let mut directories: std::collections::BTreeSet<std::path::PathBuf> = mapping
                .iter()
                .filter_map(|m| {
                    m.file
                        .logical_path
                        .path()
                        .parent()
                        .map(std::path::Path::to_path_buf)
                })
                .collect();
            let mut option = args.iter();
            while let Some(arg) = option.next() {
                let directory =
                    if ["-I", "-isystem", "-iquote", "-idirafter"].contains(&arg.as_str()) {
                        option.next().cloned()
                    } else {
                        arg.strip_prefix("-I")
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    };
                if let Some(path) = directory {
                    let path = PathBuf::from(path);
                    directories.insert(if path.is_absolute() {
                        path
                    } else {
                        cwd.join(path)
                    });
                }
            }
            let mut search_roots = directories.clone();
            for include in raw["includes"].as_array().into_iter().flatten() {
                if let (Some(name), Some(target)) =
                    (include["name"].as_str(), include["target"].as_str())
                    && let Some(prefix) = target.strip_suffix(name)
                {
                    search_roots.insert(PathBuf::from(prefix));
                }
            }
            for include in raw["includes"].as_array().into_iter().flatten() {
                if let Some(parent) = include["name"]
                    .as_str()
                    .and_then(|name| std::path::Path::new(name).parent())
                {
                    for search in &search_roots {
                        directories.insert(search.join(parent));
                    }
                }
            }
            let directory_stamps: Vec<(NativeValue, String)> = directories
                .into_iter()
                .map(|p| {
                    Ok((
                        NativeValue::new(&p),
                        crate::source_tree::directory_digest(&p)?,
                    ))
                })
                .collect::<Result<_>>()?;
            s.analysis_directories.extend(directory_stamps.clone());
            if raw["include_queries"] == true || !eligible {
                s.gaps.push(Gap::new("UntrackedIncludeQueries",file.display().to_string(),"optional include queries or plugin/module/response-file inputs require fresh native analysis"));
            }
            if !reused {
                store.save_native_cache(&NativeCache {
                    version: 1,
                    include_queries: raw["include_queries"] == true,
                    directories: directory_stamps,
                    key: cache_key,
                    raw: store
                        .content()
                        .put(&std::fs::read(&output)?, 1024 * 1024 * 1024)?,
                    sources: cache_sources,
                })?;
            }
            exports
                .push(json!({"output":output,"input":input,"mapping":mapping,"language":language}));
        }
        let exports = json!(exports);
        save_stage(
            store,
            Stage::NativeExport,
            s,
            &work,
            "native-exports.json",
            &exports,
        )?;
        exports
    };
    let archived = store.start_stage(Stage::Archive, s)?;
    let archives = if archived.reused {
        read(&archived.cursor, "manifest")?
    } else {
        let mut archives = vec![];
        for (n, export) in exports.as_array().unwrap().iter().enumerate() {
            let output = std::path::Path::new(export["output"].as_str().unwrap());
            let archive = crate::semantic::native_archive::Archive::open(output)?;
            let path = store
                .stage_file(Stage::Archive, &format!("tu-{n}"))?
                .unwrap_or_else(|| work.join(format!("tu-{n}")));
            archive.retain(&path)?;
            archives.push(json!({"archive":path,"export":export,"files":[path.join("rows"),path.join("index.json")]}));
        }
        let archives = json!(archives);
        save_stage(
            store,
            Stage::Archive,
            s,
            &work,
            "native-archives.json",
            &archives,
        )?;
        archives
    };
    let normalized = store.start_stage(Stage::Normalize, s)?;
    if !normalized.reused {
        let first = normalized.cursor["tu"].as_u64().unwrap_or(0) as usize;
        let items = archives.as_array().unwrap();
        for (n, item) in items.iter().enumerate().skip(first) {
            let export = &item["export"];
            let output = PathBuf::from(export["output"].as_str().unwrap());
            let mapping: Vec<MaterializedInput> =
                serde_json::from_value(export["mapping"].clone())?;
            let archive = crate::semantic::native_archive::Archive::restore(std::path::Path::new(
                item["archive"].as_str().unwrap(),
            ))?;
            super::super::languages::consume_untracked(
                s,
                store,
                NativeExport {
                    allow_partial: r.allow_partial,
                    worker: &worker,
                    output: &output,
                    input: &export["input"],
                    mapping: &mapping,
                    provider: "libclang",
                    language: serde_json::from_value(export["language"].clone())?,
                },
                archive,
            )?;
            store.checkpoint_stage(
                Stage::Normalize,
                s,
                json!({"tu":n+1}),
                (n + 1) as u64,
                items.len() as u64,
            )?;
        }
        super::finish_links(s)?;
        store.finish_stage(Stage::Normalize, s, Value::Null)?;
    }
    Ok(())
}
