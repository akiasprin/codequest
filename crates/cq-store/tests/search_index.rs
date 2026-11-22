//! Indexed queries must retain the scan oracle's recall, filters and version.
use cq_core::{
    model::*,
    ports::facts::*,
    query::{QueryContinuation, QueryRequest, paged::Engine},
};
use cq_store::{LocalStore, facts::writer, revisions::StoreHost};
use serde_json::Value;
#[path = "common/paged_fixture.rs"]
mod fixture;
fn collect(view: &dyn FactReader, q: &QueryRequest) -> (Vec<Value>, usize) {
    let engine = Engine {
        reader: view,
        model: None,
        control: None,
    };
    let mut cursor: Option<QueryContinuation> = None;
    let mut rows = vec![];
    let mut max = 0;
    for _ in 0..20000 {
        let r = engine.execute(q, cursor).unwrap();
        rows.extend(r.page.items);
        cursor = r.continuation;
        max = max.max(serde_json::to_vec(&cursor).unwrap().len());
        if cursor.is_none() {
            return (rows, max);
        }
    }
    panic!("query continuation did not finish")
}
#[test]
fn inverted_terms_substrings_regex_docs_and_filters_match_scan_oracle() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 25, 1);
    for (i, e) in s.entities.iter_mut().enumerate() {
        e.name = match i {
            0 => "run".into(),
            1 => "Run".into(),
            2 => "prune".into(),
            3 => "你好世界".into(),
            _ => format!("function_{i}"),
        };
        e.qualified_name = format!("module::{}", e.name);
        e.signature = "fn unrelated()".into();
    }
    let file = s.sources.files[0].clone();
    let provenance = s.entities[0].provenance.clone();
    s.documents.push(DocumentBlock {
        id: "doc".into(),
        file_revision: file.id.clone(),
        kind: DocumentKind::Markdown,
        title: "Author design".into(),
        text: "prefix 你好世界 suffix publish plan".into(),
        source: SourceAnchor {
            file_revision: file.id,
            start: 0,
            end: 0,
        },
        owner: Some("f0".into()),
        parent: None,
        links: vec![],
        provenance,
    });
    s.seal().unwrap();
    let r = fixture::write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let indexed = h.open_view(None).unwrap();
    // Same immutable facts with the planner flag disabled is an independent scan
    // oracle, not a second implementation of the candidate-selection algorithm.
    struct Scan<'a>(&'a dyn FactReader, Value);
    impl FactReader for Scan<'_> {
        fn project(&self) -> &str {
            self.0.project()
        }
        fn revision(&self) -> &str {
            self.0.revision()
        }
        fn metadata(&self) -> &Value {
            &self.1
        }
        fn get(&self, f: &str, id: &str) -> cq_core::Result<Option<FactRow>> {
            self.0.get(f, id)
        }
        fn scan(&self, g: &str, a: Option<&str>, n: usize) -> cq_core::Result<FactBatch> {
            self.0.scan(g, a, n)
        }
        fn count(&self, g: &str) -> cq_core::Result<u64> {
            self.0.count(g)
        }
        fn source_range(&self, f: &str, a: u64, b: u64) -> cq_core::Result<Vec<u8>> {
            self.0.source_range(f, a, b)
        }
        fn stats(&self) -> ReadStats {
            self.0.stats()
        }
    }
    let mut meta = indexed.metadata().clone();
    assert_eq!(
        Engine {
            reader: &indexed,
            model: None,
            control: None
        }
        .execute(
            &QueryRequest {
                operation: "semantic".into(),
                text: "run".into(),
                ..Default::default()
            },
            None
        )
        .err()
        .unwrap()
        .code(),
        "VectorSearchDisabled"
    );
    assert_eq!(indexed.count("all:entity_vector").unwrap(), 0);
    meta["search_index"] = Value::Null;
    let scan = Scan(&indexed, meta);
    for (text, regex) in [
        ("run", false),
        ("ru", false),
        ("你好世", false),
        ("^run$", true),
        ("^Run$", true),
        ("run$", true),
        ("absent", false),
    ] {
        let q = QueryRequest {
            operation: "search".into(),
            text: text.into(),
            regex,
            limit: 2,
            max_work: 8,
            ..Default::default()
        };
        let a = collect(&indexed, &q).0;
        let b = collect(&scan, &q).0;
        let normalize = |mut rows: Vec<Value>| {
            rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            rows
        };
        assert_eq!(normalize(a.clone()), normalize(b), "{text}");
        if text == "run" && !regex {
            assert!(
                a.iter().any(|r| r["name"] == "prune"),
                "substring must not be lost"
            );
        }
    }
    let q = QueryRequest {
        operation: "doc_search".into(),
        text: "你好世界 suffix".into(),
        limit: 1,
        ..Default::default()
    };
    assert_eq!(collect(&indexed, &q).0, collect(&scan, &q).0);
    assert_eq!(collect(&indexed, &q).0.len(), 1);
    let q = QueryRequest {
        operation: "search".into(),
        text: "run".into(),
        kind: Some("Struct".into()),
        ..Default::default()
    };
    assert!(collect(&indexed, &q).0.is_empty());
    let q = QueryRequest {
        operation: "search".into(),
        text: "run".into(),
        file: Some("not-this-file.rs".into()),
        ..Default::default()
    };
    assert!(collect(&indexed, &q).0.is_empty());
}
#[test]
fn bounded_rank_windows_page_past_one_thousand_without_loss_or_large_cursor() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let s = fixture::fixture(&store, 1050, 1);
    let r = fixture::write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let view = h.open_view(None).unwrap();
    let q = QueryRequest {
        operation: "search".into(),
        text: "fn".into(),
        limit: 100,
        max_work: 89,
        ..Default::default()
    };
    let (rows, bytes) = collect(&view, &q);
    assert_eq!(rows.len(), 1050);
    assert_eq!(
        rows.iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1050
    );
    assert!(bytes < 1024 * 1024, "cursor was {bytes} bytes");
}
#[test]
fn updated_deleted_and_old_entities_keep_separate_frozen_postings() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut a = fixture::fixture(&store, 3, 1);
    a.entities[0].name = "oldneedle".into();
    a.seal().unwrap();
    let first = writer::snapshot(&h.objects, &store.content, &a, None).unwrap();
    assert_eq!(h.publish(&first, None).outcome, CommitOutcome::Committed);
    let old = h.open_view(None).unwrap();
    let mut b = a.clone();
    b.entities[0].name = "newneedle".into();
    b.entities.remove(2);
    b.previous = Some(a.revision.clone());
    b.seal().unwrap();
    let next = writer::snapshot(&h.objects, &store.content, &b, Some(&first)).unwrap();
    assert_eq!(
        h.publish(&next, Some(&first.revision)).outcome,
        CommitOutcome::Committed
    );
    let new = h.open_view(None).unwrap();
    let q = QueryRequest {
        operation: "search".into(),
        text: "oldneedle".into(),
        ..Default::default()
    };
    assert_eq!(collect(&old, &q).0.len(), 1);
    assert!(collect(&new, &q).0.is_empty());
    let q = QueryRequest {
        text: "newneedle".into(),
        ..q
    };
    assert_eq!(collect(&new, &q).0.len(), 1);
    assert!(new.get("entity_vector", "f2").unwrap().is_none());
    assert!(old.get("entity_vector", "f2").unwrap().is_none());
    assert!(new.get("entity_vector", "f1").unwrap().is_none());
    assert!(
        cq_core::query::architecture::cypher_count(new.metadata(), "MATCH (n) RETURN count(n)")
            .is_some()
    );
    assert!(
        cq_core::query::architecture::cypher_count(
            new.metadata(),
            "MATCH (n) WHERE n.name='x' RETURN count(n)"
        )
        .is_none()
    );
}

