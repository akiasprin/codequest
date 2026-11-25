use cq_core::{
    model::*,
    ports::{ContentRepository, facts::*},
    query::{QueryEngine, QueryRequest, SearchView, paged::Engine},
};
use cq_store::{LocalStore, revisions::StoreHost};
use serde_json::{Value, json};
#[path = "common/paged_fixture.rs"]
mod fixture;

struct Partial<'a>(&'a dyn FactReader, Value);
impl FactReader for Partial<'_> {
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

#[test]
fn unrelated_long_gaps_do_not_consume_search_budget_or_hide_continuations() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let host = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 40, 1);
    for (i, e) in s.entities.iter_mut().enumerate() {
        e.name = format!("self_update_{i}");
        e.qualified_name = format!("test::{}", e.name);
        e.unit = format!("unit-{i}");
    }
    s.gaps = (0..8)
        .map(|i| {
            Gap::new(
                "StaticRelationBindingAmbiguous",
                format!("external-{i}::{}", "unrelated_dependency::".repeat(160)),
                "multiple native targets",
            )
        })
        .collect();
    s.seal().unwrap();
    let revision = fixture::write(&host.objects, &store, &s, None);
    assert_eq!(
        host.publish(&revision, None).outcome,
        CommitOutcome::Committed
    );
    let view = host.open_view(None).unwrap();
    let memory = QueryEngine::new(&s, &store.content);
    let paged = Engine {
        reader: &view,
        model: None,
        control: None,
    };
    for compact in [true, false] {
        let q = QueryRequest {
            operation: "search".into(),
            text: "self_update".into(),
            search_view: SearchView::Overview,
            compact,
            limit: 15,
            max_bytes: 6500,
            ..Default::default()
        };
        for persisted in [false, true] {
            let mut cursor = None;
            let mut ids = std::collections::BTreeSet::new();
            let mut continued = false;
            for _ in 0..30 {
                let r = if persisted {
                    paged.execute(&q, cursor)
                } else {
                    memory.execute(&q, cursor)
                }
                .unwrap();
                assert!(r.page.gaps.is_empty());
                assert!(r.page.hidden.iter().all(|h| h.category != "coverage_gaps"));
                let bytes = serde_json::to_vec(&if compact {
                    r.page.to_compact_json()
                } else {
                    serde_json::to_value(&r.page).unwrap()
                })
                .unwrap()
                .len();
                assert!(bytes <= q.max_bytes);
                for group in &r.page.items {
                    for e in group["symbols"].as_array().unwrap() {
                        assert!(ids.insert(e["id"].as_str().unwrap().to_owned()));
                    }
                }
                cursor = r.continuation;
                if cursor.is_none() {
                    break;
                }
                continued = true;
            }
            assert!(cursor.is_none());
            assert!(continued, "bounded grouped pages must keep a continuation");
            assert_eq!(ids.len(), 40);
        }
    }
    // Full diagnostics remain explicitly accessible, with no duplicated sample
    // in the side-channel gaps field, and can be read one record at a time.
    let q = QueryRequest {
        operation: "gaps".into(),
        limit: 1,
        ..Default::default()
    };
    assert_eq!(
        memory.execute(&q, None).unwrap().page.items[0]["code"],
        "StaticRelationBindingAmbiguous"
    );
    assert_eq!(collect_gaps(&paged, &q), 8);

    let mut metadata = view.metadata().clone();
    metadata["production"] = json!({"partial":true});
    let partial = Partial(&view, metadata);
    let paged = Engine {
        reader: &partial,
        model: None,
        control: None,
    };
    s.production = Some(cq_core::semantic::production::Production {
        partial: true,
        ..Default::default()
    });
    let memory = QueryEngine::new(&s, &store.content);
    for operation in ["search", "doc_search"] {
        let q = QueryRequest {
            operation: operation.into(),
            text: "absent_symbol".into(),
            compact: true,
            ..Default::default()
        };
        for r in [
            memory.execute(&q, None).unwrap(),
            paged.execute(&q, None).unwrap(),
        ] {
            if operation == "search" {
                assert_eq!(r.page.state, "unknown_due_to_incomplete_compilation");
                assert_eq!(r.page.gaps.len(), 1);
                assert_eq!(r.page.gaps[0].code, "IncompleteKnowledge");
            } else {
                assert_eq!(r.page.state, "no_matches_in_declared_scope");
                assert!(r.page.gaps.is_empty());
            }
        }
    }
    let q = QueryRequest {
        operation: "search".into(),
        text: "self_update".into(),
        compact: true,
        ..Default::default()
    };
    assert!(paged.execute(&q, None).unwrap().page.gaps.is_empty());
    assert!(memory.execute(&q, None).unwrap().page.gaps.is_empty());
}

fn collect_gaps(engine: &Engine<'_>, q: &QueryRequest) -> usize {
    let mut cursor = None;
    let mut count = 0;
    for _ in 0..20 {
        let r = engine.execute(q, cursor).unwrap();
        assert!(r.page.gaps.is_empty());
        count += r.page.items.len();
        cursor = r.continuation;
        if cursor.is_none() {
            return count;
        }
    }
    panic!("diagnostic pagination did not finish")
}

#[test]
fn query_local_text_decoding_gap_is_still_disclosed() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let host = StoreHost::open(t.path(), "paged-test").unwrap();
    let mut s = fixture::fixture(&store, 1, 0);
    let f = &mut s.sources.files[0];
    f.blob = store.content.put(b"invalid:\xff\n", 1024).unwrap();
    f.id = digest(&serde_json::to_vec(&(&f.logical_path, &f.blob)).unwrap());
    s.seal().unwrap();
    let revision = fixture::write(&host.objects, &store, &s, None);
    assert_eq!(
        host.publish(&revision, None).outcome,
        CommitOutcome::Committed
    );
    let view = host.open_view(None).unwrap();
    let r = Engine {
        reader: &view,
        model: None,
        control: None,
    }
    .execute(
        &QueryRequest {
            operation: "search_code".into(),
            text: "missing".into(),
            compact: true,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(r.page.gaps.iter().any(|g| g.code == "NonUtf8Source"));
}
