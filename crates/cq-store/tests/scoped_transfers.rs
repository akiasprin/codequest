use cq_core::{
    analyst::native::{self, NativeUnitInput},
    model::*,
    ports::{ContentRepository, facts::*},
    semantic::{
        production::{self, ScopeDescriptor},
        scope::*,
        transfer::{self, Base, Sink},
    },
};
use cq_store::LocalStore;
use serde_json::json;
use std::collections::BTreeMap;
fn raw(store: &LocalStore, root: &std::path::Path, run: &str, second: bool) -> SemanticSnapshot {
    let mut s = SemanticSnapshot::empty("scoped".into(), NativeValue::new(root));
    s.previous = store.head("scoped").unwrap();
    s.analysis_request = Some(json!({"allow_partial":false,"selected_second":second}));
    s.builds.push(BuildObservation {
        id: run.into(),
        language: Language::Cpp,
        command: NativeValue::from("clang++"),
        arguments: vec![],
        cwd: NativeValue::new(root),
        outcome: BuildOutcome::Succeeded,
        exit: ExitObservation {
            code: Some(0),
            signal: None,
            spawn_error: None,
        },
        events: vec![],
        captured_report: None,
        gaps: vec![],
    });
    let provenance = s.intern_provenance(Provenance {
        provider: "controlled-native".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: Some(run.into()),
        note: "compiler-output contract fixture".into(),
    });
    for name in if second { vec!["a", "b"] } else { vec!["a"] } {
        let file = root.join(format!("{name}.cpp"));
        if !file.exists() {
            std::fs::write(&file, format!("int {name}(){{return 1;}}\n")).unwrap();
        }
        let f = store
            .content
            .freeze(&file, "test compiler input", 1024)
            .unwrap();
        let u = native::unit(NativeUnitInput {
            language: Language::Cpp,
            run: run.into(),
            name: file.display().to_string(),
            root: NativeValue::new(&file),
            arguments: vec![NativeValue::from("clang++"), NativeValue::new(&file)],
            cwd: NativeValue::new(root),
            files: vec![NativeValue::new(&file)],
            invocation: Observed::Value(format!("{run}-{name}")),
        });
        s.entities.push(Entity {
            id: format!("{run}-{name}"),
            unit: u.id.clone(),
            provider_id: format!("usr:{name}"),
            kind: EntityKind::Function,
            name: name.into(),
            qualified_name: name.into(),
            stable_handle: None,
            locations: Default::default(),
            source: Some(SourceAnchor {
                file_revision: f.id.clone(),
                start: 0,
                end: f.blob.len,
            }),
            extent: None,
            signature: "int()".into(),
            parent: None,
            external: false,
            exported: true,
            attributes: Default::default(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: provenance.clone(),
        });
        s.units.push(u);
        s.sources.files.push(f);
    }
    s.sources = SourceSnapshot::seal(
        s.sources.files.clone(),
        InputBinding::PrePostChecked,
        vec![],
    );
    s.analysis_inputs = s.sources.files.clone();
    s.working_tree_manifest = cq_core::source_tree::manifest(root).unwrap();
    cq_core::semantic::identity::assign(&mut s).unwrap();
    s
}
#[test]
fn real_sparse_updates_reuse_payloads_delete_explicitly_and_preserve_old_views() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let store = LocalStore::open(&t.path().join("store")).unwrap();
    let first = raw(&store, &source, "first", true);
    let mut oracle = first.clone();
    production::canonicalize(&mut oracle).unwrap();
    let prepared = store.prepare_analysis(first.clone()).unwrap();
    assert!(prepared.header.entities.is_empty());
    assert!(prepared.header.production.as_ref().unwrap().streamed);
    let r1 = store.publish_analysis(&prepared).unwrap();
    drop(prepared);
    let fixed = store.open_facts("scoped", Some(&r1.revision)).unwrap();
    let exported = store.load_semantics("scoped", Some(&r1.revision)).unwrap();
    assert_eq!(
        serde_json::to_value(exported.entities).unwrap(),
        serde_json::to_value(oracle.entities).unwrap()
    );
    let current = store
        .prepare_analysis(raw(&store, &source, "same-new-observation", true))
        .unwrap();
    let r2 = store.publish_analysis(&current).unwrap();
    assert_eq!(r2.metadata["transfer"]["payload_rows_submitted"], 0);
    assert!(r2.metadata["transfer"]["reused_scopes"].as_u64().unwrap() > 0);
    drop(current);
    let b = cq_core::query::paged::Engine {
        reader: &fixed,
        model: None,
        control: None,
    }
    .resolve("b", &Default::default())
    .unwrap();
    let old = fixed.get("entity", &b.id).unwrap().unwrap().reference;
    std::fs::write(source.join("a.cpp"), "int a(){return 2;}\n").unwrap();
    let change = store
        .prepare_analysis(raw(&store, &source, "edited", true))
        .unwrap();
    let r3 = store.publish_analysis(&change).unwrap();
    assert_eq!(r3.metadata["transfer"]["payload_rows_submitted"], 1);
    assert!(r3.metadata["transfer"]["delta_scopes"].as_u64().unwrap() > 0);
    drop(change);
    let view = store.open_facts("scoped", None).unwrap();
    assert_eq!(
        serde_json::to_value(old).unwrap(),
        serde_json::to_value(view.get("entity", &b.id).unwrap().unwrap().reference).unwrap()
    );
    drop(view);
    let exit = store
        .prepare_analysis(raw(&store, &source, "removed", false))
        .unwrap();
    let r4 = store.publish_analysis(&exit).unwrap();
    assert!(r4.changes.facts_deleted > 0);
    assert!(
        store
            .open_facts("scoped", None)
            .unwrap()
            .get("entity", &b.id)
            .unwrap()
            .is_none()
    );
    assert!(fixed.get("entity", &b.id).unwrap().is_some());
    let mut exported = store.load_semantics("scoped", None).unwrap();
    let revision = exported.revision.clone();
    cq_core::semantic::validate::validate(&exported, &store.content).unwrap();
    exported.seal().unwrap();
    assert_eq!(
        revision, exported.revision,
        "streamed identity round-trips through explicit export"
    );
}
struct MemoryBase {
    revision: String,
    scope: transfer::BaseScope,
}
#[test]
fn incremental_statistics_match_full_oracle_across_context_boundaries() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let store = LocalStore::open(&t.path().join("store")).unwrap();
    for step in 0..4 {
        let mut s = raw(&store, &source, &format!("run-{step}"), true);
        let a = s.entities.iter().find(|e| e.name == "a").unwrap().clone();
        let b = s.entities.iter().find(|e| e.name == "b").unwrap().clone();
        s.contexts = vec![
            AnalysisContext {
                id: "local".into(),
                entry_unit: a.unit.clone(),
                units: vec![a.unit.clone()],
                status: Support::Supported,
                gaps: vec![],
            },
            AnalysisContext {
                id: "both".into(),
                entry_unit: b.unit.clone(),
                units: vec![a.unit.clone(), b.unit.clone()],
                status: Support::Supported,
                gaps: vec![],
            },
        ];
        if step < 3 {
            s.add_relation(
                &a.id,
                if step == 1 { &a.id } else { &b.id },
                if step == 2 {
                    RelationKind::References
                } else {
                    RelationKind::Calls
                },
                None,
                None,
                a.provenance.clone(),
            );
        }
        let p = store.prepare_analysis(s).unwrap();
        let expected = p.spool.statistics(&p.header).unwrap();
        for context in &p.header.contexts {
            let kinds = &expected.contexts[&context.id]["relation_kinds"];
            let calls = if step < 2 && (step == 1 || context.units.len() == 2) {
                1
            } else {
                0
            };
            let references = if step == 2 && context.units.len() == 2 {
                1
            } else {
                0
            };
            assert_eq!(kinds["CALLS"].as_u64().unwrap_or(0), calls);
            assert_eq!(kinds["REFERS_TO"].as_u64().unwrap_or(0), references);
        }
        let r = store.publish_analysis(&p).unwrap();
        assert_eq!(r.metadata["counts"], expected.counts, "step {step}");
        assert_eq!(r.metadata["entity_kinds"], json!(expected.entity_kinds));
        assert_eq!(r.metadata["relation_kinds"], json!(expected.relation_kinds));
        let view = store.open_facts("scoped", None).unwrap();
        for (id, value) in &expected.contexts {
            assert_eq!(
                view.get("context_stats", id).unwrap().unwrap().value,
                *value
            );
        }
        assert_eq!(
            view.get("search_stats", "corpus").unwrap().unwrap().value["terms"],
            expected.terms
        );
        for (term, n) in &expected.df {
            assert_eq!(view.get("term_df", term).unwrap().unwrap().value, *n);
        }
    }
}
impl Base for MemoryBase {
    fn revision(&self) -> Option<&str> {
        Some(&self.revision)
    }
    fn keys(&self) -> cq_core::Result<Vec<ScopeKey>> {
        Ok(vec![self.scope.descriptor.key.clone()])
    }
    fn scope(&self, k: &ScopeKey) -> cq_core::Result<Option<transfer::BaseScope>> {
        Ok((k == &self.scope.descriptor.key).then(|| self.scope.clone()))
    }
}
fn transfers() -> (
    MemoryBase,
    production::Production,
    transfer::Begin,
    production::Record,
) {
    let key = ScopeKey {
        unit: "u".into(),
        context: digest(b"u"),
        family: FactFamily::Entity,
    };
    let value = json!({"id":"a","name":"old"});
    let bytes = serde_json::to_vec(&value).unwrap();
    let records = BTreeMap::from([(
        "entity:a".into(),
        ContentRef {
            hash: digest(&bytes),
            bytes: bytes.len() as u64,
        },
    )]);
    let descriptor = ScopeDescriptor {
        key: key.clone(),
        input_witness: digest(b"inputs"),
        completion: Completion::Complete,
        rows: 1,
        digest: transfer::state_digest(&records).unwrap(),
    };
    let base = MemoryBase {
        revision: digest(b"revision"),
        scope: transfer::BaseScope {
            descriptor: descriptor.clone(),
            records,
        },
    };
    let row = production::Record {
        key: "entity:a".into(),
        value: json!({"id":"a","name":"new"}),
    };
    let bytes = serde_json::to_vec(&row.value).unwrap();
    let current = BTreeMap::from([(
        row.key.clone(),
        ContentRef {
            hash: digest(&bytes),
            bytes: bytes.len() as u64,
        },
    )]);
    let mut next = descriptor;
    next.digest = transfer::state_digest(&current).unwrap();
    let begin = transfer::plan(&next, current, &base).unwrap();
    let p = production::Production {
        scopes: vec![next],
        ..Default::default()
    };
    (base, p, begin, row)
}
#[test]
fn sparse_transfer_rejects_stale_base_unknown_deletes_and_conflicting_receipts() {
    let (base, p, begin, row) = transfers();
    let mut valid = transfer::Validator::new(&p, &base).unwrap();
    valid.begin(&begin).unwrap();
    valid.batch(&begin.descriptor.key, 0, &[row]).unwrap();
    valid.end(&begin.descriptor.key, 1).unwrap();
    valid.finish().unwrap();
    let mut stale = begin.clone();
    stale.base_revision = Some(digest(b"other"));
    assert!(
        transfer::Validator::new(&p, &base)
            .unwrap()
            .begin(&stale)
            .is_err()
    );
    let mut missing = begin.clone();
    missing.receipt = Receipt::new(
        begin.descriptor.key.clone(),
        Completion::Complete,
        Output::Delta {
            base: transfer::base_root(&begin.descriptor.key, &base.scope.records).unwrap(),
            upserts: BTreeMap::new(),
            deletes: std::collections::BTreeSet::from(["entity:unknown".into()]),
        },
    )
    .unwrap();
    assert!(
        transfer::Validator::new(&p, &base)
            .unwrap()
            .begin(&missing)
            .is_err()
    );
    let mut corrupt = begin.clone();
    corrupt.receipt.digest = "0".repeat(64);
    assert!(
        transfer::Validator::new(&p, &base)
            .unwrap()
            .begin(&corrupt)
            .is_err()
    );
}
#[test]
fn missing_wrong_and_duplicate_payloads_poison_the_transaction() {
    let (base, p, begin, row) = transfers();
    let mut missing = transfer::Validator::new(&p, &base).unwrap();
    missing.begin(&begin).unwrap();
    assert!(missing.end(&begin.descriptor.key, 0).is_err());
    assert!(missing.finish().is_err());
    let mut wrong = transfer::Validator::new(&p, &base).unwrap();
    wrong.begin(&begin).unwrap();
    let mut forged = row.clone();
    forged.value["name"] = json!("forged");
    assert!(wrong.batch(&begin.descriptor.key, 0, &[forged]).is_err());
    assert!(wrong.finish().is_err());
    let mut duplicate = transfer::Validator::new(&p, &base).unwrap();
    duplicate.begin(&begin).unwrap();
    duplicate
        .batch(&begin.descriptor.key, 0, std::slice::from_ref(&row))
        .unwrap();
    assert!(duplicate.batch(&begin.descriptor.key, 1, &[row]).is_err());
}
#[test]
fn reuse_requires_the_same_input_witness_and_transfers_no_payload() {
    let (base, _, _, row) = transfers();
    let p = production::Production {
        scopes: vec![base.scope.descriptor.clone()],
        ..Default::default()
    };
    let begin = transfer::plan(&base.scope.descriptor, base.scope.records.clone(), &base).unwrap();
    assert!(matches!(begin.receipt.output, Output::Reuse { .. }));
    let mut v = transfer::Validator::new(&p, &base).unwrap();
    v.begin(&begin).unwrap();
    v.end(&begin.descriptor.key, 0).unwrap();
    v.finish().unwrap();
    let mut invalid = begin.clone();
    invalid.receipt = Receipt::new(
        begin.descriptor.key.clone(),
        Completion::Complete,
        Output::Reuse {
            base: transfer::base_root(&begin.descriptor.key, &base.scope.records).unwrap(),
            input_witness: digest(b"unproved"),
        },
    )
    .unwrap();
    assert!(
        transfer::Validator::new(&p, &base)
            .unwrap()
            .begin(&invalid)
            .is_err()
    );
    let mut v = transfer::Validator::new(&p, &base).unwrap();
    v.begin(&begin).unwrap();
    assert!(v.batch(&begin.descriptor.key, 0, &[row]).is_err());
}
#[test]
fn cwd_is_part_of_the_native_compilation_identity() {
    let unit = |cwd| {
        native::unit(NativeUnitInput {
            language: Language::Cpp,
            run: "run".into(),
            name: "/repo/main.cpp".into(),
            root: NativeValue::from("/repo/main.cpp"),
            arguments: vec![
                NativeValue::from("clang++"),
                NativeValue::from("-Iinclude"),
                NativeValue::from("/repo/main.cpp"),
            ],
            cwd: NativeValue::from(cwd),
            files: vec![],
            invocation: Observed::Value("inv".into()),
        })
    };
    assert_ne!(
        cq_core::semantic::identity::unit_key(&unit("/repo/a"), "/repo").unwrap(),
        cq_core::semantic::identity::unit_key(&unit("/repo/b"), "/repo").unwrap()
    );
}
#[test]
fn diagnostic_severity_is_a_completion_condition() {
    assert!(cq_core::semantic::diagnostics::read(&json!({"diagnostics":"malformed"})).is_err());
    let good = cq_core::semantic::diagnostics::read(
        &json!({"diagnostics":[{"severity":"warning","message":"warning"}]}),
    )
    .unwrap();
    assert!(!good[0].is_error());
    let errors=cq_core::semantic::diagnostics::read(&json!({"diagnostics":[{"severity":"fatal","message":"failed"},"unclassified old producer error"]})).unwrap();
    assert!(errors.iter().all(|d| d.is_error()));
    assert!(
        cq_core::semantic::diagnostics::read(
            &json!({"diagnostics":[{"severity":"maybe","message":"unknown"}]})
        )
        .is_err()
    );
}

