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

#[test]
fn overview_groups_compiler_modules_preserves_expansion_and_scope_in_both_engines() {
    use cq_core::{
        ports::ContentReader,
        query::{QueryEngine, SearchView},
    };
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let h = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 18, 1);
    for (i, e) in s.entities.iter_mut().enumerate() {
        e.name = "dns".into();
        e.qualified_name = format!("module_{}::dns_{i}", i % 2);
        e.kind = if i < 2 {
            EntityKind::Module
        } else if i < 12 {
            EntityKind::Parameter
        } else {
            EntityKind::Function
        };
        if i >= 2 {
            e.parent = Some(format!("f{}", i % 2));
        }
        e.source = Some(SourceAnchor {
            file_revision: s.sources.files[0].id.clone(),
            start: 0,
            end: 3,
        });
        e.unit = if i == 17 { "different-config" } else { "unit" }.into();
    }
    s.seal().unwrap();
    let r = fixture::write(&h.objects, &store, &s, None);
    assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
    let view = h.open_view(None).unwrap();
    let memory = QueryEngine::new(&s, &store.content as &dyn ContentReader);
    let engine = Engine {
        reader: &view,
        model: None,
        control: None,
    };
    let q = QueryRequest {
        operation: "search".into(),
        text: "dns".into(),
        compact: true,
        search_view: SearchView::Overview,
        limit: 100,
        ..Default::default()
    };
    for result in [
        memory.execute(&q, None).unwrap(),
        engine.execute(&q, None).unwrap(),
    ] {
        let p = result.page;
        assert_eq!(
            p.items.len(),
            3,
            "configurations must not be merged: {:?}",
            p.items
        );
        assert_eq!(
            p.items
                .iter()
                .map(|g| g["matches_in_page"].as_u64().unwrap())
                .sum::<u64>(),
            8
        );
        assert!(p.items.iter().all(|g| g["kind"] == "ModuleMatches"));
        assert!(
            p.items
                .iter()
                .flat_map(|g| g["symbols"].as_array().unwrap())
                .all(|e| e["kind"] != "Parameter")
        );
        assert!(
            p.items
                .iter()
                .any(|g| g["collapsed_in_page"].as_u64().unwrap() > 0)
        );
        let hidden = p
            .hidden
            .iter()
            .find(|h| h.category == "local_symbols")
            .unwrap();
        assert_eq!(hidden.count, None);
        let expand: QueryRequest = serde_json::from_value(hidden.continue_with.clone()).unwrap();
        let all = collect(&view, &expand).0;
        assert_eq!(all.len(), 18, "all local facts remain reachable");
        for g in &p.items {
            let expand: QueryRequest = serde_json::from_value(g["continue_with"].clone()).unwrap();
            assert_eq!(expand.search_view, SearchView::Symbols);
            assert_eq!(collect(&view, &expand).0.len(), 18);
        }
    }
    let mut narrow = q.clone();
    narrow.kind = Some("Parameter".into());
    let p = engine.execute(&narrow, None).unwrap().page;
    assert_eq!(
        p.items
            .iter()
            .map(|g| g["matches_in_page"].as_u64().unwrap())
            .sum::<u64>(),
        10
    );
    assert!(!p.hidden.iter().any(|h| h.category == "local_symbols"));
    let mut tiny = q.clone();
    tiny.limit = 1;
    tiny.max_work = 2;
    let rows = collect(&view, &tiny).0;
    assert_eq!(
        rows.iter()
            .map(|g| g["matches_in_page"].as_u64().unwrap())
            .sum::<u64>(),
        8
    );
    let first = engine.execute(&tiny, None).unwrap();
    if let Some(cursor) = first.continuation {
        let mut changed = tiny.clone();
        changed.search_view = SearchView::Symbols;
        assert_eq!(
            engine.execute(&changed, Some(cursor)).err().unwrap().code(),
            "StaleCursor"
        );
    }
}

#[test]
fn paged_line_positions_share_one_verified_source_read() {
    use cq_core::ports::ContentRepository;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Count<'a> {
        inner: &'a dyn FactReader,
        reads: AtomicU64,
        bytes: AtomicU64,
    }
    impl FactReader for Count<'_> {
        fn project(&self) -> &str {
            self.inner.project()
        }
        fn revision(&self) -> &str {
            self.inner.revision()
        }
        fn metadata(&self) -> &Value {
            self.inner.metadata()
        }
        fn get(&self, f: &str, id: &str) -> cq_core::Result<Option<FactRow>> {
            self.inner.get(f, id)
        }
        fn scan(&self, g: &str, a: Option<&str>, n: usize) -> cq_core::Result<FactBatch> {
            self.inner.scan(g, a, n)
        }
        fn count(&self, g: &str) -> cq_core::Result<u64> {
            self.inner.count(g)
        }
        fn source_range(&self, f: &str, a: u64, b: u64) -> cq_core::Result<Vec<u8>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(b - a, Ordering::Relaxed);
            self.inner.source_range(f, a, b)
        }
        fn stats(&self) -> ReadStats {
            self.inner.stats()
        }
    }
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let host = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 200, 1);
    let text = "let value = 1;\n".repeat(20000);
    let f = &mut s.sources.files[0];
    f.blob = store.content.put(text.as_bytes(), 1024 * 1024).unwrap();
    f.id = digest(&serde_json::to_vec(&(&f.logical_path, &f.blob)).unwrap());
    for (i, e) in s.entities.iter_mut().enumerate() {
        e.source = Some(SourceAnchor {
            file_revision: f.id.clone(),
            start: 200000 + i as u64,
            end: 200001 + i as u64,
        });
    }
    let file = f.logical_path.display();
    s.seal().unwrap();
    let rev = fixture::write(&host.objects, &store, &s, None);
    assert_eq!(host.publish(&rev, None).outcome, CommitOutcome::Committed);
    let view = host.open_view(None).unwrap();
    let count = Count {
        inner: &view,
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    };
    let engine = Engine {
        reader: &count,
        model: None,
        control: None,
    };
    let q = QueryRequest {
        operation: "outline".into(),
        file: Some(file),
        compact: true,
        limit: 200,
        max_bytes: 1024 * 1024,
        timeout_ms: 60000,
        ..Default::default()
    };
    let page = engine.execute(&q, None).unwrap().page;
    assert_eq!(page.items.len(), 200);
    assert!(page.items.iter().all(|e| e["line"].as_u64().is_some()));
    assert_eq!(count.reads.load(Ordering::Relaxed), 1);
    let after = count.bytes.load(Ordering::Relaxed);
    let before = (0..200u64).map(|i| 200000 + i + 512).sum::<u64>();
    assert!(after * 100 < before);
    println!(
        "LINE_READ_BENEFIT {}",
        serde_json::json!({"results":200,"legacy_prefix_bytes":before,
        "cached_requested_bytes":after,"source_range_calls":count.reads.load(Ordering::Relaxed)})
    );
}