#[test]
fn text_first_page_prioritizes_exact_names_and_keeps_lexical_continuations() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 200, 1);
    for (i, e) in s.entities.iter_mut().enumerate() {
        e.name = if i == 199 {
            "stamp_rule_tags".into()
        } else {
            format!("other_rule_{i}")
        };
        e.qualified_name = format!("module::{}", e.name);
    }
    s.seal().unwrap();
    let r = fixture::write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let view = h.open_view(None).unwrap();
    let q = QueryRequest {
        operation: "search".into(),
        text: "stamp_rule_tags".into(),
        limit: 1,
        max_work: 8,
        ..Default::default()
    };
    let first = Engine {
        reader: &view,
        model: None,
        control: None,
    }
    .execute(&q, None)
    .unwrap();
    assert_eq!(first.page.items.len(), 1);
    assert_eq!(first.page.items[0]["name"], "stamp_rule_tags");
    assert_eq!(first.page.items[0]["match_tier"], "exact_name");
    assert_eq!(
        first.page.ranking.as_ref().unwrap()["globally_sorted"],
        false
    );
    let all = collect(&view, &q).0;
    assert_eq!(all.len(), 200);
    assert_eq!(
        all.iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        200
    );
    assert!(all.iter().skip(1).all(|r| r["match_tier"] == "lexical"));
}