#[test]
fn publication_rejects_missing_scope_members_even_with_a_resealed_manifest() {
    use cq_store::{facts::writer, revisions::StoreHost};
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let store = LocalStore::open(&t.path().join("store")).unwrap();
    let prepared = store
        .prepare_analysis(raw(&store, &source, "forged", true))
        .unwrap();
    let host = StoreHost::open(&store.root, "scoped").unwrap();
    let mut candidate = writer::scoped(&host.objects, &store.content, &prepared).unwrap();
    let scope = prepared
        .header
        .production
        .as_ref()
        .unwrap()
        .scopes
        .iter()
        .find(|s| s.rows > 0)
        .unwrap();
    let id = scope.id().unwrap();
    let reference = writer::reference(
        &host.objects,
        &candidate.roots,
        "scope",
        &id,
        &mut ReadStats::default(),
    )
    .unwrap()
    .unwrap();
    let mut row = writer::decode(&host.objects, &reference, &mut ReadStats::default()).unwrap();
    row["members"] = serde_json::Value::Null;
    let mut patch = writer::Writer::new(&host.objects, candidate.roots.clone());
    patch.append("scope", &id, &row).unwrap();
    patch.flush().unwrap();
    candidate.roots = patch.roots;
    candidate.seal().unwrap();
    let result = host.publish(&candidate, None);
    assert_eq!(result.outcome, CommitOutcome::NotCommitted);
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("InvalidScopeMembership"),
        "{result:?}"
    );
    assert!(host.head().unwrap().is_none());
}

