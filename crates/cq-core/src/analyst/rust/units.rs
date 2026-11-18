use super::args::{self, Arguments};
use crate::model::*;
use std::collections::{BTreeMap, HashMap};

pub(super) fn invocations(events: Vec<Envelope>) -> Vec<CompilerInvocation> {
    let mut out: BTreeMap<String, CompilerInvocation> = BTreeMap::new();
    for (index, e) in events.into_iter().enumerate() {
        let inv = out
            .entry(e.invocation_id.clone())
            .or_insert_with(|| CompilerInvocation {
                id: e.invocation_id.clone(),
                started: None,
                process: None,
                finished: None,
                argument_files: vec![],
                classification: InvocationKind::Unclassified,
                toolchain: Observed::Unknown("tool not yet probed".into()),
                execution: ExecutionObservation::NotObserved,
                evidence: vec![],
            });
        inv.evidence.push(EvidenceRef {
            run_id: e.run_id.clone(),
            source: "events.log".into(),
            index: Some(index),
        });
        match &e.event {
            Event::InvocationStarted { .. } => inv.started = Some(e),
            Event::ProcessStarted { .. } => inv.process = Some(e),
            Event::InvocationFinished { .. } => inv.finished = Some(e),
            Event::ArgumentFileObserved { .. } => inv.argument_files.push(e),
        }
    }
    out.into_values().collect()
}
pub(super) fn build_messages(
    bytes: &[u8],
    run: &str,
) -> (Vec<serde_json::Value>, Vec<BuildArtifact>, Vec<Gap>, bool) {
    let mut messages = vec![];
    let mut artifacts = vec![];
    let mut gaps = vec![];
    let mut finished = false;
    for line in bytes.split(|c| *c == b'\n').filter(|s| !s.is_empty()) {
        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(line) else {
            gaps.push(Gap::new(
                "NonJsonCargoOutput",
                run,
                "raw output retained in cargo.stdout",
            ));
            continue;
        };
        let index = messages.len();
        match serde_json::from_value::<cargo_metadata::Message>(raw.clone()) {
            Ok(cargo_metadata::Message::CompilerArtifact(a)) => {
                let id = digest(&serde_json::to_vec(&(run, index)).unwrap());
                artifacts.push(BuildArtifact {
                    id,
                    package_id: a.package_id.to_string(),
                    target_name: a.target.name,
                    target_kinds: a.target.kind.iter().map(|s| s.to_string()).collect(),
                    src_path: NativeValue::new(args::joined(
                        std::path::Path::new("/"),
                        a.target.src_path.as_std_path(),
                    )),
                    profile: ArtifactProfile {
                        opt_level: a.profile.opt_level,
                        debug_assertions: a.profile.debug_assertions,
                        overflow_checks: a.profile.overflow_checks,
                        test: a.profile.test,
                    },
                    features: a.features,
                    paths: a
                        .filenames
                        .iter()
                        .map(|s| NativeValue::from(s.as_str()))
                        .collect(),
                    fresh: a.fresh,
                    output_aliases: vec![],
                    producer: Observed::Unknown("not yet bound".into()),
                    evidence: EvidenceRef {
                        run_id: run.into(),
                        source: "build_messages".into(),
                        index: Some(index),
                    },
                });
            }
            Ok(cargo_metadata::Message::BuildFinished(_)) => finished = true,
            Ok(_) => {}
            Err(e) => gaps.push(Gap::new("UnknownCargoMessage", run, e.to_string())),
        }
        messages.push(raw);
    }
    (messages, artifacts, gaps, finished)
}
fn observed<T>(value: Option<T>, reason: &str) -> Observed<T> {
    value
        .map(Observed::Value)
        .unwrap_or_else(|| Observed::Unknown(reason.into()))
}
fn role(types: &[String], test: bool) -> UnitRole {
    if test {
        UnitRole::Test
    } else if types.iter().any(|s| s == "proc-macro") {
        UnitRole::ProcMacro
    } else if types.iter().any(|s| s == "bin") {
        UnitRole::Binary
    } else if types.is_empty() {
        UnitRole::Unknown
    } else {
        UnitRole::Library
    }
}
pub(super) struct Normalized {
    pub units: Vec<BuildUnit>,
    pub dependencies: Vec<DependencyBinding>,
    pub arguments: HashMap<String, Arguments>,
    pub gaps: Vec<Gap>,
    pub accounting: JobAccounting,
}
pub(super) fn normalize(
    run: &str,
    invs: &mut [CompilerInvocation],
    artifacts: &mut [BuildArtifact],
    tools: &HashMap<NativeValue, ToolchainObservation>,
    cfgs: &HashMap<String, (Observed<Vec<String>>, usize)>,
    cargo_finished: bool,
) -> Normalized {
    let mut units = vec![];
    let mut arguments = HashMap::new();
    let mut gaps = vec![];
    let mut accounting = JobAccounting::ReconciledWithinScope;
    for inv in invs {
        let a = args::inspect(inv);
        let tool = inv.started.as_ref().and_then(|e| {
            if let Event::InvocationStarted { command, .. } = &e.event {
                tools.get(command)
            } else {
                None
            }
        });
        inv.toolchain = tool
            .map(|t| t.version.clone())
            .unwrap_or_else(|| Observed::Unknown("missing tool identity".into()));
        inv.execution = if inv.process.is_some() {
            if inv.toolchain.value().is_some() {
                ExecutionObservation::KnownRustcStarted
            } else {
                ExecutionObservation::UnidentifiedProcessStarted
            }
        } else {
            ExecutionObservation::NotObserved
        };
        inv.classification = if a.probe {
            InvocationKind::Probe
        } else if a.name.is_some() && a.root.is_some() {
            InvocationKind::Compile
        } else {
            InvocationKind::Unclassified
        };
        if inv.classification == InvocationKind::Probe {
            if inv.execution == ExecutionObservation::UnidentifiedProcessStarted {
                accounting = JobAccounting::Incomplete;
            }
            continue;
        }
        if inv.classification == InvocationKind::Unclassified {
            accounting = JobAccounting::Incomplete;
            gaps.push(Gap::new(
                "UnclassifiedInvocation",
                &inv.id,
                "not a known probe or compile invocation",
            ));
            continue;
        }
        if inv.execution != ExecutionObservation::KnownRustcStarted || inv.finished.is_none() {
            accounting = JobAccounting::Incomplete;
        }
        let id = digest(&serde_json::to_vec(&(run, &inv.id)).unwrap());
        let builtin = cfgs
            .get(&inv.id)
            .map(|(value, _)| value.clone())
            .unwrap_or_else(|| Observed::Unknown("cfg not probed".into()));
        let coverage = if builtin.value().is_some() && a.gaps.is_empty() {
            ConfigurationCoverage::ExplicitAndCompilerCfg
        } else {
            ConfigurationCoverage::Partial
        };
        let config = RustConfiguration {
            explicit_cfg: a.cfg.clone(),
            expected_cfg: a.check_cfg.clone(),
            compiler_cfg: builtin,
            compiler_cfg_evidence: cfgs.get(&inv.id).map(|(_, index)| EvidenceRef {
                run_id: run.into(),
                source: "report.probes".into(),
                index: Some(*index),
            }),
            test: a.test,
            edition: observed(a.edition.clone(), "compiler default not reconstructed"),
            options: a.raw.clone(),
            coverage,
            gaps: a.gaps.clone(),
        };
        gaps.extend(a.gaps.clone());
        let host = tool
            .map(|t| t.host.clone())
            .unwrap_or_else(|| Observed::Unknown("host not observed".into()));
        let target = a
            .target
            .clone()
            .map(Observed::Value)
            .unwrap_or_else(|| host.clone());
        units.push(BuildUnit {
            id,
            language: Language::Rust,
            name: observed(a.name.clone(), "missing name"),
            root: observed(a.root.clone(), "missing root"),
            output_kinds: a.types.clone(),
            role: role(&a.types, a.test),
            host,
            target,
            configuration: ConfigurationObservation::Rust(config),
            invocation: Observed::Value(inv.id.clone()),
            artifacts: vec![],
            binding: UnitBinding::ObservedInvocation,
            evidence: inv.evidence.clone(),
        });
        arguments.insert(inv.id.clone(), a);
    }
    for artifact in artifacts.iter_mut() {
        let candidates: Vec<_> = units
            .iter()
            .filter(|u| {
                let Some(inv) = u.invocation.value() else {
                    return false;
                };
                let a = &arguments[inv];
                u.root.value() == Some(&artifact.src_path)
                    && a.test == artifact.profile.test
                    && (artifact.paths.iter().any(|p| args::output_matches(a, p))
                        || artifact
                            .output_aliases
                            .iter()
                            .any(|alias| &alias.invocation_id == inv))
            })
            .map(|u| u.id.clone())
            .collect();
        artifact.producer = match candidates.len() {
            1 => Observed::Value(candidates[0].clone()),
            0 => Observed::Unknown(
                if artifact.fresh {
                    "cached artifact; no invocation attributed"
                } else {
                    "no exact output identity matched"
                }
                .into(),
            ),
            _ => Observed::Ambiguous(candidates.clone()),
        };
        if let Some(id) = artifact.producer.value() {
            let unit = units.iter_mut().find(|u| &u.id == id).unwrap();
            unit.artifacts.push(artifact.id.clone());
            if artifact.target_kinds.iter().any(|s| s == "custom-build") {
                unit.role = UnitRole::BuildScript;
            }
        } else {
            if !artifact.fresh {
                accounting = JobAccounting::Incomplete;
                gaps.push(Gap::new(
                    "ArtifactProducerUnresolved",
                    &artifact.id,
                    "a non-fresh Cargo artifact has no unique observed producer",
                ));
            }
            let id = digest(&serde_json::to_vec(&(run, &artifact.id, "artifact-unit")).unwrap());
            units.push(BuildUnit {
                id,
                language: Language::Rust,
                name: Observed::Value(artifact.target_name.replace('-', "_")),
                root: Observed::Value(artifact.src_path.clone()),
                output_kinds: vec![],
                role: if artifact.target_kinds.iter().any(|s| s == "custom-build") {
                    UnitRole::BuildScript
                } else {
                    UnitRole::Unknown
                },
                host: Observed::Unknown("no invocation".into()),
                target: Observed::Unknown("no invocation".into()),
                configuration: ConfigurationObservation::Rust(RustConfiguration {
                    explicit_cfg: vec![],
                    expected_cfg: vec![],
                    compiler_cfg: Observed::Unknown(
                        "cached artifact has no current compiler arguments".into(),
                    ),
                    compiler_cfg_evidence: None,
                    test: artifact.profile.test,
                    edition: Observed::Unknown("no invocation".into()),
                    options: vec![],
                    coverage: ConfigurationCoverage::Partial,
                    gaps: vec![Gap::new(
                        "CachedConfigurationUnknown",
                        &artifact.id,
                        "Cargo features are retained on artifact; not a complete cfg observation",
                    )],
                }),
                invocation: Observed::Unknown("no unique current invocation".into()),
                artifacts: vec![artifact.id.clone()],
                binding: if artifact.fresh {
                    UnitBinding::ReusedArtifact
                } else {
                    UnitBinding::UnattributedArtifact
                },
                evidence: vec![artifact.evidence.clone()],
            });
        }
    }
    let mut dependencies = vec![];
    for unit in units
        .iter()
        .filter(|u| u.binding == UnitBinding::ObservedInvocation)
    {
        let a = &arguments[unit.invocation.value().unwrap()];
        for (alias, path) in &a.externs {
            let matched: Vec<_> = artifacts
                .iter()
                .filter(|a| a.paths.contains(path))
                .collect();
            let mut producers: Vec<String> = matched
                .iter()
                .filter_map(|a| a.producer.value().cloned())
                .collect();
            // Cargo may report the rlib while rustc consumes rmeta. The exact
            // out-dir / crate-name / extra-filename tuple is another output identity.
            producers.extend(
                units
                    .iter()
                    .filter(|u| {
                        u.invocation
                            .value()
                            .is_some_and(|i| args::output_matches(&arguments[i], path))
                    })
                    .map(|u| u.id.clone()),
            );
            producers.sort();
            producers.dedup();
            let producer = match producers.len() {
                1 => Observed::Value(producers[0].clone()),
                0 => Observed::Unknown(
                    "external artifact has no uniquely identified producer in this run".into(),
                ),
                _ => Observed::Ambiguous(producers),
            };
            let artifact = match matched.len() {
                1 => Observed::Value(matched[0].id.clone()),
                0 => Observed::Unknown("path not reported by Cargo".into()),
                _ => Observed::Ambiguous(matched.iter().map(|a| a.id.clone()).collect()),
            };
            dependencies.push(DependencyBinding {
                consumer: unit.id.clone(),
                alias: alias.clone(),
                path: path.clone(),
                producer,
                artifact,
                evidence: unit.evidence.clone(),
            });
        }
    }
    if !cargo_finished {
        accounting = JobAccounting::Incomplete;
        gaps.push(Gap::new(
            "CargoBuildFinishedMissing",
            run,
            "no build-finished Cargo message; early build failure or output loss",
        ));
    }
    Normalized {
        units,
        dependencies,
        arguments,
        gaps,
        accounting,
    }
}
