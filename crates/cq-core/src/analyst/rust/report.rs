use super::{args, inputs::Inputs, plan, process, toolchain, units};
use crate::{Error, Result, analyst::BuildObserver, control::CaptureControl, model::*, ports::*};
use std::{collections::HashMap, process::Command, sync::Arc, time::Instant};

pub struct RustBuildObserver;
impl BuildObserver for RustBuildObserver {
    type Request = super::RustBuildRequest;
    fn capabilities(&self) -> CaptureCapabilities {
        CaptureCapabilities {
            language: Language::Rust,
            exclusive_build: true,
            preserve_existing_chain: false,
            semantic_relations: false,
            immutable_compiler_inputs: false,
        }
    }
    fn plan(&self, request: Self::Request) -> Result<CapturePlan> {
        plan::plan(request)
    }
    fn observe(
        &self,
        plan: CapturePlan,
        journal: Arc<dyn CaptureJournal>,
        content: &dyn ContentRepository,
        control: &CaptureControl,
    ) -> Result<CaptureReport> {
        let started_ms = cq_capture_protocol::now_ms();
        let start = Instant::now();
        let mut probes = vec![];
        let mut gaps = vec![];
        if digest(&std::fs::read(plan.recorder.path())?) != plan.recorder_digest {
            return Err(Error::new(
                "RecorderChanged",
                "recorder changed after planning",
            ));
        }
        let compiler = std::env::var_os("RUSTC")
            .map(NativeValue::new)
            .unwrap_or_else(|| NativeValue::from("rustc"));
        let tool = toolchain::observe(
            compiler.clone(),
            &plan.project,
            plan.budget.probe_timeout_ms,
            &mut probes,
            &plan.run_id,
        );
        let metadata = metadata(&plan, &mut probes);
        let before = Inputs::before(
            &plan.project.path(),
            metadata.as_ref(),
            content,
            plan.budget.content_bytes,
        );
        let mut cmd = Command::new(plan.build_tool.os());
        cmd.current_dir(plan.project.path())
            .args(&plan.effective_args);
        for change in &plan.environment_changes {
            cmd.env(&change.key, change.effective.os());
        }
        cmd.env(cq_capture_protocol::ENDPOINT_ENV, journal.endpoint())
            .env(cq_capture_protocol::RUN_ENV, &plan.run_id)
            .env(
                "CQ_CAPTURE_FRAME_BYTES",
                plan.budget.frame_bytes.to_string(),
            )
            .env(
                "CQ_CAPTURE_REPORT_MS",
                plan.budget.report_timeout_ms.to_string(),
            )
            .env(
                "CQ_CAPTURE_REPORT_TOTAL_MS",
                plan.budget.invocation_report_timeout_ms.to_string(),
            )
            .env(
                "CQ_CAPTURE_ARGUMENT_FILE_BYTES",
                plan.budget.argument_file_bytes.to_string(),
            )
            .env(
                "CQ_CAPTURE_ARGUMENT_TOTAL_BYTES",
                plan.budget.argument_total_bytes.to_string(),
            )
            .env(
                "CQ_CAPTURE_ENV_KEYS",
                serde_json::to_string(&plan.extra_env_keys)?,
            );
        let unsupported = std::env::var_os("RUSTC").is_some() && tool.version.value().is_none();
        let result = if unsupported {
            gaps.push(Gap::new(
                "UnsupportedToolchain",
                &plan.run_id,
                "explicit RUSTC failed identification; compilation was not started",
            ));
            process::ProcessResult {
                exit: ExitObservation {
                    code: None,
                    signal: None,
                    spawn_error: Some("UnsupportedToolchain".into()),
                },
                stop: None,
                duration_ms: 0,
            }
        } else if control.is_cancelled() {
            process::ProcessResult {
                exit: ExitObservation {
                    code: None,
                    signal: None,
                    spawn_error: None,
                },
                stop: Some("user_cancelled".into()),
                duration_ms: 0,
            }
        } else {
            process::run_cargo(cmd, journal.clone(), control, &plan.budget)?
        };
        if let Err(e) = journal.write_output(OutputStream::Probes, &serde_json::to_vec(&probes)?) {
            gaps.push(Gap::new(
                "ProbeJournalUnavailable",
                &plan.run_id,
                e.to_string(),
            ));
        }
        let seal_started = Instant::now();
        let mut seal = journal.seal()?;
        if matches!(seal, SealOutcome::Unknown(_)) {
            let recovered = journal.recover()?;
            if matches!(recovered, SealOutcome::Sealed(_)) {
                seal = recovered;
            }
        }
        let seal_duration_ms = seal_started.elapsed().as_millis() as u64;
        let mut events = vec![];
        let mut stdout = vec![];
        if let SealOutcome::Sealed(receipt) = &seal {
            let mut offset = 0;
            loop {
                let page = journal.read(receipt, offset, 128)?;
                events.extend(page.items);
                if let Some(next) = page.next {
                    offset = next;
                } else {
                    break;
                }
            }
            stdout = journal.output(receipt, OutputStream::Stdout)?;
        } else {
            gaps.push(Gap::new(
                "CaptureNotSealed",
                &plan.run_id,
                "raw events cannot be promoted to verified observations",
            ));
        }
        let mut invocations = units::invocations(events);
        let (build_messages, mut artifacts, message_gaps, finished) =
            units::build_messages(&stdout, &plan.run_id);
        gaps.extend(message_gaps);
        super::outputs::collect_aliases(&invocations, &mut artifacts);
        let mut tools = HashMap::from([(compiler, tool.clone())]);
        let mut cfgs = HashMap::new();
        let mut cfg_cache = HashMap::new();
        for inv in &invocations {
            let Some(Envelope {
                event: Event::InvocationStarted { command, cwd, .. },
                ..
            }) = &inv.started
            else {
                continue;
            };
            if !tools.contains_key(command) {
                tools.insert(
                    command.clone(),
                    toolchain::observe(
                        command.clone(),
                        cwd,
                        plan.budget.probe_timeout_ms,
                        &mut probes,
                        &plan.run_id,
                    ),
                );
            }
            let args = args::inspect(inv);
            if !args.probe && args.name.is_some() && tools[command].version.value().is_some() {
                let key = serde_json::to_vec(&(command, cwd, &args.cfg_probe_args))?;
                let value = cfg_cache
                    .entry(key)
                    .or_insert_with(|| {
                        let index = probes.len();
                        let value = toolchain::cfg(
                            command,
                            cwd,
                            args.cfg_probe_args,
                            plan.budget.probe_timeout_ms,
                            &mut probes,
                        );
                        (value, index)
                    })
                    .clone();
                cfgs.insert(inv.id.clone(), value);
            }
        }
        let normalized = units::normalize(
            &plan.run_id,
            &mut invocations,
            &mut artifacts,
            &tools,
            &cfgs,
            finished,
        );
        gaps.extend(normalized.gaps);
        for inv in &invocations {
            if inv.execution == ExecutionObservation::UnidentifiedProcessStarted {
                gaps.push(Gap::new(
                    "UnsupportedToolchain",
                    &inv.id,
                    "downstream process ran but was not identified as rustc",
                ));
            }
        }
        let snapshot = before.after(
            normalized.arguments.into_values(),
            &build_messages,
            content,
            plan.budget.content_bytes,
        );
        let outcome = if result.stop.as_deref() == Some("user_cancelled") {
            BuildOutcome::Cancelled
        } else if result.exit.spawn_error.is_some() {
            BuildOutcome::NotStarted
        } else if result.exit.code == Some(0) {
            BuildOutcome::Succeeded
        } else {
            BuildOutcome::Failed
        };
        let accounting = if matches!(seal, SealOutcome::Sealed(_)) {
            normalized.accounting
        } else {
            JobAccounting::Unavailable
        };
        let run = BuildRun {
            id: plan.run_id.clone(),
            started_ms,
            ended_ms: cq_capture_protocol::now_ms(),
            outcome,
            tool_exit: result.exit,
            stop_reason: result.stop,
            build_duration_ms: result.duration_ms,
            seal_duration_ms,
        };
        let mut toolchains: Vec<_> = tools.into_values().collect();
        toolchains.sort_by(|a, b| a.command.cmp(&b.command));
        let report = CaptureReport {
            schema_version: 1,
            plan,
            run,
            seal,
            health: journal.health(),
            probes,
            preflight_toolchain: tool,
            toolchains,
            invocations,
            artifacts,
            units: normalized.units,
            dependencies: normalized.dependencies,
            snapshot,
            build_messages,
            job_accounting: accounting,
            gaps,
            duration_ms: start.elapsed().as_millis() as u64,
        };
        super::validation::validate_report(&report, content)?;
        Ok(report)
    }
}
fn metadata(plan: &CapturePlan, probes: &mut Vec<ProbeObservation>) -> Option<serde_json::Value> {
    let prefix = usize::from(
        plan.effective_args
            .first()
            .is_some_and(|s| s.starts_with('+')),
    );
    let mut args = plan.effective_args[..prefix].to_vec();
    args.extend([
        "metadata".into(),
        "--format-version=1".into(),
        "--no-deps".into(),
    ]);
    let mut iter = plan.effective_args[prefix + 1..].iter();
    while let Some(a) = iter.next() {
        if [
            "--locked",
            "--frozen",
            "--offline",
            "--all-features",
            "--no-default-features",
        ]
        .contains(&a.as_str())
            || a.starts_with("--features=")
        {
            args.push(a.clone());
        } else if a == "--features"
            && let Some(v) = iter.next()
        {
            args.extend([a.clone(), v.clone()]);
        }
    }
    let mut cmd = Command::new(plan.build_tool.os());
    cmd.current_dir(plan.project.path());
    let p = process::probe(cmd, args, plan.budget.probe_timeout_ms);
    let result = if p.exit.code == Some(0) && p.exit.spawn_error.is_none() {
        serde_json::from_slice(&p.output.0).ok()
    } else {
        None
    };
    probes.push(p);
    result
}