#[test]
fn batched_explicit_deletes_preserve_other_refs_memberships_and_audit_entries() {
    use cq_store::facts::{
        objects::Objects,
        tree,
        writer::{self, Group},
    };
    let t = tempfile::tempdir().unwrap();
    let objects = Objects::new(t.path().join("objects")).unwrap();
    let mut w = writer::Writer::new(&objects, Default::default());
    for n in 0..300 {
        let id = format!("item{n:03}");
        w.append(
            "entity",
            &id,
            &json!({"id":id,"name":id,"kind":"Function","unit":"u"}),
        )
        .unwrap();
    }
    w.flush().unwrap();
    let before = w.roots.clone();
    let keys = (0..300)
        .filter(|n| n % 3 != 0)
        .map(|n| format!("entity:item{n:03}"))
        .collect::<Vec<_>>();
    w.delete_many(&keys).unwrap();
    assert_eq!(w.stats.facts_deleted, 200);
    let group: Group = serde_json::from_value(
        tree::get(
            &objects,
            &w.roots.groups,
            "all:entity",
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap(),
    )
    .unwrap();
    assert_eq!(group.count, 100);
    for n in 0..300 {
        let id = format!("item{n:03}");
        let old = writer::reference(&objects, &before, "entity", &id, &mut ReadStats::default())
            .unwrap()
            .unwrap();
        let current =
            writer::reference(&objects, &w.roots, "entity", &id, &mut ReadStats::default())
                .unwrap();
        let audit = tree::get(
            &objects,
            &w.stats.deletes,
            &format!("entity:{id}"),
            &mut ReadStats::default(),
        )
        .unwrap();
        let member = tree::get(
            &objects,
            &group.root,
            &format!("entity:{id}"),
            &mut ReadStats::default(),
        )
        .unwrap();
        if n % 3 == 0 {
            assert_eq!(
                serde_json::to_value(current.unwrap()).unwrap(),
                serde_json::to_value(&old).unwrap()
            );
            assert!(audit.is_none());
            assert!(member.is_some());
        } else {
            assert!(current.is_none());
            assert!(member.is_none());
            assert_eq!(audit.unwrap(), serde_json::to_value(old).unwrap());
            assert!(
                tree::get(
                    &objects,
                    &w.roots.groups,
                    &format!("lookup:{id}"),
                    &mut ReadStats::default()
                )
                .unwrap()
                .is_none()
            );
        }
    }
    let sealed = serde_json::to_value(&w.roots).unwrap();
    assert!(
        w.delete_many(&["entity:item000".into(), "entity:missing".into()])
            .is_err()
    );
    assert!(
        w.delete_many(&["entity:item000".into(), "entity:item000".into()])
            .is_err()
    );
    assert_eq!(sealed, serde_json::to_value(&w.roots).unwrap());
}

#[test]
fn inherited_scope_tokens_and_omitted_unchanged_members_are_not_trusted() {
    use cq_store::{
        facts::{objects::Objects, tree, writer},
        runtime::Client,
    };
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let store = LocalStore::open(&t.path().join("store")).unwrap();
    let p = store
        .prepare_analysis(raw(&store, &source, "initial", true))
        .unwrap();
    let first = store.publish_analysis(&p).unwrap();
    drop(p);
    let p = store
        .prepare_analysis(raw(&store, &source, "new-observation", true))
        .unwrap();
    let o = Objects::new(store.root.join("format-2/objects")).unwrap();
    let r = writer::scoped(&o, &store.content, &p).unwrap();
    let mut selected = None;
    tree::visit(&o, &r.roots.facts, |key, v| {
        if selected.is_none() && key.starts_with("scope:") {
            let f: FactRef = serde_json::from_value(v.clone())?;
            let value = writer::decode(&o, &f, &mut ReadStats::default())?;
            if value["descriptor"]["rows"].as_u64().unwrap_or(0) > 0 {
                selected = Some((key.to_owned(), value));
            }
        }
        Ok(())
    })
    .unwrap();
    let (key, mut value) = selected.unwrap();
    value["state_base"] = json!("0".repeat(64));
    let mut w = writer::Writer::new(&o, r.roots.clone());
    w.append("scope", key.strip_prefix("scope:").unwrap(), &value)
        .unwrap();
    w.flush().unwrap();
    let mut bad = r.clone();
    bad.roots = w.roots;
    let reference = tree::get(&o, &bad.roots.facts, &key, &mut ReadStats::default())
        .unwrap()
        .unwrap();
    bad.changes.upserts = tree::set(
        &o,
        &bad.changes.upserts,
        &key,
        Some(reference),
        &mut ReadStats::default(),
    )
    .unwrap();
    bad.seal().unwrap();
    let result = Client::new(&store.root)
        .unwrap()
        .publish(&bad, Some(&first.revision))
        .unwrap();
    assert_eq!(result.outcome, CommitOutcome::NotCommitted);
    assert!(result.detail.unwrap().contains("base token"));
    let mut missing = value.clone();
    missing["members"] = serde_json::Value::Null;
    missing["descriptor"]["rows"] = json!(0);
    missing["descriptor"]["digest"] = json!(transfer::state_digest(&BTreeMap::new()).unwrap());
    let descriptor: ScopeDescriptor =
        serde_json::from_value(missing["descriptor"].clone()).unwrap();
    missing["state_base"] = json!(transfer::base_root(&descriptor.key, &BTreeMap::new()).unwrap());
    let mut w = writer::Writer::new(&o, r.roots.clone());
    w.append("scope", key.strip_prefix("scope:").unwrap(), &missing)
        .unwrap();
    w.flush().unwrap();
    let mut omitted = r.clone();
    omitted.roots = w.roots;
    let reference = tree::get(&o, &omitted.roots.facts, &key, &mut ReadStats::default())
        .unwrap()
        .unwrap();
    omitted.changes.upserts = tree::set(
        &o,
        &omitted.changes.upserts,
        &key,
        Some(reference),
        &mut ReadStats::default(),
    )
    .unwrap();
    omitted.seal().unwrap();
    let result = Client::new(&store.root)
        .unwrap()
        .publish(&omitted, Some(&first.revision))
        .unwrap();
    assert_eq!(result.outcome, CommitOutcome::NotCommitted);
    assert!(result.detail.unwrap().contains("live semantic fact"));
    assert_eq!(store.head("scoped").unwrap(), Some(first.revision));
    store.publish_analysis(&p).unwrap();
}

#[test]
fn retention_preserves_scopes_inside_jsonl_pages_and_treats_source_json_as_evidence() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let note = source.join("note.json");
    let text = format!("{{\"hash\":\"{}\",\"bytes\":1}}", "0".repeat(64));
    std::fs::write(&note, &text).unwrap();
    let store = LocalStore::open(&t.path().join("store")).unwrap();
    let build = |run: &str| {
        let mut s = raw(&store, &source, run, true);
        let f = store
            .content
            .freeze(&note, "opaque source evidence", 1024)
            .unwrap();
        s.sources.files.push(f.clone());
        s.analysis_inputs.push(f);
        s.sources = SourceSnapshot::seal(
            s.sources.files.clone(),
            InputBinding::PrePostChecked,
            vec![],
        );
        s
    };
    let p = store.prepare_analysis(build("before-gc")).unwrap();
    let r = store.publish_analysis(&p).unwrap();
    drop(p);
    let o =
        cq_store::facts::objects::Objects::buffered(store.root.join("format-2/objects")).unwrap();
    let view = store.open_facts("scoped", Some(&r.revision)).unwrap();
    let mut row = view
        .scan("all:scope", None, 100)
        .unwrap()
        .rows
        .into_iter()
        .find(|row| !row.value["members"].is_null())
        .unwrap();
    // A valid, uncollapsed radix tree can result from deleting most of a scope.
    // Give the same logical members that shape so another canonical directory
    // cannot accidentally keep its root alive and hide a GC traversal defect.
    let root: cq_store::facts::tree::Root =
        serde_json::from_value(row.value["members"].clone()).unwrap();
    let member = cq_store::facts::tree::scan(&o, &root, None, 1, &mut ReadStats::default())
        .unwrap()
        .rows[0]
        .2
        .clone();
    let expanded = cq_store::facts::tree::edit(
        &o,
        &root,
        (0..40)
            .map(|i| (format!("temporary-{i}"), Some(member.clone())))
            .collect(),
        &mut ReadStats::default(),
    )
    .unwrap();
    let alternate = cq_store::facts::tree::edit(
        &o,
        &expanded,
        (0..40).map(|i| (format!("temporary-{i}"), None)).collect(),
        &mut ReadStats::default(),
    )
    .unwrap();
    assert_ne!(root, alternate);
    let alternate = alternate.unwrap();
    row.value["members"] = json!(alternate);
    drop(view);
    let mut w = cq_store::facts::writer::Writer::new(&o, r.roots.clone());
    w.append("scope", row.key.strip_prefix("scope:").unwrap(), &row.value)
        .unwrap();
    w.append(
        "metadata",
        "gc_fixture",
        &json!({"synthetic_metadata_revision":true}),
    )
    .unwrap();
    w.flush().unwrap();
    let mut repacked = r.clone();
    repacked.parent = Some(r.revision.clone());
    repacked.sequence += 1;
    repacked.revision = digest(format!("gc-repack:{}", r.revision).as_bytes());
    repacked.metadata["revision"] = json!(repacked.revision);
    repacked.metadata["previous"] = json!(r.revision);
    repacked.roots = w.roots;
    repacked.changes = w.stats;
    repacked.seal().unwrap();
    o.sync_pending().unwrap();
    let client = cq_store::runtime::Client::new(&store.root).unwrap();
    let result:CommitResult=serde_json::from_value(client.call(json!({"op":"publish","project":"scoped","manifest":repacked,"expected":r.revision})).unwrap()).unwrap();
    assert_eq!(result.outcome, CommitOutcome::Committed, "{:?}", result);
    let view = store
        .open_facts("scoped", Some(&repacked.revision))
        .unwrap();
    let updated = view
        .get("scope", row.key.strip_prefix("scope:").unwrap())
        .unwrap()
        .unwrap();
    let bytes = o
        .read(&updated.reference.object, &mut ReadStats::default())
        .unwrap();
    assert_eq!(
        serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<serde_json::Value>()
            .count(),
        2
    );
    let garbage = o.put(b"unreachable sentinel").unwrap();
    o.sync_pending().unwrap();
    let plan: cq_store::revisions::retention::RetentionPlan = serde_json::from_value(
        client
            .call(json!({"op":"retention_preview","project":"scoped","keep":1}))
            .unwrap(),
    )
    .unwrap();
    assert!(plan.objects.contains(&garbage.hash));
    assert!(!plan.objects.contains(&alternate.hash));
    client
        .call(json!({"op":"retention_apply","project":"scoped","plan":plan}))
        .unwrap();
    assert!(!o.path(&garbage).unwrap().exists());
    let base =
        cq_store::analysis::ScopeBase::open(&store, "scoped", Some(&repacked.revision)).unwrap();
    for key in base.keys().unwrap() {
        base.scope(&key).unwrap().unwrap();
    }
    let p = store.prepare_analysis(build("after-gc")).unwrap();
    store.publish_analysis(&p).unwrap();
}
