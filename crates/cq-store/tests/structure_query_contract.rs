//! Query contracts are isolated from compiler adapters. Native black-box tests
//! independently verify that language providers actually supply these facts.
use cq_core::{model::*, ports::ContentRepository, query::*};
use cq_store::LocalStore;
use std::collections::BTreeMap;

fn entity(
    id: &str,
    unit: &str,
    kind: EntityKind,
    parent: Option<&str>,
    source: &SourceAnchor,
    p: &ProvenanceId,
) -> Entity {
    Entity {
        stable_handle: None,
        locations: Default::default(),
        id: id.into(),
        unit: unit.into(),
        provider_id: id.into(),
        kind,
        name: id.into(),
        qualified_name: format!("contract::{id}"),
        source: Some(source.clone()),
        extent: Some(source.clone()),
        signature: String::new(),
        parent: parent.map(str::to_owned),
        external: false,
        exported: true,
        attributes: BTreeMap::new(),
        type_definitions: vec![],
        type_navigation: vec![],
        provenance: p.clone(),
    }
}

fn fixture(store: &LocalStore, root: &std::path::Path) -> SemanticSnapshot {
    let header = root.join("types.hpp");
    let source = root.join("source.cpp");
    std::fs::write(&header, "class Container { int field; void method(); };\n").unwrap();
    std::fs::write(
        &source,
        "void Container::method() {}\nvoid caller() { target(); }\n",
    )
    .unwrap();
    let h = store.content.freeze(&header, "contract", 4096).unwrap();
    let c = store.content.freeze(&source, "contract", 4096).unwrap();
    let hs = SourceAnchor {
        file_revision: h.id.clone(),
        start: 0,
        end: h.blob.len,
    };
    let cs = SourceAnchor {
        file_revision: c.id.clone(),
        start: 0,
        end: c.blob.len,
    };
    let mut s = SemanticSnapshot::empty("structure-contract".into(), NativeValue::new(root));
    s.sources = SourceSnapshot::seal(vec![h, c], InputBinding::PrePostChecked, vec![]);
    let p = Provenance {
        provider: "test-contract".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: "manually specified query contract, not compiler test evidence".into(),
    };
    let pid = s.intern_provenance(p.clone());
    s.entities = vec![
        entity("container", "dep", EntityKind::Class, None, &hs, &pid),
        entity(
            "field",
            "dep",
            EntityKind::Field,
            Some("container"),
            &hs,
            &pid,
        ),
        entity(
            "method",
            "dep",
            EntityKind::Method,
            Some("container"),
            &cs,
            &pid,
        ),
        entity("target", "dep", EntityKind::Function, None, &hs, &pid),
        entity("caller-a", "a", EntityKind::Function, None, &cs, &pid),
        entity("caller-b", "b", EntityKind::Function, None, &cs, &pid),
    ];
    s.contexts = vec![
        AnalysisContext {
            id: "context-a".into(),
            entry_unit: "a".into(),
            units: vec!["dep".into(), "a".into()],
            status: Support::Supported,
            gaps: vec![],
        },
        AnalysisContext {
            id: "context-b".into(),
            entry_unit: "b".into(),
            units: vec!["dep".into(), "b".into()],
            status: Support::Supported,
            gaps: vec![],
        },
    ];
    for unit in ["a", "b"] {
        s.occurrences.push(Occurrence {
            locations: Default::default(),
            id: format!("reference-{unit}"),
            unit: unit.into(),
            owner: Some(format!("caller-{unit}")),
            source: cs.clone(),
            kind: RelationKind::Calls,
            declaration: Resolution::Resolved("target".into()),
            executable: Resolution::Resolved("target".into()),
            dispatch: Dispatch::Direct,
            evaluation: Evaluation::Immediate,
            arguments: vec![],
            argument_bindings: vec![],
            provenance: pid.clone(),
        });
    }
    s
}

#[test]
fn member_filters_apply_to_results_not_container_identity() {
    let root = tempfile::tempdir().unwrap();
    let store = LocalStore::open(root.path().join("state").as_path()).unwrap();
    let s = fixture(&store, root.path());
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "members".into(),
        subject: Some("container".into()),
        kind: Some("Method".into()),
        ..Default::default()
    };
    let rows = engine.execute(&q, None).unwrap().page.items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "method");
    let details = engine
        .execute(
            &QueryRequest {
                operation: "details".into(),
                subject: q.subject.clone(),
                ..q
            },
            None,
        )
        .unwrap();
    assert_eq!(details.page.items[0]["counts"]["members"], 1);
}

#[test]
fn member_source_filter_can_cross_declaration_and_definition_files() {
    let root = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&root.path().join("state")).unwrap();
    let s = fixture(&store, root.path());
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "members".into(),
        subject: Some("container".into()),
        file: Some("source.cpp".into()),
        ..Default::default()
    };
    let rows = engine.execute(&q, None).unwrap().page.items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "method");
}

#[test]
fn reference_file_scope_uses_use_site_instead_of_definition_file() {
    let root = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&root.path().join("state")).unwrap();
    let s = fixture(&store, root.path());
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "references".into(),
        subject: Some("target".into()),
        file: Some("source.cpp".into()),
        ..Default::default()
    };
    assert_eq!(engine.execute(&q, None).unwrap().page.items.len(), 2);
    assert!(
        engine
            .execute(
                &QueryRequest {
                    file: Some("types.hpp".into()),
                    ..q
                },
                None
            )
            .unwrap()
            .page
            .items
            .is_empty()
    );
}

#[test]
fn identical_use_site_bytes_in_distinct_units_remain_context_scoped() {
    let root = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&root.path().join("state")).unwrap();
    let s = fixture(&store, root.path());
    let engine = QueryEngine::new(&s, &store.content);
    let q = QueryRequest {
        operation: "references".into(),
        subject: Some("target".into()),
        context: Some("context-a".into()),
        ..Default::default()
    };
    let rows = engine.execute(&q, None).unwrap().page.items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["unit"], "a");
    let rows = engine
        .execute(
            &QueryRequest {
                context: Some("context-b".into()),
                ..q
            },
            None,
        )
        .unwrap()
        .page
        .items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["unit"], "b");
}
