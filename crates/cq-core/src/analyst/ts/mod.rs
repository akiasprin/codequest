//! Native TypeScript / TSX analyst backed by the TypeScript compiler API
//! (`providers/ts-compiler/index.mjs`). All exported facts adhere to the
//! `codequest-hir-export` v1 contract with exact UTF-8 byte spans.

use super::languages::{self, NativeAnalysisRequest, NativeExport};
use crate::execution::Stage;
use crate::{Error, Result, control::CaptureControl, model::*, ports::*};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
};

const SKIP_DISCOVERY_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".codequest",
    ".codequest-tools",
    "vendor",
    "dist",
    "build",
    "mockups",
    "coverage",
    ".cache",
    "fixtures",
    "examples",
];

const SKIP_SOURCE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".codequest",
    ".codequest-tools",
    "vendor",
    "dist",
    "build",
    "coverage",
    ".cache",
];

const TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Discover `tsconfig.json` project files under `root` up to depth 3.
pub fn discover_tsconfigs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_tsconfigs(root, 0, &mut found);
    found.sort();
    found.dedup();
    found
}

fn walk_tsconfigs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let config = dir.join("tsconfig.json");
    if config.is_file() {
        out.push(config.canonicalize().unwrap_or(config));
    }
    if depth == 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if SKIP_DISCOVERY_DIRS.iter().any(|skip| name == *skip) {
            continue;
        }
        subdirs.push(entry.path());
    }
    subdirs.sort();
    for sub in subdirs {
        walk_tsconfigs(&sub, depth + 1, out);
    }
}

fn collect_project_sources(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 24 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            if !SKIP_SOURCE_DIRS.iter().any(|skip| name == *skip) {
                subdirs.push(path);
            }
        } else if ft.is_file() {
            let keep = path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| TS_EXTENSIONS.contains(&ext))
                || path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| name.starts_with("tsconfig") && name.ends_with(".json"));
            if keep {
                files.push(path);
            }
        }
    }
    files.sort();
    subdirs.sort();
    out.extend(files);
    for sub in subdirs {
        collect_project_sources(&sub, depth + 1, out);
    }
}

fn unit_for_tsconfig(
    root: &Path,
    tsconfig: &Path,
    selected_files: &[PathBuf],
) -> Result<BuildUnit> {
    let project_dir = tsconfig.parent().unwrap_or(root);
    let rel_dir = project_dir
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".into());
    let unit_name = format!("ts:{rel_dir}");
    let id = digest(&serde_json::to_vec(&(
        "typescript",
        &unit_name,
        tsconfig.to_string_lossy().as_ref(),
    ))?);
    Ok(BuildUnit {
        id,
        language: Language::TypeScript,
        name: Observed::Value(unit_name),
        root: Observed::Value(NativeValue::new(tsconfig)),
        output_kinds: vec!["typecheck".into()],
        role: UnitRole::Library,
        host: Observed::Value("node".into()),
        target: Observed::Value("esnext".into()),
        configuration: ConfigurationObservation::Native(NativeConfiguration {
            language: Language::TypeScript,
            arguments: vec![
                NativeValue::from("--project"),
                NativeValue::new(tsconfig),
                NativeValue::from("--noEmit"),
            ],
            environment: BTreeMap::new(),
            selected_files: selected_files.iter().map(NativeValue::new).collect(),
            working_directory: NativeValue::new(project_dir),
        }),
        invocation: Observed::Unknown("direct ts.createProgram invocation".into()),
        artifacts: vec![],
        binding: UnitBinding::ObservedInvocation,
        evidence: vec![],
    })
}

/// Merge an independently normalized snapshot (e.g. a secondary TypeScript pass)
/// into `target` without re-scanning existing `target` occurrences/relations.
pub fn merge_snapshot(target: &mut SemanticSnapshot, extra: SemanticSnapshot) {
    target.units.extend(extra.units);
    target.providers.extend(extra.providers);
    target.analysis_inputs.extend(extra.analysis_inputs);
    target.provenance_records.extend(extra.provenance_records);
    target.entities.extend(extra.entities);
    target.bodies.extend(extra.bodies);
    target.occurrences.extend(extra.occurrences);
    target.relations.extend(extra.relations);
    target.documents.extend(extra.documents);
    target.coverage.extend(extra.coverage);
    target.gaps.extend(extra.gaps);

    let mut files_by_path: BTreeMap<NativeValue, FileRevision> = target
        .sources
        .files
        .drain(..)
        .map(|f| (f.logical_path.clone(), f))
        .collect();
    for f in extra.sources.files {
        files_by_path.entry(f.logical_path.clone()).or_insert(f);
    }
    let mut source_gaps = target.sources.gaps.clone();
    source_gaps.extend(extra.sources.gaps);
    target.sources = SourceSnapshot::seal(
        files_by_path.into_values().collect(),
        target.sources.binding.clone(),
        source_gaps,
    );
}

