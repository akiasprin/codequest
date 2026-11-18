use cq_core::semantic::records::{Record, Source, Store};
use cq_store::{LocalStore, raw::Journal};
#[allow(dead_code)]
#[path = "common/paged_fixture.rs"]
mod paged_fixture;

#[test]
fn batch_deduplicates_checks_conflicts_and_preserves_explicit_replacements() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let s = paged_fixture::fixture(&store, 3, 1);
    let j = Journal::new(t.path().into()).unwrap();
    let a = Record::Entity(s.entities[0].clone());
    j.put_batch(&s, vec![a.clone(), a.clone()], false).unwrap();
    assert_eq!(j.stats()["records"], 1);
    let bytes = j.stats()["journal_bytes"].clone();
    j.put_batch(&s, vec![a.clone()], false).unwrap();
    assert_eq!(j.stats()["journal_bytes"], bytes);
    let mut changed = s.entities[0].clone();
    changed.name = "changed".into();
    let b = Record::Entity(changed);
    assert_eq!(
        j.put_batch(&s, vec![a.clone(), b.clone()], false)
            .unwrap_err()
            .code(),
        "ConflictingAnalysisRecord"
    );
    assert_eq!(
        j.get(a.family(), a.id()).unwrap().unwrap().value().unwrap(),
        a.value().unwrap()
    );
    j.put_batch(&s, vec![b.clone(), a.clone(), b.clone()], true)
        .unwrap();
    assert_eq!(
        j.get(a.family(), a.id()).unwrap().unwrap().value().unwrap(),
        b.value().unwrap()
    );
    let fresh = Journal::new(t.path().into()).unwrap();
    assert_eq!(
        fresh.put_batch(&s, vec![a, b], false).unwrap_err().code(),
        "ConflictingAnalysisRecord"
    );
    assert_eq!(fresh.stats()["records"], 0);
}

#[test]
fn batch_chunks_have_correct_offsets_and_progress_reports_worker_count() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let s = paged_fixture::fixture(&store, 5, 1);
    let j = Journal::new(t.path().into()).unwrap();
    let records = s
        .entities
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut e = e.clone();
            e.signature = i.to_string().repeat(600_000);
            Record::Entity(e)
        })
        .collect::<Vec<_>>();
    j.put_batch(&s, records.clone(), false).unwrap();
    for r in &records {
        assert_eq!(
            j.get(r.family(), r.id()).unwrap().unwrap().value().unwrap(),
            r.value().unwrap()
        );
    }
    let mut count = 0;
    j.visit(&mut |_| {
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, records.len());
    j.progress_workers(&s.project, 0, 12, 4);
    j.progress_pipeline(
        &s.project,
        12,
        12,
        &cq_core::progress::Pipeline {
            workers: 4,
            ..Default::default()
        },
    );
    let p: serde_json::Value = serde_json::from_slice(
        &std::fs::read(cq_core::progress::path(t.path(), &s.project)).unwrap(),
    )
    .unwrap();
    assert_eq!(p["workers"], 4);
    assert_eq!(p["completed"], 12);
    assert_eq!(p["total"], 12);
    assert_eq!(p["pipeline"]["active"], 0);
    assert_eq!(p["pipeline"]["ready"], 0);
}
