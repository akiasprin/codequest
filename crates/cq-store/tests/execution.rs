use cq_core::{
    execution::{Stage, StageState},
    model::*,
    ports::AnalysisWorkspace,
    semantic::records::{Record, Source, Store},
};
use cq_store::{LocalStore, execution::Execution};
use serde_json::json;
use std::sync::Arc;
fn document(id: &str) -> Record {
    Record::Document(DocumentBlock {
        id: id.into(),
        file_revision: "file".into(),
        kind: DocumentKind::Markdown,
        title: id.into(),
        text: "text".into(),
        source: SourceAnchor {
            file_revision: "file".into(),
            start: 0,
            end: 4,
        },
        owner: None,
        parent: None,
        links: vec![],
        provenance: ProvenanceId("prov".into()),
    })
}
fn point(store: &LocalStore, stage: Stage, header: &mut SemanticSnapshot) {
    assert!(!store.start_stage(stage, header).unwrap().reused);
    store.finish_stage(stage, header, json!(null)).unwrap();
}
#[test]
fn immutable_record_generations_restore_replacements_and_discard_unsealed_tails() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let request = json!({"name":"p"});
    let source = json!({"file":"hash"});
    let execution = Arc::new(
        Execution::create(
            &store.root,
            "p",
            &source,
            request,
            vec![Stage::Capture, Stage::Relations, Stage::Documents],
            None,
        )
        .unwrap(),
    );
    let id = execution.id();
    let s = store.execution_scope(execution).unwrap();
    let mut header = SemanticSnapshot::empty("p".into(), NativeValue::new(tmp.path()));
    point(&s, Stage::Capture, &mut header);
    assert!(!s.start_stage(Stage::Relations, &mut header).unwrap().reused);
    let record = document("doc");
    s.analysis_records()
        .unwrap()
        .put(&header, record, false)
        .unwrap();
    s.finish_stage(Stage::Relations, &header, json!(null))
        .unwrap();
    assert!(!s.start_stage(Stage::Documents, &mut header).unwrap().reused);
    let mut replacement = document("doc");
    if let Record::Document(d) = &mut replacement {
        d.text = "second".into();
    }
    s.analysis_records()
        .unwrap()
        .put(&header, replacement, true)
        .unwrap();
    s.checkpoint_stage(Stage::Documents, &header, json!({"batch":1}), 1, 2)
        .unwrap();
    // A crash may leave appended records. They must not enter the restored
    // generation just because their bytes reached the filesystem.
    let record = document("tail");
    s.analysis_records()
        .unwrap()
        .put(&header, record, false)
        .unwrap();
    drop(s);
    let execution = Arc::new(Execution::open(&store.root, &id, &source, None, None).unwrap());
    let s = store.execution_scope(execution).unwrap();
    assert!(s.start_stage(Stage::Relations, &mut header).unwrap().reused);
    assert!(
        s.analysis_records()
            .unwrap()
            .get("document", "doc")
            .unwrap()
            .is_some()
    );
    assert!(
        s.analysis_records()
            .unwrap()
            .get("document", "tail")
            .unwrap()
            .is_none()
    );
    assert!(!s.start_stage(Stage::Documents, &mut header).unwrap().reused);
    let doc = s
        .analysis_records()
        .unwrap()
        .get("document", "doc")
        .unwrap()
        .unwrap();
    assert_eq!(doc.value().unwrap()["text"], "second");
    assert!(
        s.analysis_records()
            .unwrap()
            .get("document", "tail")
            .unwrap()
            .is_none()
    );
}
#[test]
fn rerun_invalidates_descendants_but_keeps_history_and_rejects_other_owners_and_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let source = json!({"x":"a"});
    let e = Arc::new(
        Execution::create(
            &store.root,
            "p",
            &source,
            json!({}),
            vec![Stage::Capture, Stage::Relations, Stage::Documents],
            None,
        )
        .unwrap(),
    );
    let id = e.id();
    assert!(Execution::open(&store.root, &id, &source, None, None).is_err());
    let s = store.execution_scope(e).unwrap();
    let mut h = SemanticSnapshot::empty("p".into(), NativeValue::new(tmp.path()));
    for stage in [Stage::Capture, Stage::Relations, Stage::Documents] {
        point(&s, stage, &mut h);
    }
    drop(s);
    assert!(Execution::open(&store.root, &id, &json!({"x":"b"}), None, None).is_err());
    let e =
        Arc::new(Execution::open(&store.root, &id, &source, Some(Stage::Relations), None).unwrap());
    let s = store.execution_scope(e).unwrap();
    assert!(s.start_stage(Stage::Capture, &mut h).unwrap().reused);
    point(&s, Stage::Relations, &mut h);
    let status = Execution::status(&store.root, &id).unwrap();
    assert_eq!(status["stages"][1]["history"].as_array().unwrap().len(), 2);
    assert_eq!(
        status["stages"][2]["attempt"]["state"],
        serde_json::to_value(StageState::Stale).unwrap()
    );
}
