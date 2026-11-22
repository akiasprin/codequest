use cq_core::{
    analyst::native::{self, NativeUnitInput},
    model::*,
    ports::{ContentRepository, facts::*},
    semantic::{
        production::{self, Record, ScopeDescriptor, Sink, Validator},
        scope::*,
    },
};
use cq_store::{
    LocalStore,
    facts::{objects::Objects, writer},
    revisions::StoreHost,
};
use serde_json::json;
use std::collections::BTreeMap;
fn fixture(store: &LocalStore, run: &str) -> SemanticSnapshot {
    let mut s = SemanticSnapshot::empty("production".into(), NativeValue::from("/repo"));
    s.builds.push(BuildObservation {
        id: run.into(),
        language: Language::Cpp,
        command: NativeValue::from("clang++"),
        arguments: vec![],
        cwd: NativeValue::from("/repo"),
        outcome: BuildOutcome::Succeeded,
        exit: ExitObservation {
            code: Some(0),
            signal: None,
            spawn_error: None,
        },
        events: vec![],
        rejected_fields: vec![],
        captured_report: None,
        gaps: vec![],
    });
    let prov = s.intern_provenance(Provenance {
        provider: "native-contract".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: Some(run.into()),
        note: "observed context".into(),
    });
    for name in ["a", "b"] {
        let u = native::unit(NativeUnitInput {
            language: Language::Cpp,
            run: run.into(),
            name: name.into(),
            root: NativeValue::from(format!("/repo/{name}.cpp").as_str()),
            arguments: vec![NativeValue::from("clang++")],
            cwd: NativeValue::from("/repo"),
            files: vec![],
            invocation: Observed::Value(format!("inv-{run}-{name}")),
        });
        let path = NativeValue::from(format!("/repo/{name}.cpp").as_str());
        let blob = store.content.put(b"int run(){return 1;}\n", 1024).unwrap();
        let f = FileRevision {
            id: digest(&serde_json::to_vec(&(&path, &blob)).unwrap()),
            logical_path: path,
            blob,
            origin: "fixture".into(),
        };
        s.entities.push(Entity {
            id: format!("{run}-{name}"),
            stable_handle: None,
            locations: Default::default(),
            unit: u.id.clone(),
            provider_id: format!("usr:{name}"),
            kind: EntityKind::Function,
            name: name.into(),
            qualified_name: name.into(),
            source: Some(SourceAnchor {
                file_revision: f.id.clone(),
                start: 0,
                end: 20,
            }),
            extent: None,
            signature: "int()".into(),
            parent: None,
            external: false,
            exported: true,
            attributes: BTreeMap::new(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: prov.clone(),
        });
        s.units.push(u);
        s.sources.files.push(f);
    }
    s.sources = SourceSnapshot::seal(
        s.sources.files.clone(),
        InputBinding::PrePostChecked,
        vec![],
    );
    s.analysis_request = Some(json!({"allow_partial":false}));
    cq_core::semantic::identity::assign(&mut s).unwrap();
    production::canonicalize(&mut s).unwrap();
    s.seal().unwrap();
    s
}
#[derive(Clone)]
enum Frame {
    Begin(ScopeDescriptor),
    Batch(ScopeKey, u64, Vec<Record>),
    End(ScopeDescriptor, u64),
}
#[derive(Default)]
struct Frames(Vec<Frame>);
impl Sink for Frames {
    fn begin(&mut self, s: &ScopeDescriptor) -> cq_core::Result<()> {
        self.0.push(Frame::Begin(s.clone()));
        Ok(())
    }
    fn batch(&mut self, s: &ScopeKey, n: u64, r: &[Record]) -> cq_core::Result<()> {
        self.0.push(Frame::Batch(s.clone(), n, r.to_vec()));
        Ok(())
    }
    fn end(&mut self, s: &ScopeDescriptor, n: u64) -> cq_core::Result<()> {
        self.0.push(Frame::End(s.clone(), n));
        Ok(())
    }
    fn finish(&mut self) -> cq_core::Result<()> {
        Ok(())
    }
}
fn replay(s: &SemanticSnapshot, frames: &[Frame]) -> cq_core::Result<()> {
    let mut v = Validator::new(s.production.as_ref().unwrap())?;
    for f in frames {
        match f {
            Frame::Begin(s) => v.begin(s)?,
            Frame::Batch(s, n, r) => v.batch(s, *n, r)?,
            Frame::End(s, n) => v.end(s, *n)?,
        }
    }
    v.finish()
}
#[test]
fn observation_changes_do_not_rewrite_semantic_payloads_and_export_is_lossless() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let host = StoreHost::open(t.path(), "production").unwrap();
    let a = fixture(&store, "run-a");
    let mut b = fixture(&store, "run-b");
    b.previous = Some(a.revision.clone());
    b.seal().unwrap();
    assert_eq!(
        serde_json::to_value(&a.entities).unwrap(),
        serde_json::to_value(&b.entities).unwrap()
    );
    assert_ne!(
        serde_json::to_value(&a.production.as_ref().unwrap().unit_observations).unwrap(),
        serde_json::to_value(&b.production.as_ref().unwrap().unit_observations).unwrap()
    );
    let ra = writer::snapshot(&host.objects, &store.content, &a, None).unwrap();
    assert_eq!(host.publish(&ra, None).outcome, CommitOutcome::Committed);
    let rb = writer::snapshot(&host.objects, &store.content, &b, Some(&ra)).unwrap();
    assert_eq!(
        host.publish(&rb, Some(&ra.revision)).outcome,
        CommitOutcome::Committed
    );
    for e in &a.entities {
        let old = writer::reference(
            &host.objects,
            &ra.roots,
            "entity",
            &e.id,
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap();
        let new = writer::reference(
            &host.objects,
            &rb.roots,
            "entity",
            &e.id,
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::to_value(old).unwrap(),
            serde_json::to_value(new).unwrap()
        );
    }
    let exported = writer::export(&host.objects, &rb).unwrap();
    assert_eq!(
        serde_json::to_value(&b).unwrap(),
        serde_json::to_value(&exported).unwrap()
    );
    cq_core::semantic::validate::validate(&exported, &store.content).unwrap();
    let current = host.open_view(None).unwrap();
    assert!(current.count("all:observation_unit").unwrap() > 0);
}
#[test]
fn incomplete_duplicate_foreign_and_corrupt_streams_fail_closed() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let s = fixture(&store, "a");
    let mut frames = Frames::default();
    production::emit(&s, &mut frames).unwrap();
    replay(&s, &frames.0).unwrap();
    let batch = frames
        .0
        .iter()
        .position(|f| matches!(f, Frame::Batch(..)))
        .unwrap();
    let mut missing = frames.0.clone();
    missing.remove(batch);
    assert!(replay(&s, &missing).is_err());
    let mut duplicate = frames.0.clone();
    duplicate.insert(batch, duplicate[batch].clone());
    assert!(replay(&s, &duplicate).is_err());
    let mut corrupt = frames.0.clone();
    if let Frame::Batch(_, _, rows) = &mut corrupt[batch] {
        rows[0].value["name"] = json!("forged");
    }
    assert!(replay(&s, &corrupt).is_err());
    let mut reordered = frames.0.clone();
    if let Frame::Batch(_, sequence, _) = &mut reordered[batch] {
        *sequence = 8;
    }
    assert!(replay(&s, &reordered).is_err());
    let mut foreign = frames.0.clone();
    if let Frame::Batch(key, _, _) = &mut foreign[batch] {
        key.unit = "foreign".into();
    }
    assert!(replay(&s, &foreign).is_err());
    let mut truncated = frames.0.clone();
    truncated.pop();
    assert!(replay(&s, &truncated).is_err());
    let mut invalid = s.production.clone().unwrap();
    invalid.selection_complete = false;
    assert!(Validator::new(&invalid).is_err());
    let mut p = s.production.clone().unwrap();
    p.scopes.truncate(1);
    let scope = p.scopes[0].clone();
    let mut v = Validator::new(&p).unwrap();
    v.begin(&scope).unwrap();
    let mut forged = scope.clone();
    forged.rows += 1;
    assert!(v.end(&forged, 0).is_err());
    assert!(
        v.finish().is_err(),
        "an ignored frame failure must never make the transaction valid"
    );
}
#[test]
fn failed_scope_drops_stale_payloads_empty_completion_deletes_and_old_view_survives() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "production").unwrap();
    let a = fixture(&store, "a");
    let ra = writer::snapshot(&h.objects, &store.content, &a, None).unwrap();
    assert_eq!(h.publish(&ra, None).outcome, CommitOutcome::Committed);
    let old = h.open_view(None).unwrap();
    let mut b = fixture(&store, "b");
    let failed = b.units[0].id.clone();
    b.production.as_mut().unwrap().partial = true;
    b.production
        .as_mut()
        .unwrap()
        .failed_units
        .insert(failed.clone(), "failed".into());
    b.entities.retain(|e| e.unit != failed);
    b.analysis_request = Some(json!({"allow_partial":true}));
    production::refresh_descriptors(&mut b).unwrap();
    b.previous = Some(a.revision.clone());
    b.seal().unwrap();
    let rb = writer::snapshot(&h.objects, &store.content, &b, Some(&ra)).unwrap();
    assert_eq!(
        h.publish(&rb, Some(&ra.revision)).outcome,
        CommitOutcome::Committed
    );
    assert_eq!(old.count("all:entity").unwrap(), 2);
    assert_eq!(h.open_view(None).unwrap().count("all:entity").unwrap(), 1);
    assert!(rb.changes.facts_deleted > 0);
    let mut empty = b.clone();
    empty.entities.clear();
    production::refresh_descriptors(&mut empty).unwrap();
    empty.previous = Some(b.revision.clone());
    empty.seal().unwrap();
    let rc = writer::snapshot(&h.objects, &store.content, &empty, Some(&rb)).unwrap();
    assert_eq!(
        h.publish(&rc, Some(&rb.revision)).outcome,
        CommitOutcome::Committed
    );
    assert_eq!(h.open_view(None).unwrap().count("all:entity").unwrap(), 0);
}
#[test]
fn configuration_change_has_a_distinct_scope_and_old_context_is_not_reused() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let a = fixture(&store, "a");
    let mut b = a.clone();
    // Start from the retained native observation to simulate a changed compile selection.
    b.units = b
        .production
        .as_ref()
        .unwrap()
        .unit_observations
        .iter()
        .map(|o| o.observed.clone())
        .collect();
    let old = b.units[0].id.clone();
    if let ConfigurationObservation::Native(c) = &mut b.units[0].configuration {
        c.arguments.push(NativeValue::from("-DNEW=1"));
    }
    for e in &mut b.entities {
        if e.unit == a.units[0].id {
            e.unit = old.clone();
            e.stable_handle = None;
        }
    }
    production::canonicalize(&mut b).unwrap();
    assert_ne!(a.units[0].id, b.units[0].id);
    assert!(
        b.production
            .as_ref()
            .unwrap()
            .scopes
            .iter()
            .all(|s| s.key.unit != a.units[0].id)
    );
}
#[test]
fn scope_receipts_cannot_be_tampered_after_candidate_preparation() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let o = Objects::new(t.path().join("objects")).unwrap();
    let mut s = fixture(&store, "a");
    s.production.as_mut().unwrap().scopes[0].digest = "0".repeat(64);
    assert!(writer::snapshot(&o, &store.content, &s, None).is_err());
    assert!(cq_core::semantic::validate::validate(&s, &store.content).is_err());
}

