use cq_core::{
    model::*,
    ports::{ContentRepository, facts::*},
    query::{QueryRequest, paged::Engine},
};
use cq_store::{
    LocalStore,
    facts::{
        objects::Objects,
        source::Source,
        tree,
        writer::{self, Roots},
    },
    revisions::StoreHost,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
fn fixture(store: &LocalStore, n: usize, target: usize) -> SemanticSnapshot {
    let mut s = SemanticSnapshot::empty("paged-test".into(), NativeValue::from("/frozen/repo"));
    let p = Provenance {
        provider: "contract".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: "controlled fixture".into(),
    };
    let pid = s.intern_provenance(p.clone());
    for i in 0..n {
        s.entities.push(Entity {
            stable_handle: None,
            locations: Default::default(),
            id: format!("f{i}"),
            unit: "unit".into(),
            provider_id: format!("f{i}"),
            kind: EntityKind::Function,
            name: format!("f{i}"),
            qualified_name: format!("test::f{i}"),
            source: None,
            extent: None,
            signature: "fn()".into(),
            parent: None,
            external: false,
            exported: true,
            attributes: BTreeMap::new(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: pid.clone(),
        });
    }
    if n > 1 {
        s.add_relation(
            "f0",
            &format!("f{target}"),
            RelationKind::Calls,
            None,
            None,
            p,
        );
    }
    let blob = store
        .content
        .put(b"frozen source\nsecond line\n", 1024)
        .unwrap();
    let mut file = FileRevision {
        id: String::new(),
        logical_path: NativeValue::from("/frozen/repo/main.rs"),
        blob,
        origin: "fixture".into(),
    };
    file.id = digest(&serde_json::to_vec(&(&file.logical_path, &file.blob)).unwrap());
    s.sources = SourceSnapshot::seal(vec![file], InputBinding::ImmutableScopeVerified, vec![]);
    s.seal().unwrap();
    s
}
fn write(
    o: &Objects,
    store: &LocalStore,
    s: &SemanticSnapshot,
    base: Option<&writer::Revision>,
) -> writer::Revision {
    writer::snapshot(o, &store.content, s, base).unwrap()
}
#[test]
fn resolve_restores_first_publication_registration_without_reindexing() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let s = fixture(&store, 3, 1);
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let r = write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    drop(h); // The frontend never wrote project.json or the format marker.
    assert!(store.project_info("paged-test").is_err());
    assert!(!store.paged_enabled("paged-test"));
    for _ in 0..2 {
        assert_eq!(
            store
                .resolve_publication("paged-test", &r.revision)
                .unwrap()
                .outcome,
            CommitOutcome::Committed
        );
        assert_eq!(
            store.project_info("paged-test").unwrap().head.as_deref(),
            Some(r.revision.as_str())
        );
        assert!(store.paged_enabled("paged-test"));
        let view = store.open_facts("paged-test", None).unwrap();
        assert_eq!(view.revision(), r.revision);
        assert!(view.get("entity", "f1").unwrap().is_some());
    }
}
#[test]
fn resolve_uncommitted_candidate_does_not_register_a_project() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let s = fixture(&store, 3, 1);
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let r = write(&h.objects, &store, &s, None);
    drop(h);
    assert_eq!(
        store
            .resolve_publication("paged-test", &r.revision)
            .unwrap()
            .outcome,
        CommitOutcome::NotCommitted
    );
    assert!(!store.paged_enabled("paged-test"));
    assert!(store.project_info("paged-test").is_err());
}
#[test]
fn resolve_older_commit_registers_current_head_instead_of_rolling_back() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture(&store, 3, 1);
    let r1 = write(&h.objects, &store, &a, None);
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    let b = fixture(&store, 3, 2);
    let r2 = write(&h.objects, &store, &b, Some(&r1));
    assert_eq!(
        h.publish(&r2, Some(&r1.revision)).outcome,
        CommitOutcome::Committed
    );
    drop(h);
    assert_eq!(
        store
            .resolve_publication("paged-test", &r1.revision)
            .unwrap()
            .outcome,
        CommitOutcome::Committed
    );
    assert_eq!(
        store.project_info("paged-test").unwrap().head.as_deref(),
        Some(r2.revision.as_str())
    );
    let view = store.open_facts("paged-test", None).unwrap();
    assert_eq!(view.revision(), r2.revision);
}
#[test]
fn r1_persistent_manifest_has_stable_pages_and_bounded_keyset_scan() {
    let t = tempfile::tempdir().unwrap();
    let o = Objects::new(t.path().join("objects")).unwrap();
    let mut root = None;
    let mut stats = ReadStats::default();
    for i in 0..1100 {
        root = tree::set(&o, &root, &format!("key-{i}"), Some(json!(i)), &mut stats).unwrap();
    }
    let old = root.clone();
    let mut ids = BTreeSet::new();
    let mut after = None;
    loop {
        let page = tree::scan(&o, &root, after.as_deref(), 7, &mut stats).unwrap();
        for (_, k, _) in page.rows {
            assert!(ids.insert(k));
        }
        if page.complete {
            break;
        }
        after = page.after;
    }
    assert_eq!(ids.len(), 1100);
    root = tree::set(&o, &root, "key-42", Some(json!(999)), &mut stats).unwrap();
    assert_eq!(
        tree::get(&o, &old, "key-42", &mut stats).unwrap(),
        Some(json!(42))
    );
    assert_eq!(
        tree::get(&o, &root, "key-42", &mut stats).unwrap(),
        Some(json!(999))
    );
    assert_eq!(
        tree::set(&o, &root, "key-42", Some(json!(999)), &mut stats).unwrap(),
        root
    );
    root = tree::set(&o, &root, "key-42", None, &mut stats).unwrap();
    assert!(
        tree::get(&o, &root, "key-42", &mut stats)
            .unwrap()
            .is_none()
    );
    let mut cold = ReadStats::default();
    o.clear_cache();
    assert!(
        tree::get(&o, &root, "key-999", &mut cold)
            .unwrap()
            .is_some()
    );
    assert!(
        cold.bytes_decoded < 128 * 1024,
        "point lookup must not load all manifest entries"
    );
}
#[test]
fn r1_frozen_source_reads_cross_block_ranges_and_rejects_damage() {
    let t = tempfile::tempdir().unwrap();
    let o = Objects::new(t.path().join("objects")).unwrap();
    let bytes = (0..210_000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let src = Source::freeze(&o, &BlobId::of(&bytes), bytes.as_slice()).unwrap();
    o.clear_cache();
    let mut stats = ReadStats::default();
    assert_eq!(
        src.range(&o, 65530, 65550, &mut stats).unwrap(),
        bytes[65530..65550]
    );
    assert!(stats.bytes_decoded < 150_000);
    assert!(src.range(&o, 12, 11, &mut stats).is_err());
    let value = tree::get(&o, &src.blocks, "1", &mut stats)
        .unwrap()
        .unwrap();
    let r: ObjectRef = serde_json::from_value(value).unwrap();
    std::fs::write(o.path(&r).unwrap(), b"damaged").unwrap();
    o.clear_cache();
    assert!(src.range(&o, 65536, 65540, &mut stats).is_err());
}
#[test]
fn r1_complete_inventory_reuses_unchanged_pages_and_removes_old_facts() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let o = Objects::new(t.path().join("format-2/objects")).unwrap();
    let a = fixture(&store, 50, 1);
    let r1 = write(&o, &store, &a, None);
    let r2 = write(&o, &store, &a, Some(&r1));
    assert_eq!(r2.changes.fact_pages_prepared, 0);
    assert_eq!(r2.changes.facts_changed, 0);
    assert_eq!(r1.roots.facts, r2.roots.facts);
    let w = writer::Writer::new(&o, r1.roots.clone());
    let (empty, stats) = w.finish_inventory().unwrap();
    assert!(empty.facts.is_none());
    assert!(stats.facts_deleted > 50);
    assert!(empty.groups.is_none());
    let mut w = writer::Writer::new(&o, Roots::default());
    w.append("entity", "same", &json!({"id":"same"})).unwrap();
    assert!(w.append("entity", "same", &json!({"id":"same"})).is_err());
    let exported = writer::export(&o, &r1).unwrap();
    assert_eq!(
        serde_json::to_value(&a.entities).unwrap(),
        serde_json::to_value(&exported.entities).unwrap()
    );
    assert_eq!(a.sources, exported.sources);
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&exported).unwrap()
    );
    cq_core::semantic::validate::validate(&exported, &store.content).unwrap();
}
#[test]
fn r2_native_multiversion_aba_epoch_and_unknown_candidate_are_isolated() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture(&store, 5, 1);
    let r1 = write(&h.objects, &store, &a, None);
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    let old = h.open_view(None).unwrap();
    let b = fixture(&store, 5, 2);
    let r2 = write(&h.objects, &store, &b, Some(&r1));
    assert!(h.open_view(Some(&r2.revision)).is_err());
    assert_eq!(
        h.publish(&r2, Some(&r1.revision)).outcome,
        CommitOutcome::Committed
    );
    let new = h.open_view(None).unwrap();
    let q = QueryRequest {
        operation: "callees".into(),
        subject: Some("f0".into()),
        ..Default::default()
    };
    let run = |v: &dyn FactReader| {
        Engine {
            reader: v,
            model: None,
            control: None,
        }
        .execute(&q, None)
        .unwrap()
        .page
    };
    assert_eq!(run(&old).items[0]["entity"]["id"], "f1");
    assert_eq!(run(&new).items[0]["entity"]["id"], "f2");
    assert_eq!(
        h.publish(&r2, Some("wrong")).outcome,
        CommitOutcome::Committed,
        "committed retry must be idempotent"
    );
    let mut again = a.clone();
    again.previous = Some(b.revision.clone());
    again.seal().unwrap();
    let r3 = write(&h.objects, &store, &again, Some(&r2));
    assert_eq!(
        h.publish(&r3, Some(&r2.revision)).outcome,
        CommitOutcome::Committed
    );
    let before = h.head().unwrap();
    h.rebuild().unwrap();
    assert_eq!(h.head().unwrap(), before);
    assert_eq!(run(&old).items[0]["entity"]["id"], "f1");
    assert_eq!(run(&new).items[0]["entity"]["id"], "f2");
    let newest = h.open_view(None).unwrap();
    assert_eq!(run(&newest).items[0]["entity"]["id"], "f1");
    assert_eq!(newest.stats().full_snapshot_decodes, 0);
    assert!(newest.source_range(&a.sources.files[0].id, 0, 6).unwrap() == b"frozen");
    drop(old);
    drop(new);
    drop(newest);
    drop(h);
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    assert_eq!(h.head().unwrap(), before);
}
#[test]
fn r2_native_bulk_copy_preserves_quotes_commas_and_multiline_document_names() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture(&store, 4, 1);
    s.entities[1].name =
        "Package proto stands in for gin's testdata/protoexample.\nNext, \"quoted\" line 中文"
            .into();
    s.seal().unwrap();
    let r = write(&h.objects, &store, &s, None);
    let result = h.publish(&r, None);
    assert_eq!(result.outcome, CommitOutcome::Committed, "{result:?}");
    let v = h.open_view(None).unwrap();
    assert_eq!(
        v.get("entity", &s.entities[1].id).unwrap().unwrap().value["name"],
        s.entities[1].name
    );
}
#[test]
fn r3_native_pagination_walk_and_ranking_match_explicit_expectations() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture(&store, 70, 1);
    let p = s.provenance_records.values().next().unwrap().clone();
    s.relations.clear();
    for i in 1..70 {
        s.add_relation(
            &format!("f{}", i - 1),
            &format!("f{i}"),
            RelationKind::Calls,
            None,
            None,
            p.clone(),
        );
    }
    s.seal().unwrap();
    let r = write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let v = h.open_view(None).unwrap();
    let engine = Engine {
        reader: &v,
        model: None,
        control: None,
    };
    let q = QueryRequest {
        operation: "path".into(),
        subject: Some("f0".into()),
        target: Some("f69".into()),
        depth: 100,
        max_work: 3,
        limit: 1,
        ..Default::default()
    };
    let mut next = None;
    let mut paths = vec![];
    for _ in 0..300 {
        let result = engine.execute(&q, next).unwrap();
        paths.extend(result.page.items);
        next = result.continuation;
        if next.is_none() {
            break;
        }
    }
    assert!(next.is_none());
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0]["relation_path"].as_array().unwrap().len(), 69);
    let q = QueryRequest {
        operation: "search".into(),
        text: "f12".into(),
        limit: 1,
        max_work: 7,
        ..Default::default()
    };
    let mut next = None;
    let mut rows = vec![];
    for _ in 0..100 {
        let result = engine.execute(&q, next).unwrap();
        rows.extend(result.page.items);
        next = result.continuation;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(rows[0]["id"], "f12");
    let q = QueryRequest {
        operation: "trace".into(),
        subject: Some("f0".into()),
        depth: 100,
        max_work: 4,
        limit: 3,
        ..Default::default()
    };
    let mut next = None;
    let mut ids = BTreeSet::new();
    for _ in 0..300 {
        let result = engine.execute(&q, next).unwrap();
        for row in result.page.items {
            assert!(ids.insert(row["entity"]["id"].as_str().unwrap().to_owned()));
        }
        next = result.continuation;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 70);
    assert!(next.is_none());
}
#[test]
fn r3_high_fanout_pages_survive_new_head_without_duplicates_or_snapshot_decode() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture(&store, 1501, 1);
    let p = s.provenance_records.values().next().unwrap().clone();
    s.relations.clear();
    for i in 1..1501 {
        s.add_relation(
            "f0",
            &format!("f{i}"),
            RelationKind::Calls,
            None,
            None,
            p.clone(),
        );
    }
    s.seal().unwrap();
    let r = write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let view = h.open_view(None).unwrap();
    let q = QueryRequest {
        operation: "callees".into(),
        subject: Some("f0".into()),
        limit: 7,
        max_work: 11,
        ..Default::default()
    };
    let engine = Engine {
        reader: &view,
        model: None,
        control: None,
    };
    let mut next = None;
    let mut seen = BTreeSet::new();
    for page in 0..400 {
        let result = engine.execute(&q, next).unwrap();
        for row in result.page.items {
            assert!(seen.insert(row["entity"]["id"].as_str().unwrap().to_owned()));
        }
        next = result.continuation;
        if page == 0 {
            let mut changed = s.clone();
            changed.previous = Some(s.revision.clone());
            changed.relations.retain(|r| r.to == "f1");
            changed.seal().unwrap();
            let r2 = write(&h.objects, &store, &changed, Some(&r));
            assert_eq!(
                h.publish(&r2, Some(&r.revision)).outcome,
                CommitOutcome::Committed
            );
        }
        if next.is_none() {
            break;
        }
    }
    assert!(next.is_none());
    assert_eq!(seen.len(), 1500);
    assert_eq!(view.stats().full_snapshot_decodes, 0);
    assert!(view.stats().database_queries > 0);
    assert!(
        view.stats().database_queries < 100,
        "native page cache must avoid one database round trip per neighbor"
    );
}
#[test]
fn r2_retention_preserves_leases_shared_objects_and_rebuild_after_gc() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture(&store, 5, 1);
    let r1 = write(&h.objects, &store, &a, None);
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    let old = h.open_view(None).unwrap();
    let h2 = StoreHost::open(t.path(), "other").unwrap();
    let mut shared = a.clone();
    shared.project = "other".into();
    shared.seal().unwrap();
    let shared_r = write(&h.objects, &store, &shared, None);
    assert_eq!(
        h2.publish(&shared_r, None).outcome,
        CommitOutcome::Committed
    );
    let b = fixture(&store, 5, 2);
    let r2 = write(&h.objects, &store, &b, Some(&r1));
    assert_eq!(
        h.publish(&r2, Some(&r1.revision)).outcome,
        CommitOutcome::Committed
    );
    assert!(h.preview_retention(1).unwrap().revisions.is_empty());
    drop(old);
    let stale = h.preview_retention(1).unwrap();
    assert_eq!(stale.revisions, vec![r1.revision.clone()]);
    let lease = h.open_view(Some(&r1.revision)).unwrap();
    assert!(h.apply_retention(&stale).is_err());
    drop(lease);
    let plan = h.preview_retention(1).unwrap();
    h.apply_retention(&plan).unwrap();
    assert!(h.open_view(Some(&r1.revision)).is_err());
    assert_eq!(
        h.resolve(&r1.revision).unwrap().outcome,
        CommitOutcome::Committed
    );
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    assert_eq!(h.head().unwrap().as_deref(), Some(r2.revision.as_str()));
    let mut forged = h.preview_retention(1).unwrap();
    forged.revisions.push("unpublished".into());
    assert!(h.apply_retention(&forged).is_err());
    let shared_view = h2.open_view(None).unwrap();
    assert!(shared_view.get("entity", "f1").unwrap().is_some());
    h.rebuild().unwrap();
    let current = h.open_view(None).unwrap();
    assert!(current.get("entity", "f2").unwrap().is_some());
    drop(current);
    let epoch = h
        .root
        .join("indexes")
        .join(
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(h.root.join("route.json")).unwrap(),
            )
            .unwrap()["epoch"]
                .as_str()
                .unwrap(),
        )
        .join("graph.lbdb");
    drop(h);
    std::fs::remove_file(epoch).unwrap();
    let repaired = StoreHost::open(t.path(), "paged-test").unwrap();
    assert_eq!(
        repaired.head().unwrap().as_deref(),
        Some(r2.revision.as_str())
    );
    assert!(
        repaired
            .open_view(None)
            .unwrap()
            .get("entity", "f2")
            .unwrap()
            .is_some()
    );
}
#[test]
fn r2_forged_delta_and_dangling_endpoint_cannot_publish() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture(&store, 5, 1);
    let r1 = write(&h.objects, &store, &a, None);
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    let mut b = fixture(&store, 5, 2);
    let mut missing_directory = write(&h.objects, &store, &b, Some(&r1));
    missing_directory.roots.groups = None;
    missing_directory.seal().unwrap();
    assert_eq!(
        h.publish(&missing_directory, Some(&r1.revision)).outcome,
        CommitOutcome::NotCommitted
    );
    let mut r2 = write(&h.objects, &store, &b, Some(&r1));
    r2.changes.upserts = None;
    r2.seal().unwrap();
    assert_eq!(
        h.publish(&r2, Some(&r1.revision)).outcome,
        CommitOutcome::NotCommitted
    );
    assert_eq!(h.head().unwrap().as_deref(), Some(r1.revision.as_str()));
    b.relations[0].to = "absent".into();
    b.seal().unwrap();
    let r2 = write(&h.objects, &store, &b, Some(&r1));
    assert_eq!(
        h.publish(&r2, Some(&r1.revision)).outcome,
        CommitOutcome::NotCommitted
    );
}
#[test]
fn r2_expired_session_cannot_admit_new_reads_but_active_read_stays_protected() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture(&store, 4, 1);
    let r1 = write(&h.objects, &store, &a, None);
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    let session = h.open_session(None).unwrap();
    let read = session.request().unwrap();
    let b = fixture(&store, 4, 2);
    let r2 = write(&h.objects, &store, &b, Some(&r1));
    assert_eq!(
        h.publish(&r2, Some(&r1.revision)).outcome,
        CommitOutcome::Committed
    );
    h.release_lease(session.lease()).unwrap();
    assert!(session.request().is_err());
    assert!(session.get("entity", "f0").is_err());
    assert!(read.get("entity", "f0").unwrap().is_some());
    assert!(h.preview_retention(1).unwrap().revisions.is_empty());
    drop(read);
    let plan = h.preview_retention(1).unwrap();
    assert_eq!(plan.revisions, vec![r1.revision]);
}
#[test]
fn r3_source_search_and_at_are_paged_frozen_reads() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture(&store, 3, 1);
    let file = s.sources.files[0].id.clone();
    s.entities[0].source = Some(SourceAnchor {
        file_revision: file,
        start: 0,
        end: 6,
    });
    s.seal().unwrap();
    let r = write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let v = h.open_view(None).unwrap();
    let engine = Engine {
        reader: &v,
        model: None,
        control: None,
    };
    let q = QueryRequest {
        operation: "search_code".into(),
        text: "frozen".into(),
        file: Some("main.rs".into()),
        max_work: 1,
        limit: 1,
        ..Default::default()
    };
    let mut next = None;
    let mut rows = vec![];
    for _ in 0..20 {
        let result = engine.execute(&q, next).unwrap();
        rows.extend(result.page.items);
        next = result.continuation;
        if next.is_none() {
            break;
        }
    }
    assert!(next.is_none());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["line"], 1);
    assert!(rows[0]["text"].as_str().unwrap().contains("frozen"));
    let q = QueryRequest {
        operation: "at".into(),
        file: Some("main.rs".into()),
        line: Some(1),
        column: Some(2),
        ..Default::default()
    };
    let page = engine.execute(&q, None).unwrap().page;
    assert_eq!(page.items[0]["id"], "f0");
    let bad = QueryRequest {
        column: Some(999),
        ..q
    };
    assert!(engine.execute(&bad, None).is_err());
    assert_eq!(v.stats().full_snapshot_decodes, 0);
}
#[test]
#[ignore = "CLI acceptance fixture generator, invoked with CQ_PAGED_FIXTURE_OUTPUT"]
fn export_cli_acceptance_fixture() {
    let Some(path) = std::env::var_os("CQ_PAGED_FIXTURE_OUTPUT") else {
        return;
    };
    let out = std::path::PathBuf::from(path);
    std::fs::create_dir_all(&out).unwrap();
    let store = LocalStore::open(&out.join("store")).unwrap();
    let mut a = fixture(&store, 1601, 1);
    let file = a.sources.files[0].clone();
    let p = a.provenance_records.values().next().unwrap().clone();
    for e in &mut a.entities {
        e.source = Some(SourceAnchor {
            file_revision: file.id.clone(),
            start: 0,
            end: 6,
        });
    }
    a.contexts = vec![
        AnalysisContext {
            id: "ctx-a".into(),
            entry_unit: "unit".into(),
            units: vec!["unit".into()],
            status: Support::Supported,
            gaps: vec![],
        },
        AnalysisContext {
            id: "ctx-b".into(),
            entry_unit: "other".into(),
            units: vec!["other".into()],
            status: Support::Supported,
            gaps: vec![],
        },
    ];
    a.relations.clear();
    for i in 1..1601 {
        a.add_relation(
            "f0",
            &format!("f{i}"),
            RelationKind::Calls,
            None,
            None,
            p.clone(),
        );
    }
    a.seal().unwrap();
    cq_core::semantic::validate::validate(&a, &store.content).unwrap();
    let mut b = a.clone();
    b.previous = Some(a.revision.clone());
    b.created_ms += 1;
    b.relations.retain(|r| r.to == "f2");
    b.seal().unwrap();
    let mut c = b.clone();
    c.previous = Some(b.revision.clone());
    c.created_ms += 1;
    c.relations.clear();
    c.seal().unwrap();
    let mut d = c.clone();
    d.previous = Some(c.revision.clone());
    d.created_ms += 1;
    d.seal().unwrap();
    for (name, s) in [("R1", a), ("R2", b), ("R3", c), ("R4", d)] {
        std::fs::write(
            out.join(format!("{name}.json")),
            serde_json::to_vec(&s).unwrap(),
        )
        .unwrap();
    }
    let mut other: SemanticSnapshot =
        serde_json::from_reader(std::fs::File::open(out.join("R1.json")).unwrap()).unwrap();
    other.project = "paged-other".into();
    other.seal().unwrap();
    std::fs::write(out.join("Other.json"), serde_json::to_vec(&other).unwrap()).unwrap();
}

