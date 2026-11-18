use cq_core::{model::*, ports::facts::CommitOutcome};
use cq_store::{LocalStore, facts::writer, revisions::StoreHost};
#[allow(dead_code)]
#[path = "common/paged_fixture.rs"]
mod paged_fixture;

fn build(events: Vec<Envelope>, run: &str) -> BuildObservation {
    BuildObservation {
        id: run.into(),
        language: Language::Rust,
        command: NativeValue::from("cargo"),
        arguments: vec![],
        cwd: NativeValue::from("/repo"),
        outcome: BuildOutcome::Succeeded,
        exit: ExitObservation {
            code: Some(0),
            signal: None,
            spawn_error: None,
        },
        events,
        rejected_fields: vec![],
        captured_report: Some(run.into()),
        gaps: vec![],
    }
}
fn check_rejection(mut snapshot: SemanticSnapshot, store: &LocalStore, events: &[Envelope]) {
    let hash = digest(&serde_json::to_vec(events).unwrap());
    let source = store
        .run_path(&snapshot.builds[0].id)
        .unwrap()
        .join("report.json");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(
        &source,
        serde_json::to_vec(&serde_json::json!({"invocations":events})).unwrap(),
    )
    .unwrap();
    snapshot.revision.clear();
    cq_core::progress::Progress::begin(&store.root, &snapshot.project);
    let prepared = store.prepare_analysis(snapshot).unwrap();
    let retained = &prepared.header.builds[0];
    assert!(retained.events.is_empty());
    assert_eq!(retained.rejected_fields.len(), 1);
    assert_eq!(retained.rejected_fields[0].sha256, hash);
    assert_eq!(retained.rejected_fields[0].originals[0].path(), source);
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(retained.rejected_fields[0].report.path()).unwrap())
            .unwrap();
    assert_eq!(
        report["capture_report"]["report_path"],
        source.to_str().unwrap()
    );
    assert!(
        prepared
            .header
            .gaps
            .iter()
            .any(|g| g.code == "OversizedValueRejected")
    );
    let host = StoreHost::open(&store.root, &prepared.header.project).unwrap();
    let revision = writer::scoped(&host.objects, &store.content, &prepared).unwrap();
    host.objects.sync_pending().unwrap();
    let outcome = host.publish(&revision, None);
    assert_eq!(outcome.outcome, CommitOutcome::Committed, "{outcome:?}");
    assert_eq!(revision.metadata["completion"], "complete_with_warnings");
    assert_eq!(
        revision.metadata["storage_rejections"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let exported = writer::export(&host.objects, &revision).unwrap();
    assert_eq!(exported.builds[0].rejected_fields[0].sha256, hash);
    assert!(!exported.entities.is_empty());
    assert!(!exported.relations.is_empty());
    assert!(
        exported
            .gaps
            .iter()
            .any(|g| g.code == "OversizedValueRejected")
    );
}
#[test]
fn rejected_build_events_warn_but_other_facts_publish_to_real_ladybug() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let events = vec![Envelope {
        protocol_version: 1,
        run_id: "11111111111111111111111111111111".into(),
        invocation_id: "compiler-1".into(),
        seq: 0,
        time_ms: 1,
        event: Event::ArgumentFileObserved {
            path: NativeValue::from("args.rsp"),
            contents: Some(NativeValue::from("x".repeat(8 * 1024 * 1024).as_str())),
            error: None,
        },
    }];
    let mut s = paged_fixture::fixture(&store, 3, 1);
    s.builds
        .push(build(events.clone(), "11111111111111111111111111111111"));
    check_rejection(s, &store, &events);
}
#[test]
#[ignore = "manual real Brocade metadata check; CQ_REAL_CAPTURE_REPORT"]
fn real_brocade_build_metadata_continues_without_semantic_replay() {
    let input = std::env::var("CQ_REAL_CAPTURE_REPORT").unwrap();
    let report: CaptureReport =
        serde_json::from_reader(std::fs::File::open(input).unwrap()).unwrap();
    let events = report
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
        .collect::<Vec<_>>();
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let mut s = paged_fixture::fixture(&store, 3, 1);
    s.builds.push(build(events.clone(), &report.run.id));
    check_rejection(s, &store, &events);
}