#[test]
fn grouped_byte_budget_requeues_every_declaration_in_both_engines() {
    use cq_core::query::{QueryEngine, SearchView};
    use std::collections::BTreeSet;
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let host = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 80, 1);
    for (i, e) in s.entities.iter_mut().enumerate() {
        e.name = "needle".into();
        e.qualified_name = format!("module_{i}::needle");
        e.unit = format!("unit_{i}");
    }
    s.seal().unwrap();
    let r = fixture::write(&host.objects, &store, &s, None);
    assert_eq!(host.publish(&r, None).outcome, CommitOutcome::Committed);
    let view = host.open_view(None).unwrap();
    let memory = QueryEngine::new(&s, &store.content);
    let paged = Engine {
        reader: &view,
        model: None,
        control: None,
    };
    for compact in [false, true] {
        for regex in [false, true] {
            let q = QueryRequest {
                operation: "search".into(),
                text: "needle".into(),
                search_view: SearchView::Overview,
                limit: 60,
                max_bytes: 8192,
                compact,
                regex,
                ..Default::default()
            };
            for engine in 0..2 {
                let mut cursor = None;
                let mut ids = vec![];
                let mut byte_limited = false;
                for _ in 0..100 {
                    let r = if engine == 0 {
                        memory.execute(&q, cursor).unwrap()
                    } else {
                        paged.execute(&q, cursor).unwrap()
                    };
                    let output = if compact {
                        r.page.to_compact_json()
                    } else {
                        serde_json::to_value(&r.page).unwrap()
                    };
                    assert!(serde_json::to_vec(&output).unwrap().len() <= q.max_bytes);
                    byte_limited |=
                        r.page.ranking.as_ref().unwrap()["disclosure"]["byte_limited"] == true;
                    for g in r.page.items {
                        for e in g["symbols"].as_array().unwrap() {
                            ids.push(e["id"].as_str().unwrap().to_owned());
                        }
                    }
                    cursor = r.continuation;
                    if cursor.is_none() {
                        break;
                    }
                }
                assert!(cursor.is_none(), "continuation must finish");
                assert!(
                    byte_limited,
                    "counterexample must exercise grouped budget deferral"
                );
                assert_eq!(
                    ids.len(),
                    80,
                    "lost or repeated declarations: compact={compact} regex={regex} engine={engine}"
                );
                assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), 80);
            }
        }
    }
}

#[test]
fn frozen_file_identity_reads_are_version_bound_in_both_engines() {
    use cq_core::{ports::ContentRepository, query::QueryEngine};
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let host = StoreHost::open(t.path(), "paged-test").unwrap();
    let a = fixture::fixture(&store, 2, 1);
    let old_file = a.sources.files[0].id.clone();
    let rev = fixture::write(&host.objects, &store, &a, None);
    assert_eq!(host.publish(&rev, None).outcome, CommitOutcome::Committed);
    let old = host.open_view(None).unwrap();
    let q = QueryRequest {
        operation: "source".into(),
        file_revision: Some(old_file.clone()),
        line: Some(1),
        max_lines: 2,
        limit: 2,
        compact: true,
        ..Default::default()
    };
    let memory = QueryEngine::new(&a, &store.content);
    let paged = Engine {
        reader: &old,
        model: None,
        control: None,
    };
    for p in [
        memory.execute(&q, None).unwrap().page,
        paged.execute(&q, None).unwrap().page,
    ] {
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0]["text"], "frozen source\n");
    }
    let mut b = a.clone();
    let f = &mut b.sources.files[0];
    f.blob = store.content.put(b"changed source\n", 1024).unwrap();
    f.id = digest(&serde_json::to_vec(&(&f.logical_path, &f.blob)).unwrap());
    b.seal().unwrap();
    let next = fixture::write(&host.objects, &store, &b, Some(&rev));
    assert_eq!(
        host.publish(&next, Some(&rev.revision)).outcome,
        CommitOutcome::Committed
    );
    let latest = host.open_view(None).unwrap();
    assert!(
        Engine {
            reader: &latest,
            model: None,
            control: None
        }
        .execute(&q, None)
        .is_err()
    );
    assert!(
        QueryEngine::new(&b, &store.content)
            .execute(&q, None)
            .is_err()
    );
    assert_eq!(
        paged.execute(&q, None).unwrap().page.items[0]["text"],
        "frozen source\n"
    );
}