#[test]
fn runtime_evidence_refresh_preserves_native_bindings_and_static_calls() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let mut s = fixture(&store, "a");
    let bindings = serde_json::to_value(&s.production.as_ref().unwrap().unit_observations).unwrap();
    let from = s.entities[0].id.clone();
    let to = s.entities[1].id.clone();
    s.add_relation(
        &from,
        &to,
        RelationKind::ObservedCall,
        None,
        None,
        Provenance {
            provider: "runtime".into(),
            version: "1".into(),
            authority: Authority::RuntimeObservation,
            build_run: None,
            note: "caller supplied; not static resolution".into(),
        },
    );
    production::refresh_descriptors(&mut s).unwrap();
    s.seal().unwrap();
    cq_core::semantic::validate::validate(&s, &store.content).unwrap();
    assert_eq!(
        serde_json::to_value(&s.production.as_ref().unwrap().unit_observations).unwrap(),
        bindings
    );
    assert_eq!(
        s.relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .count(),
        0
    );
    assert_eq!(
        s.relations
            .iter()
            .filter(|r| r.kind == RelationKind::ObservedCall)
            .count(),
        1
    );
}

#[test]
fn generation_scoped_query_aliases_roundtrip_without_modifying_canonical_payloads() {
    use cq_core::query::{QueryRequest, paged::Engine};
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "production").unwrap();
    let mut s = fixture(&store, "a");
    s.entities[0].id = digest(b"local-v1");
    s.entities[0].stable_handle = None;
    let local = s.entities[0].clone();
    production::refresh_descriptors(&mut s).unwrap();
    s.seal().unwrap();
    let r = writer::snapshot(&h.objects, &store.content, &s, None).unwrap();
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let old = h.open_view(None).unwrap();
    let handle = cq_core::semantic::identity::query_handle(&local).unwrap();
    assert!(handle.starts_with("cqg1:"));
    let q = QueryRequest {
        operation: "definition".into(),
        subject: Some(handle.clone()),
        ..Default::default()
    };
    let result = Engine {
        reader: &old,
        model: None,
        control: None,
    }
    .execute(&q, None)
    .unwrap();
    assert_eq!(result.page.items[0]["id"], local.id);
    assert_eq!(result.page.items[0]["stable_handle"], handle);
    assert!(
        old.get("entity", &local.id).unwrap().unwrap().value["stable_handle"].is_null(),
        "query aliases must not rewrite semantic payloads"
    );
    let mut next = s.clone();
    next.previous = Some(s.revision.clone());
    next.entities
        .iter_mut()
        .find(|e| e.id == local.id)
        .unwrap()
        .id = digest(b"local-v2");
    production::refresh_descriptors(&mut next).unwrap();
    next.seal().unwrap();
    let r2 = writer::snapshot(&h.objects, &store.content, &next, Some(&r)).unwrap();
    assert_eq!(
        h.publish(&r2, Some(&r.revision)).outcome,
        CommitOutcome::Committed
    );
    let current = h.open_view(None).unwrap();
    assert!(
        Engine {
            reader: &current,
            model: None,
            control: None
        }
        .execute(&q, None)
        .is_err()
    );
    assert!(
        Engine {
            reader: &old,
            model: None,
            control: None
        }
        .execute(&q, None)
        .is_ok()
    );
}
