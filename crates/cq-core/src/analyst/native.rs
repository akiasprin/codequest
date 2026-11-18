//! Common observed process boundary for non-Cargo build tools.
use crate::{Error, Result, control::CaptureControl, model::*, ports::*};
use std::{process::Command, sync::Arc};
pub fn observe_command(
    command: Command,
    language: Language,
    run: &str,
    journal: Arc<dyn CaptureJournal>,
    control: &CaptureControl,
) -> Result<BuildObservation> {
    let program = NativeValue::new(command.get_program());
    let arguments = command.get_args().map(NativeValue::new).collect();
    let cwd = NativeValue::new(
        command
            .get_current_dir()
            .unwrap_or(std::path::Path::new(".")),
    );
    let result =
        super::rust::process::run_cargo(command, journal, control, &CaptureBudget::default())?;
    let outcome = if control.is_cancelled() {
        BuildOutcome::Cancelled
    } else if result.exit.code == Some(0) && result.stop.is_none() {
        BuildOutcome::Succeeded
    } else {
        BuildOutcome::Failed
    };
    Ok(BuildObservation {
        id: run.into(),
        language,
        command: program,
        arguments,
        cwd,
        outcome,
        exit: result.exit,
        events: vec![],
        rejected_fields: vec![],
        captured_report: None,
        gaps: result
            .stop
            .into_iter()
            .map(|reason| Gap::new("BuildStopped", run, reason))
            .collect(),
    })
}
pub fn seal_events(journal: &dyn CaptureJournal) -> Result<Vec<Envelope>> {
    let seal = journal.seal()?;
    let SealOutcome::Sealed(receipt) = seal else {
        return Err(Error::new(
            "CaptureUnsealed",
            "native build journal did not seal",
        ));
    };
    if receipt.integrity != JournalIntegrity::Sound || !journal.health().issues.is_empty() {
        return Err(Error::new(
            "CaptureIncomplete",
            "native compiler capture contains gaps",
        ));
    }
    let mut events = vec![];
    let mut at = 0;
    loop {
        let page = journal.read(&receipt, at, 1024)?;
        events.extend(page.items);
        if let Some(next) = page.next {
            at = next
        } else {
            break;
        }
    }
    Ok(events)
}
pub struct NativeUnitInput {
    pub language: Language,
    pub run: String,
    pub name: String,
    pub root: NativeValue,
    pub arguments: Vec<NativeValue>,
    pub cwd: NativeValue,
    pub files: Vec<NativeValue>,
    pub invocation: Observed<String>,
}
pub fn unit(input: NativeUnitInput) -> BuildUnit {
    let NativeUnitInput {
        language,
        run,
        name,
        root,
        arguments: args,
        cwd,
        files,
        invocation,
    } = input;
    let id = digest(&serde_json::to_vec(&(&run, &name, &args, &root)).expect("unit identity"));
    let binding = if invocation.value().is_some() {
        UnitBinding::ObservedInvocation
    } else {
        UnitBinding::UnattributedArtifact
    };
    BuildUnit {
        id,
        language,
        name: Observed::Value(name),
        root: Observed::Value(root),
        output_kinds: vec![],
        role: UnitRole::Library,
        host: Observed::Unknown("see captured compiler environment".into()),
        target: Observed::Unknown("see captured compiler options".into()),
        configuration: ConfigurationObservation::Native(NativeConfiguration {
            language,
            arguments: args,
            environment: Default::default(),
            selected_files: files,
            working_directory: cwd,
        }),
        invocation,
        artifacts: vec![],
        binding,
        evidence: vec![EvidenceRef {
            run_id: run,
            source: "events.log".into(),
            index: None,
        }],
    }
}