#[test]
fn r2_missing_head_blocks_publication_and_admission() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture(&store, 5, 1);
    let r1 = write(&h.objects, &store, &a, None);
    assert_eq!(h.publish(&r1, None).outcome, CommitOutcome::Committed);
    let held = h.open_view(None).unwrap();
    std::fs::remove_file(h.root.join("HEAD")).unwrap();
    let b = fixture(&store, 5, 2);
    let r2 = write(&h.objects, &store, &b, Some(&r1));
    assert_eq!(h.publish(&r2, None).outcome, CommitOutcome::NotCommitted);
    assert!(h.open_view(None).is_err());
    assert!(held.get("entity", "f0").unwrap().is_some());
    assert_eq!(
        h.resolve(&r1.revision).unwrap().outcome,
        CommitOutcome::Unknown
    );
}

#[test]
#[ignore = "creates a real legacy store for CLI migration acceptance"]
fn export_legacy_acceptance_fixture() {
    let path = std::env::var_os("CQ_LEGACY_FIXTURE_OUTPUT").expect("fixture output");
    let store = LocalStore::open(std::path::Path::new(&path)).unwrap();
    let a = fixture(&store, 5, 1);
    store.publish_semantics(&a, None).unwrap();
    let mut b = fixture(&store, 5, 2);
    b.previous = Some(a.revision.clone());
    b.seal().unwrap();
    store.publish_semantics(&b, Some(&a.revision)).unwrap();
    store
        .save_annotation(&Annotation {
            id: digest(b"migration-note"),
            project: b.project.clone(),
            revision: b.revision.clone(),
            parent: None,
            kind: AnnotationKind::Responsibility,
            text: "fixture responsibility".into(),
            author: "contract-test".into(),
            entities: vec!["f0".into()],
            evidence: vec![],
            depends_on: vec![],
            status: ReviewStatus::Proposed,
            created_ms: 1,
        })
        .unwrap();
    std::fs::write(
        std::path::Path::new(&path).join("legacy-ids.json"),
        serde_json::to_vec(&json!([a.revision, b.revision])).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "subprocess legacy writer used by the migration race acceptance"]
fn legacy_writer_during_migration() {
    let root = std::env::var_os("CQ_LEGACY_WRITER_STORE").unwrap();
    let path = std::env::var_os("CQ_LEGACY_WRITER_SNAPSHOT").unwrap();
    let snapshot: SemanticSnapshot =
        serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    let store = LocalStore::open(std::path::Path::new(&root)).unwrap();
    store
        .publish_semantics(&snapshot, snapshot.previous.as_deref())
        .unwrap();
    assert!(store.paged_enabled(&snapshot.project));
    assert_eq!(
        store.head(&snapshot.project).unwrap(),
        Some(snapshot.revision)
    );
}

#[test]
fn r5_oversized_value_reports_frozen_original_and_keeps_head() {
    use cq_core::semantic::records::{Record, Store};
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let initial = fixture(&store, 2, 1);
    let r = write(&h.objects, &store, &initial, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let j = cq_store::raw::Journal::new(t.path().into()).unwrap();
    let mut e = initial.entities[0].clone();
    e.signature = "x".repeat(8 * 1024 * 1024 + 1);
    let f = &initial.sources.files[0];
    e.source = Some(SourceAnchor {
        file_revision: f.id.clone(),
        start: 0,
        end: 6,
    });
    let error = j.put(&initial, Record::Entity(e), false).unwrap_err();
    assert_eq!(error.code(), "OversizedFact");
    let details: serde_json::Value = serde_json::from_str(&error.message()).unwrap();
    let report: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(details["report"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(report["key_bytes"].as_u64().unwrap() < 100);
    assert!(report["value_bytes"].as_u64().unwrap() > 8 * 1024 * 1024);
    assert!(
        report["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["category"] == "Value" && f["path"] == "/signature")
    );
    assert_eq!(
        std::fs::read(report["originals"][0]["frozen_path"].as_str().unwrap()).unwrap(),
        b"frozen source\nsecond line\n"
    );
    assert_eq!(h.head().unwrap(), Some(r.revision));
    assert_eq!(j.stats()["records"], 0);
}
#[test]
fn r5_oversized_key_is_reported_without_echoing_or_truncating_identity() {
    use cq_core::semantic::records::{Record, Store};
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let s = fixture(&store, 1, 0);
    let j = cq_store::raw::Journal::new(t.path().into()).unwrap();
    let mut e = s.entities[0].clone();
    e.id = "k".repeat(8 * 1024 * 1024 + 1);
    let error = j.put(&s, Record::Entity(e), false).unwrap_err();
    assert!(
        error.message().len() < 32 * 1024,
        "diagnostic must be bounded"
    );
    let d: serde_json::Value = serde_json::from_str(&error.message()).unwrap();
    assert!(d["details"]["key_bytes"].as_u64().unwrap() > 8 * 1024 * 1024);
    assert!(
        d["details"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["category"] == "Key")
    );
    assert!(d["details"]["id"].is_null());
    assert_eq!(j.stats()["records"], 0);
}
#[test]
fn r5_object_batch_barrier_reopens_verified_and_cache_is_shared() {
    let t = tempfile::tempdir().unwrap();
    let o = Objects::buffered(t.path().join("objects")).unwrap();
    let a = o.put(b"first").unwrap();
    let b = o.put(b"second").unwrap();
    assert_eq!(o.resource_stats()["sync_calls"], 0);
    o.sync_pending().unwrap();
    assert!(o.resource_stats()["sync_calls"].as_u64().unwrap() >= 4);
    let same = Objects::new(t.path().join("objects")).unwrap();
    let mut read = ReadStats::default();
    o.read(&a, &mut read).unwrap();
    let mut cached = ReadStats::default();
    same.read(&a, &mut cached).unwrap();
    assert_eq!(cached.page_reads, 0);
    same.clear_cache();
    let mut cold = ReadStats::default();
    o.read(&a, &mut cold).unwrap();
    assert_eq!(cold.page_reads, 1);
    assert_eq!(
        &**same.read(&b, &mut ReadStats::default()).unwrap(),
        b"second"
    );
}
#[test]
fn r5_logical_views_do_not_pin_native_databases_and_old_epoch_reopens() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let mut hosts = vec![];
    let mut views = vec![];
    for name in ["residency-a", "residency-b", "residency-c"] {
        let h = StoreHost::open(t.path(), name).unwrap();
        let mut s = fixture(&store, 3, 1);
        s.project = name.into();
        s.seal().unwrap();
        let r = write(&h.objects, &store, &s, None);
        assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
        views.push(h.open_view(None).unwrap());
        hosts.push(h);
    }
    for _ in 0..2 {
        for v in &views {
            assert_eq!(v.get("entity", "f0").unwrap().unwrap().value["name"], "f0");
            assert_eq!(
                cq_store::ladybug::residency::diagnostics()["ordinary_databases"],
                1
            );
        }
    }
    let old = views[0].epoch().to_owned();
    let new = hosts[0].rebuild().unwrap();
    assert_ne!(old, new);
    assert!(hosts[0].root.join("indexes").join(&old).exists());
    assert_eq!(
        views[0].get("entity", "f0").unwrap().unwrap().value["name"],
        "f0"
    );
    drop(views);
    assert!(hosts[0].collect_epochs().unwrap().contains(&old));
    assert_eq!(
        cq_store::ladybug::residency::diagnostics()["maintenance_databases"],
        0
    );
}
#[test]
fn r5_compaction_preserves_revision_and_raw_evidence() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let s = fixture(&store, 5, 1);
    let r = write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let old = h.open_view(None).unwrap();
    let report = h.compact().unwrap();
    assert_eq!(report["head_before"], report["head_after"]);
    assert_eq!(report["canonical_facts_deleted"], false);
    assert!(old.get("entity", "f0").unwrap().is_some());
    let exported = writer::export(&h.objects, &r).unwrap();
    assert_eq!(
        serde_json::to_value(exported).unwrap(),
        serde_json::to_value(s).unwrap()
    );
}

#[test]
fn incremental_audit_rejects_missing_group_updates_and_forged_delta_inventory() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let s = fixture(&store, 40, 1);
    let a = write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&a, None).outcome, CommitOutcome::Committed);
    let mut next = fixture(&store, 40, 2);
    next.previous = Some(a.revision.clone());
    next.seal().unwrap();
    let b = write(&h.objects, &store, &next, Some(&a));
    let mut missing = b.clone();
    missing.roots.groups = a.roots.groups.clone();
    missing.seal().unwrap();
    let result = h.publish(&missing, Some(&a.revision));
    assert_eq!(result.outcome, CommitOutcome::NotCommitted);
    assert!(result.detail.unwrap().contains("IndexIncomplete"));
    let mut forged = b.clone();
    forged.changes.facts_changed += 1;
    forged.seal().unwrap();
    assert_eq!(
        h.publish(&forged, Some(&a.revision)).outcome,
        CommitOutcome::NotCommitted
    );
    assert_eq!(
        h.publish(&b, Some(&a.revision)).outcome,
        CommitOutcome::Committed
    );
    let metrics: serde_json::Value = serde_json::from_reader(
        std::fs::File::open(
            std::fs::read_dir(h.root.join("publication-traces"))
                .unwrap()
                .filter_map(|v| v.ok())
                .map(|v| v.path())
                .find(|path| {
                    serde_json::from_reader::<_, serde_json::Value>(
                        std::fs::File::open(path).unwrap(),
                    )
                    .unwrap()
                    .to_string()
                    .contains("incremental_merkle")
                })
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(metrics.to_string().contains("incremental_merkle"));
}
#[test]
fn merkle_diff_handles_splits_merges_and_unchanged_subtrees() {
    let t = tempfile::tempdir().unwrap();
    let o = Objects::buffered(t.path().join("objects")).unwrap();
    let mut a = None;
    for i in 0..80 {
        a = tree::set(
            &o,
            &a,
            &format!("k{i}"),
            Some(json!(i)),
            &mut ReadStats::default(),
        )
        .unwrap();
    }
    let mut b = a.clone();
    for i in 0..65 {
        b = tree::set(&o, &b, &format!("k{i}"), None, &mut ReadStats::default()).unwrap();
    }
    b = tree::set(&o, &b, "new", Some(json!(99)), &mut ReadStats::default()).unwrap();
    let mut changed = BTreeSet::new();
    tree::diff(&o, &a, &b, &mut ReadStats::default(), |key, old, new| {
        assert_ne!(old, new);
        changed.insert(key.to_owned());
        Ok(())
    })
    .unwrap();
    assert_eq!(changed.len(), 66);
    let mut empty = 0;
    tree::diff(&o, &b, &b, &mut ReadStats::default(), |_, _, _| {
        empty += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(empty, 0);
}

#[test]
fn large_retention_plans_have_bounded_handles_and_hash_checked_originals() {
    use cq_store::revisions::retention::RetentionPlan;
    let t = tempfile::tempdir().unwrap();
    let h = StoreHost::open(t.path(), "review-plan").unwrap();
    let plan = RetentionPlan {
        generation: 1,
        revisions: vec![],
        objects: (0..65537).map(|i| format!("{i:064x}")).collect(),
        bytes: 42,
    };
    let response = h.plan_response(&plan).unwrap();
    assert!(serde_json::to_vec(&response).unwrap().len() < 4096);
    let id = response["plan_file"].as_str().unwrap();
    let restored = h.read_plan_reference(id).unwrap();
    assert_eq!(restored.objects, plan.objects);
    std::fs::write(response["path"].as_str().unwrap(), b"{}").unwrap();
    assert_eq!(
        h.read_plan_reference(id).unwrap_err().code(),
        "DigestMismatch"
    );
}
