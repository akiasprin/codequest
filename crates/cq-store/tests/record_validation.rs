use cq_core::{
    Result,
    semantic::{
        record_projection,
        records::{Record, Source, Store},
    },
};
use cq_store::{LocalStore, raw::Journal};
use std::sync::Mutex;

#[allow(dead_code)]
#[path = "common/paged_fixture.rs"]
mod paged_fixture;

// A broken container link must fail before reading any other family. This
// models the real Brocade failure without replaying millions of unrelated rows.
struct EntityOnly<'a> {
    journal: &'a Journal,
    visits: Mutex<Vec<String>>,
}
impl Source for EntityOnly<'_> {
    fn visit(&self, _: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        panic!("invalid parent should be rejected before a full scan");
    }
    fn visit_family(&self, family: &str, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        assert_eq!(family, "entity");
        self.visits.lock().unwrap().push(family.into());
        self.journal.visit_family(family, f)
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>> {
        self.journal.get(family, id)
    }
}

#[test]
fn dangling_parent_fails_after_entities_with_actionable_context() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let mut snapshot = paged_fixture::fixture(&store, 3, 1);
    snapshot.entities[0].parent = Some("missing-native-container".into());
    let journal = Journal::new(temp.path().into()).unwrap();
    cq_core::semantic::records::drain(&mut snapshot, &journal).unwrap();
    snapshot.revision.clear();
    let source = EntityOnly {
        journal: &journal,
        visits: Mutex::new(Vec::new()),
    };
    let error = record_projection::validate(&snapshot, &source, &store.content).unwrap_err();
    assert_eq!(error.code(), "InvalidAnalysisRecords", "{error}");
    assert!(error.to_string().contains("missing-native-container"));
    assert!(error.to_string().contains("test::f0"));
    assert_eq!(*source.visits.lock().unwrap(), ["entity"]);
    // Removing the invalid reference allows the complete validator to run.
    let Some(Record::Entity(mut entity)) = journal.get("entity", "f0").unwrap() else {
        panic!()
    };
    entity.parent = None;
    journal
        .put(&snapshot, Record::Entity(entity), true)
        .unwrap();
    record_projection::validate(&snapshot, &journal, &store.content).unwrap();
}

#[test]
fn prepared_spool_survives_releasing_the_raw_analysis_journal() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let scoped = store.streaming_scope().unwrap();
    let mut snapshot = paged_fixture::fixture(&scoped, 3, 1);
    snapshot.revision.clear();
    let raw = scoped.analysis_records().unwrap();
    cq_core::semantic::records::drain(&mut snapshot, raw).unwrap();
    let prepared = scoped.prepare_records(snapshot, raw).unwrap();
    let scoped = scoped.finish_streaming();
    assert!(scoped.analysis_records().is_none());
    let mut entities = 0;
    prepared
        .spool
        .each("entity", |_, _| {
            entities += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(entities, 3);
    assert!(store.analysis_records().is_none());
}
