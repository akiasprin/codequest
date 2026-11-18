//! Replay captured metadata without native exports; optionally publish it.
use cq_core::{
    model::*,
    ports::facts::{CommitOutcome, FactReader},
    semantic,
};
use cq_store::{LocalStore, facts::writer, raw::Journal, revisions::StoreHost};
use std::{collections::HashSet, io::BufReader, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let path = std::env::args_os()
        .nth(1)
        .ok_or("capture report path required")?;
    let report: CaptureReport =
        serde_json::from_reader(BufReader::new(std::fs::File::open(path)?))?;
    let temp = tempfile::tempdir()?;
    let store = LocalStore::open(temp.path())?;
    let mut snapshot = SemanticSnapshot::empty(
        "compilation-metadata-audit".into(),
        report.plan.project.clone(),
    );
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
        events: vec![],
        captured_report: Some(report.run.id.clone()),
        gaps: vec![],
    });
    snapshot.units = report.units;
    snapshot.build_dependencies = report.dependencies;
    snapshot.production = Some(Default::default());
    let identities =
        semantic::identity::compilation_identities(&snapshot.units, &snapshot.root.display())?;
    let scoped = identities.observation_scoped.len();
    let observations = snapshot.units.len();
    let journal = Journal::new(temp.path().into())?;
    semantic::stream_ops::topology(&mut snapshot, &journal)?;
    let prepared = store.prepare_records(snapshot, &journal)?;
    // Validate observation bindings after canonical IDs have replaced raw IDs.
    let empty = Journal::new(temp.path().into())?;
    semantic::record_projection::validate(&prepared.header, &empty, &store.content)?;
    assert_eq!(prepared.header.units.len(), observations);
    assert_eq!(
        prepared
            .header
            .units
            .iter()
            .map(|u| &u.id)
            .collect::<HashSet<_>>()
            .len(),
        observations
    );
    let mut forged = prepared.header.clone();
    let p = forged.production.as_mut().unwrap();
    p.unit_observations[0].unit = p.unit_observations[1].unit.clone();
    assert_eq!(
        semantic::record_projection::validate(&forged, &empty, &store.content)
            .unwrap_err()
            .code(),
        "InvalidObservationBinding"
    );
    assert_eq!(
        prepared
            .header
            .production
            .as_ref()
            .unwrap()
            .unit_observations
            .len(),
        observations
    );
    let publish = std::env::args().skip(2).any(|a| a == "--publish");
    if publish {
        let host = StoreHost::open(temp.path(), &prepared.header.project)?;
        let objects = cq_store::facts::objects::Objects::buffered(host.objects.root.clone())?;
        let revision = writer::scoped(&objects, &store.content, &prepared)?;
        objects.sync_pending()?;
        let outcome = host.publish(&revision, None);
        if outcome.outcome != CommitOutcome::Committed {
            let retained = temp.keep();
            return Err(format!(
                "publication failed: {outcome:?}; retained diagnostic store {}",
                retained.display()
            )
            .into());
        }
        let view = host.open_view(None)?;
        assert_eq!(view.count("all:unit")?, observations as u64);
    }
    println!(
        "{}",
        serde_json::json!({"seconds":started.elapsed().as_secs_f64(),"observations":observations,"contexts":prepared.header.units.len(),"observation_scoped":scoped,"facts":prepared.spool.counts(),"published_and_read_back":publish,"scope":"captured compilation metadata and topology only; no HIR replay or repository semantic facts"})
    );
    Ok(())
}