/// Analyze TypeScript/TSX projects discovered under `r.root` through an
/// `AnalysisWorkspace` (used by `cq index`).
pub fn analyze(
    r: &NativeAnalysisRequest,
    store: &dyn AnalysisWorkspace,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
    tracked: bool,
) -> Result<()> {
    if tracked && store.reuse_stage(Stage::Normalize, s)? {
        return Ok(());
    }
    let tsconfigs = discover_tsconfigs(&r.root);
    if tsconfigs.is_empty() {
        return Err(Error::new(
            "MissingBuildConfiguration",
            format!("no tsconfig.json found under {}", r.root.display()),
        ));
    }

    let run = cq_capture_protocol::new_id()?;
    let dir = store.run_directory(&run)?;
    std::fs::create_dir_all(&dir)?;

    let mut mapping = Vec::new();
    let mut projects_json = Vec::new();
    for tsconfig in &tsconfigs {
        let project_dir = tsconfig.parent().unwrap_or(&r.root);
        let mut sources = Vec::new();
        collect_project_sources(project_dir, 0, &mut sources);
        let unit = unit_for_tsconfig(&r.root, tsconfig, &sources)?;
        let mut file_map = BTreeMap::new();
        for src in &sources {
            let file = store.content().freeze(
                src,
                "compiler_selected_input_prepost_checked",
                1024 * 1024 * 1024,
            )?;
            let materialized = store.materialize_input(&file, &dir)?;
            file_map.insert(
                src.to_string_lossy().into_owned(),
                materialized.to_string_lossy().into_owned(),
            );
            s.analysis_inputs.push(file.clone());
            mapping.push(MaterializedInput {
                path: src.to_string_lossy().into_owned(),
                file,
                unit: unit.id.clone(),
            });
        }
        projects_json.push(json!({
            "unit": unit.id,
            "tsconfig": tsconfig.to_string_lossy(),
            "files": file_map,
        }));
        s.units.push(unit);
    }

    let input = json!({
        "root": r.root.to_string_lossy(),
        "projects": projects_json,
    });
    let input_path = dir.join("ts-input.json");
    let output_path = dir.join("ts-output.json");
    std::fs::write(&input_path, serde_json::to_vec(&input)?)?;

    let mut cmd = Command::new("node");
    cmd.arg(&r.provider).arg(&input_path).arg(&output_path);
    languages::run_worker(cmd, control, 300_000)?;
    languages::postcheck(&mapping)?;

    let export = NativeExport {
        allow_partial: r.allow_partial,
        worker: &r.provider,
        output: &output_path,
        input: &input,
        mapping: &mapping,
        provider: "ts-compiler",
        language: Language::TypeScript,
    };

    if !tracked && store.semantic_records().is_none() && !s.entities.is_empty() {
        let mut ts_snapshot = SemanticSnapshot::empty(s.project.clone(), s.root.clone());
        languages::consume_with_tracking(&mut ts_snapshot, store, export, false)?;
        merge_snapshot(s, ts_snapshot);
        return Ok(());
    }
    languages::consume_with_tracking(s, store, export, tracked)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct TsAugmentStats {
    pub tsconfigs: usize,
    pub files: usize,
    pub entities: usize,
    pub bodies: usize,
    pub occurrences: usize,
    pub relations: usize,
    pub diagnostics: usize,
}

/// Directly index TypeScript/TSX projects under `disk_root` and merge them into
/// an in-memory `SemanticSnapshot` anchored at `logical_root`.
pub fn analyze_in_memory(
    disk_root: &Path,
    logical_root: &Path,
    provider: &Path,
    ts_module: Option<&Path>,
    snapshot: &mut SemanticSnapshot,
    content: &dyn ContentRepository,
) -> Result<TsAugmentStats> {
    let disk_root = disk_root.canonicalize()?;
    let tsconfigs = discover_tsconfigs(&disk_root);
    if tsconfigs.is_empty() {
        return Err(Error::new(
            "MissingBuildConfiguration",
            format!("no tsconfig.json found under {}", disk_root.display()),
        ));
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "cq-ts-mem-{}",
        cq_capture_protocol::new_id().unwrap_or_else(|_| "run".into())
    ));
    std::fs::create_dir_all(&tmp_dir)?;

    let result = (|| -> Result<TsAugmentStats> {
        let mut projects_json = Vec::new();
        let mut units = Vec::new();
        let mut unit_by_tsconfig = HashMap::new();
        for tsconfig in &tsconfigs {
            let project_dir = tsconfig.parent().unwrap_or(&disk_root);
            let mut sources = Vec::new();
            collect_project_sources(project_dir, 0, &mut sources);
            let logical_tsconfig = remap_path(tsconfig, &disk_root, logical_root);
            let logical_sources: Vec<_> = sources
                .iter()
                .map(|p| remap_path(p, &disk_root, logical_root))
                .collect();
            let unit = unit_for_tsconfig(logical_root, &logical_tsconfig, &logical_sources)?;
            projects_json.push(json!({
                "unit": unit.id,
                "tsconfig": tsconfig.to_string_lossy(),
                "files": {},
            }));
            unit_by_tsconfig.insert(tsconfig.clone(), unit.id.clone());
            units.push(unit);
        }

        let mut input = json!({
            "root": disk_root.to_string_lossy(),
            "projects": projects_json,
        });
        if let Some(ts_mod) = ts_module {
            input["typescript_path"] = json!(ts_mod.to_string_lossy());
        }
        let input_path = tmp_dir.join("ts-input.json");
        let output_path = tmp_dir.join("ts-output.json");
        std::fs::write(&input_path, serde_json::to_vec(&input)?)?;

        let mut cmd = Command::new("node");
        cmd.arg(provider).arg(&input_path).arg(&output_path);
        languages::run_worker(cmd, &CaptureControl::default(), 300_000)?;

        let mut raw: Value =
            serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(&output_path)?))?;
        let diagnostics = crate::semantic::diagnostics::read(&raw)?;
        if diagnostics.iter().any(|d| d.is_error()) {
            let errors: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.is_error())
                .map(|d| d.message.as_str())
                .collect();
            return Err(Error::new("NativeAnalysisFailed", errors.join("; ")));
        }

        // Remap disk_root -> logical_root on exported file entries and build the
        // MaterializedInput mapping from the exact exported file bytes.
        let mut mapping = Vec::new();
        let mut seen_files = HashSet::new();
        if let Some(files) = raw["files"].as_array_mut() {
            for f in files.iter_mut() {
                let disk_path = PathBuf::from(f["path"].as_str().unwrap_or_default());
                let logical_path = remap_path(&disk_path, &disk_root, logical_root);
                let logical_str = logical_path.to_string_lossy().into_owned();
                let unit = f["unit"].as_str().unwrap_or("ts-project").to_owned();
                let text = f["text"].as_str().unwrap_or_default();
                let blob = content.put(text.as_bytes(), 1024 * 1024 * 1024)?;
                f["path"] = json!(logical_str);
                if seen_files.insert((logical_str.clone(), unit.clone())) {
                    let nv = NativeValue::new(&logical_path);
                    let id = digest(&serde_json::to_vec(&(&nv, &blob))?);
                    mapping.push(MaterializedInput {
                        path: logical_str,
                        file: FileRevision {
                            id,
                            logical_path: nv,
                            blob,
                            origin: "ts_compiler_selected_input".into(),
                        },
                        unit,
                    });
                }
            }
        }

        let version = raw["version"].as_str().unwrap_or("ts-v1").to_owned();
        let mut ts_snapshot =
            SemanticSnapshot::empty(snapshot.project.clone(), NativeValue::new(logical_root));
        ts_snapshot.units = units;
        ts_snapshot.providers.push(ProviderRecord {
            analysis: None,
            reused_from: None,
            name: "ts-compiler".into(),
            version: version.clone(),
            binary_digest: digest(&std::fs::read(provider)?),
            input_digest: digest(&serde_json::to_vec(&input)?),
            protocol: "native-v1".into(),
        });

        crate::semantic::normalize::native_graph(
            &mut ts_snapshot,
            &raw,
            &mapping,
            content,
            "ts-compiler",
            Language::TypeScript,
            &version,
        )?;
        for d in &diagnostics {
            ts_snapshot.gaps.push(Gap::new(
                "NativeDiagnostic",
                "ts-compiler",
                d.message.clone(),
            ));
        }

        let stats = TsAugmentStats {
            tsconfigs: tsconfigs.len(),
            files: ts_snapshot.sources.files.len(),
            entities: ts_snapshot.entities.len(),
            bodies: ts_snapshot.bodies.len(),
            occurrences: ts_snapshot.occurrences.len(),
            relations: ts_snapshot.relations.len(),
            diagnostics: diagnostics.len(),
        };

        merge_snapshot(snapshot, ts_snapshot);
        Ok(stats)
    })();

    let _ = std::fs::remove_dir_all(&tmp_dir);
    result
}

fn remap_path(path: &Path, from_root: &Path, to_root: &Path) -> PathBuf {
    if from_root == to_root {
        return path.to_path_buf();
    }
    match path.strip_prefix(from_root) {
        Ok(rel) => to_root.join(rel),
        Err(_) => path.to_path_buf(),
    }
}
