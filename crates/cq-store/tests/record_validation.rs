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

struct CountedProgress<'a> {
    journal: &'a Journal,
    phases: Mutex<std::collections::BTreeMap<String, (u64, u64)>>,
}
impl Source for CountedProgress<'_> {
    fn visit(&self, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        self.journal.visit(f)
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>> {
        self.journal.get(family, id)
    }
    fn visit_family(&self, family: &str, f: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        self.journal.visit_family(family, f)
    }
    fn family_len(&self, family: &str) -> Result<Option<u64>> {
        self.journal.family_len(family)
    }
    fn visit_batches(
        &self,
        family: Option<&str>,
        f: &mut dyn FnMut(Vec<Record>) -> Result<()>,
    ) -> Result<()> {
        self.journal.visit_batches(family, f)
    }
    fn phase_progress(
        &self,
        _: &str,
        phase: &str,
        _: &str,
        done: u64,
        total: Option<u64>,
        workers: usize,
    ) {
        let total = total.expect("production journal must supply every phase total");
        assert!(done <= total);
        assert!((1..=128).contains(&workers));
        let mut phases = self.phases.lock().unwrap();
        if let Some((previous, old_total)) = phases.get(phase) {
            assert!(done >= *previous);
            assert_eq!(*old_total, total);
        }
        phases.insert(phase.into(), (done, total));
    }
}
#[test]
fn all_record_preparation_phases_reach_their_exact_totals() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let mut header = paged_fixture::fixture(&store, 1500, 1);
    header.revision.clear();
    let journal = Journal::new(temp.path().into()).unwrap();
    cq_core::semantic::records::drain(&mut header, &journal).unwrap();
    let progress = CountedProgress {
        journal: &journal,
        phases: Mutex::new(Default::default()),
    };
    let _prepared = store.prepare_records(header, &progress).unwrap();
    let phases = progress.phases.lock().unwrap();
    for name in [
        "prepare.validate.ids.entity",
        "prepare.validate.references",
        "prepare.handles.entities",
        "prepare.identity.entities",
        "prepare.sources",
        "prepare.normalize.entities",
        "prepare.normalize.relations",
    ] {
        let (done, total) = phases[name];
        assert_eq!(done, total, "{name}");
        assert!(total > 0, "{name}");
    }
    for (phase, (done, total)) in phases.iter() {
        assert_eq!(done, total, "unfinished phase {phase}");
    }
}

#[test]
fn physical_batches_freeze_live_locators_while_callbacks_append_and_replace() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let mut header = paged_fixture::fixture(&store, 5000, 1);
    let journal = Journal::new(temp.path().into()).unwrap();
    cq_core::semantic::records::drain(&mut header, &journal).unwrap();
    let mut visited = std::collections::BTreeMap::new();
    let mut first = true;
    journal
        .visit_batches(Some("entity"), &mut |rows| {
            if first {
                first = false;
                let Some(Record::Entity(mut e)) = journal.get("entity", "f4900")? else {
                    panic!()
                };
                e.name = "updated-after-visit-start".into();
                journal.put(&header, Record::Entity(e.clone()), true)?;
                e.id = "new-after-visit-start".into();
                journal.put(&header, Record::Entity(e), false)?;
            }
            for row in rows {
                if let Record::Entity(e) = row {
                    visited.insert(e.id, e.name);
                }
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(visited.len(), 5000);
    assert_eq!(visited["f4900"], "f4900");
    assert!(!visited.contains_key("new-after-visit-start"));
    let Some(Record::Entity(updated)) = journal.get("entity", "f4900").unwrap() else {
        panic!()
    };
    assert_eq!(updated.name, "updated-after-visit-start");
    assert_eq!(journal.family_len("entity").unwrap(), Some(5001));
}
