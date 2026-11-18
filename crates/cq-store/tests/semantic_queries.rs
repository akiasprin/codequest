use cq_core::{model::*, ports::ContentRepository, query::*};
use cq_store::LocalStore;
use std::collections::BTreeMap;
fn fixture(store: &LocalStore) -> SemanticSnapshot {
    let mut s = SemanticSnapshot::empty("query-test".into(), NativeValue::from("/fixed/repo"));
    let p = Provenance {
        provider: "test".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: String::new(),
    };
    let provenance = s.intern_provenance(p.clone());
    for i in 0..45 {
        s.entities.push(Entity {
            stable_handle: None,
            locations: Default::default(),
            id: format!("f{i}"),
            unit: "unit".into(),
            provider_id: format!("f{i}"),
            kind: EntityKind::Function,
            name: format!("f{i}"),
            qualified_name: format!("fixture::f{i}"),
            source: None,
            extent: None,
            signature: String::new(),
            parent: None,
            external: false,
            exported: true,
            attributes: BTreeMap::new(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: provenance.clone(),
        });
        if i > 0 {
            s.add_relation(
                &format!("f{}", i - 1),
                &format!("f{i}"),
                RelationKind::Calls,
                None,
                None,
                p.clone(),
            );
        }
    }
    s.seal().unwrap();
    store.publish_semantics(&s, None).unwrap();
    s
}
#[test]
fn paginated_walk_retains_frontier_after_rows_are_split() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let s = fixture(&store);
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "trace".into(),
        subject: Some("f0".into()),
        depth: 100,
        limit: 1,
        max_work: 3,
        ..Default::default()
    };
    let mut next = None;
    let mut count = 0;
    for _ in 0..100 {
        let page = engine.execute(&q, next).unwrap();
        count += page.page.items.len();
        next = page.continuation;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(count, 45);
}
#[test]
fn deep_paths_are_not_cut_off_at_a_hidden_fixed_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let s = fixture(&store);
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "path".into(),
        subject: Some("f0".into()),
        target: Some("f44".into()),
        depth: 100,
        max_work: 4,
        ..Default::default()
    };
    let mut next = None;
    let mut found = None;
    // Work now charges examined edges as well as visited nodes.
    for _ in 0..100 {
        let page = engine.execute(&q, next).unwrap();
        if let Some(row) = page.page.items.first() {
            found = Some(row.clone());
        }
        next = page.continuation;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(
        found.unwrap()["relation_path"].as_array().unwrap().len(),
        44
    );
}
#[test]
fn native_ladybug_queries_and_publish_conflicts_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let s = fixture(&store);
    let result = store
        .cypher(
            &s,
            "MATCH (a:Function)-[:CALLS]->(b:Function) RETURN count(*)",
            5,
            5000,
        )
        .unwrap();
    assert_eq!(result["rows"][0][0], 44);
    assert!(store.cypher(&s, "MATCH (a) DELETE a", 5, 5000).is_err());
    assert!(
        store
            .cypher(
                &s,
                "MATCH (a) RETURN a; CREATE (:Function {id:'bad'})",
                5,
                5000
            )
            .is_err()
    );
    assert!(store.publish_semantics(&s, None).is_err());
}
#[test]
fn cursor_pins_revision_and_rejects_different_query() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let s = fixture(&store);
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "search".into(),
        text: "f".into(),
        limit: 1,
        ..Default::default()
    };
    let state = engine.execute(&q, None).unwrap().continuation.unwrap();
    let id = store.save_cursor(&state).unwrap();
    let state = store.load_cursor(&id).unwrap();
    let other = QueryRequest {
        text: "other".into(),
        ..q
    };
    assert!(engine.execute(&other, Some(state)).is_err());
}
#[test]
fn missing_content_is_not_replaced_by_live_source() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let path = dir.path().join("source.rs");
    std::fs::write(&path, b"old").unwrap();
    let f = store.content.freeze(&path, "fixture", 100).unwrap();
    std::fs::write(&path, b"new").unwrap();
    std::fs::remove_file(store.content.path(&f.blob).unwrap()).unwrap();
    let snapshot = SourceSnapshot::seal(vec![f], InputBinding::PrePostChecked, vec![]);
    assert!(cq_core::input::InputView::open(snapshot, &store.content).is_err());
}

#[test]
fn bounded_search_resumes_until_ranking_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let s = fixture(&store);
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "search".into(),
        text: "f".into(),
        max_work: 3,
        limit: 2,
        ..Default::default()
    };
    let mut next = None;
    let mut ids = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = engine.execute(&q, next).unwrap();
        for item in &page.page.items {
            assert!(ids.insert(item["id"].as_str().unwrap().to_owned()));
        }
        next = page.continuation;
        if next.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 45);
}

#[test]
fn high_fanout_charges_edges_before_expanding_and_resumes_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let mut s = fixture(&store);
    let prototype = s.entities[0].clone();
    let p = s.provenance_records.values().next().unwrap().clone();
    s.entities.clear();
    s.relations.clear();
    for i in 0..1501 {
        let mut e = prototype.clone();
        e.id = format!("hub-{i}");
        e.name = e.id.clone();
        e.qualified_name = e.id.clone();
        s.entities.push(e);
        if i > 0 {
            s.add_relation(
                "hub-0",
                &format!("hub-{i}"),
                RelationKind::Calls,
                None,
                None,
                p.clone(),
            );
        }
    }
    s.seal().unwrap();
    let engine = QueryEngine::new(&s, &store.content);
    let mut q = QueryRequest {
        operation: "trace".into(),
        subject: Some("hub-0".into()),
        depth: 2,
        limit: 2,
        max_work: 4,
        ..Default::default()
    };
    let first = engine.execute(&q, None).unwrap();
    assert!(first.page.work <= 4);
    let continuation = first.continuation.unwrap();
    match &continuation.state {
        ContinuationState::Walk { queue, .. } => assert!(
            queue.len() <= 4,
            "budget must bound queued fanout, not just emitted rows"
        ),
        other => panic!("expected walk frontier: {other:?}"),
    }
    let mut ids: std::collections::HashSet<String> = first
        .page
        .items
        .iter()
        .map(|r| r["entity"]["id"].as_str().unwrap().to_owned())
        .collect();
    let mut next = Some(continuation);
    q.max_work = 64;
    for _ in 0..3002 {
        let page = engine.execute(&q, next).unwrap();
        assert!(page.page.work <= 64);
        assert!(page.page.items.len() <= 2);
        for row in page.page.items {
            assert!(
                ids.insert(row["entity"]["id"].as_str().unwrap().to_owned()),
                "resumed parent/node emitted twice"
            );
        }
        next = page.continuation;
        if next.is_none() {
            break;
        }
    }
    assert!(next.is_none(), "frontier failed to terminate");
    assert_eq!(ids.len(), 1501);
}

#[test]
fn types_focus_never_follows_execution_or_dispatch_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let mut s = fixture(&store);
    let p = s.provenance_records.values().next().unwrap().clone();
    s.add_relation("f0", "f2", RelationKind::Dispatches, None, None, p.clone());
    s.add_relation("f0", "f3", RelationKind::UsesType, None, None, p);
    let engine = QueryEngine::new(&s, &store.content);
    let page = engine
        .execute(
            &QueryRequest {
                operation: "trace".into(),
                subject: Some("f0".into()),
                mode: "types".into(),
                include_candidates: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let ids: Vec<_> = page
        .page
        .items
        .iter()
        .map(|r| r["entity"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["f0", "f3"]);
}
