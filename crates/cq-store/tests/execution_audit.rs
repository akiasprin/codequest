use cq_core::{
    execution::Stage,
    ports::facts::{CommitOutcome, ReadStats},
};
use cq_store::{
    LocalStore,
    facts::{
        objects::Objects,
        writer::{self, Revision, Writer},
    },
    revisions::StoreHost,
};
use serde_json::json;
#[test]
fn completed_audit_stages_do_not_hide_changed_physical_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let objects = Objects::buffered(store.root.join("format-2/objects")).unwrap();
    let mut writer = Writer::initial(&objects).unwrap();
    for id in ["a", "b"] {
        writer
            .append(
                "entity",
                id,
                &json!({"id":id,"name":id,"kind":"Function","unit":"u","external":false}),
            )
            .unwrap();
    }
    writer
        .append(
            "relation",
            "r",
            &json!({"id":"r","from":"a","to":"b","kind":"Calls"}),
        )
        .unwrap();
    let (roots, changes) = writer.finish_inventory().unwrap();
    objects.sync_pending().unwrap();
    let mut revision = Revision {
        format: 2,
        project: "audit-resume".into(),
        revision: "d".repeat(64),
        parent: None,
        sequence: 1,
        roots,
        changes,
        metadata: json!({}),
        seal: String::new(),
    };
    revision.metadata = json!({"project":revision.project,"revision":revision.revision});
    revision.seal().unwrap();
    let host = StoreHost::open(&store.root, &revision.project).unwrap();
    for stage in [
        Stage::AuditInventory,
        Stage::AuditPayloads,
        Stage::AuditDirectories,
        Stage::AuditScopes,
    ] {
        host.audit_stage(&revision, stage).unwrap();
    }
    let f = writer::reference(
        &objects,
        &revision.roots,
        "entity",
        "a",
        &mut ReadStats::default(),
    )
    .unwrap()
    .unwrap();
    let bytes = objects.read(&f.object, &mut ReadStats::default()).unwrap();
    let mut bad = bytes.as_ref().clone();
    bad[0] ^= 1;
    let path = objects.path(&f.object).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bad).unwrap();
    let result = host.publish(&revision, None);
    assert_eq!(result.outcome, CommitOutcome::NotCommitted);
    assert!(result.detail.unwrap().contains("DigestMismatch"));
    assert_eq!(host.head().unwrap(), None);
    std::fs::write(objects.path(&f.object).unwrap(), bytes.as_ref()).unwrap();
    let repaired = host.publish(&revision, None);
    assert_eq!(
        repaired.outcome,
        CommitOutcome::Committed,
        "{:?}",
        repaired.detail
    );
}
#[test]
fn staged_import_cannot_rebind_an_admitted_revision_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let objects = Objects::buffered(store.root.join("format-2/objects")).unwrap();
    let mut writer = Writer::initial(&objects).unwrap();
    writer
        .append(
            "entity",
            "a",
            &json!({"id":"a","name":"a","kind":"Function","unit":"u","external":false}),
        )
        .unwrap();
    let (roots, changes) = writer.finish_inventory().unwrap();
    objects.sync_pending().unwrap();
    let mut r = Revision {
        format: 2,
        project: "receipt-binding".into(),
        revision: "c".repeat(64),
        parent: None,
        sequence: 1,
        roots,
        changes,
        metadata: json!({}),
        seal: String::new(),
    };
    r.metadata = json!({"project":r.project,"revision":r.revision});
    r.seal().unwrap();
    let host = StoreHost::open(&store.root, &r.project).unwrap();
    let published = host.publish(&r, None);
    assert_eq!(
        published.outcome,
        CommitOutcome::Committed,
        "{:?}",
        published.detail
    );
    let mut conflict = r.clone();
    conflict.metadata["changed"] = json!(true);
    conflict.seal().unwrap();
    for stage in [
        Stage::AuditInventory,
        Stage::GraphInventory,
        Stage::GraphFacts,
    ] {
        let error = host
            .execute_stage(&conflict, stage, &Default::default())
            .unwrap_err();
        assert_eq!(error.code(), "RevisionConflict");
    }
    assert_eq!(host.head().unwrap().as_deref(), Some(r.revision.as_str()));
    drop(host);
    // A reopened host verifies the original receipt before admitting HEAD.
    let reopened = StoreHost::open(&store.root, &r.project).unwrap();
    assert_eq!(
        reopened.head().unwrap().as_deref(),
        Some(r.revision.as_str())
    );
}
