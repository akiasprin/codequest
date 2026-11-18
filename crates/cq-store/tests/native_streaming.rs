//! Differential oracle: the new disk transport must preserve every native fact
//! produced by the existing full-frame normalizer and canonical identity pass.
use cq_core::{
    model::*,
    ports::ContentRepository,
    semantic::{self, records::Source},
};
use cq_store::{LocalStore, raw::Journal};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
fn frame() -> Value {
    json!({"protocol":"codequest-hir-export","schema_version":1,
 "files":[{"id":0,"path":"/repo/a.rs","text":"fn run(){target();}"},{"id":1,"path":"/repo/b.rs","text":"fn target(){}"}],
 "symbols":[{"token":0,"key":"run","name":"run","kind":"Function","definition":{"file":0,"start":3,"end":6},"extent":{"file":0,"start":0,"end":19}},{"token":1,"key":"target","name":"target","kind":"Function","definition":{"file":1,"start":3,"end":9},"extent":{"file":1,"start":0,"end":13}}],
 "references":[{"target":1,"kind":"Call","source":{"file":0,"start":9,"end":15},"call_index":0}],
 "parents":[],"relations":[],"bodies":[{"owner":{"file":0,"start":3,"end":6},"source":{"file":0,"start":8,"end":19},"kind":"Function"}],
 "calls":[{"callee":{"file":0,"start":9,"end":15},"arguments":[]}],"doc_links":[]})
}
fn facts(s: &SemanticSnapshot) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    for (k, rows) in [
        ("entity", serde_json::to_value(&s.entities).unwrap()),
        ("body", serde_json::to_value(&s.bodies).unwrap()),
        ("occurrence", serde_json::to_value(&s.occurrences).unwrap()),
        ("relation", serde_json::to_value(&s.relations).unwrap()),
        ("document", serde_json::to_value(&s.documents).unwrap()),
    ] {
        for v in rows.as_array().unwrap() {
            result.insert(format!("{}:{}", k, v["id"].as_str().unwrap()), v.clone());
        }
    }
    result
}
fn compare(raw: Value, language: Language) {
    compare_mapping(raw, language, false)
}
fn compare_mapping(raw: Value, language: Language, mirrored: bool) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let path = dir.path().join("native.json");
    std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    let mut seed = SemanticSnapshot::empty("differential".into(), NativeValue::from("/repo"));
    let mut mapping = vec![];
    let mut units = BTreeSet::new();
    for f in raw["files"].as_array().unwrap() {
        let capture_unit = format!("captured-{}", f["id"]);
        if let Some(unit) = f["unit"]
            .as_str()
            .or(mirrored.then_some(capture_unit.as_str()))
        {
            let logical_path = if mirrored {
                NativeValue::from(format!("/repo/original-{}.rs", f["id"]).as_str())
            } else {
                NativeValue::from(f["path"].as_str().unwrap())
            };
            let blob = store
                .content
                .put(f["text"].as_str().unwrap().as_bytes(), 1024 * 1024 * 1024)
                .unwrap();
            let file = FileRevision {
                id: digest(&serde_json::to_vec(&(&logical_path, &blob)).unwrap()),
                logical_path,
                blob,
                origin: "test".into(),
            };
            mapping.push(MaterializedInput {
                path: f["path"].as_str().unwrap().into(),
                file,
                unit: unit.into(),
            });
            if units.insert(unit.to_owned()) {
                let mut u =
                    cq_core::analyst::native::unit(cq_core::analyst::native::NativeUnitInput {
                        language,
                        run: "run".into(),
                        name: unit.into(),
                        root: NativeValue::from("/repo"),
                        arguments: vec![],
                        cwd: NativeValue::from("/repo"),
                        files: vec![],
                        invocation: Observed::Unknown("test".into()),
                    });
                u.id = unit.into();
                seed.units.push(u);
            }
        }
    }
    let mut full = seed.clone();
    semantic::normalize::native_graph(
        &mut full,
        &raw,
        &mapping,
        &store.content,
        "differential",
        language,
        "1",
    )
    .unwrap();
    let archive = semantic::native_archive::Archive::open(&path).unwrap();
    let journal = Journal::new(dir.path().into()).unwrap();
    let mut thin = seed;
    archive
        .normalize(
            &mut thin,
            &mapping,
            &store.content,
            "differential",
            language,
            "1",
            &journal,
        )
        .unwrap();
    assert!(thin.entities.is_empty() && thin.occurrences.is_empty() && thin.relations.is_empty());
    let mut streamed = BTreeMap::new();
    journal
        .visit(&mut |r| {
            streamed.insert(format!("{}:{}", r.family(), r.id()), r.value()?);
            Ok(())
        })
        .unwrap();
    let expected = facts(&full);
    assert_eq!(
        streamed.len(),
        expected.len(),
        "native row count; only streamed {:?}; only full {:?}",
        streamed
            .keys()
            .filter(|k| !expected.contains_key(*k))
            .take(8)
            .collect::<Vec<_>>(),
        expected
            .keys()
            .filter(|k| !streamed.contains_key(*k))
            .take(8)
            .collect::<Vec<_>>()
    );
    for (k, v) in &expected {
        assert_eq!(streamed.get(k), Some(v), "native fact {k}");
    }
    semantic::identity::assign(&mut full).unwrap();
    let expected = store.prepare_analysis(full).unwrap();
    let actual = store.prepare_records(thin, &journal).unwrap();
    for family in ["entity", "body", "occurrence", "relation", "document"] {
        let mut a = BTreeMap::new();
        let mut b = BTreeMap::new();
        actual
            .spool
            .each(family, |id, v| {
                a.insert(id.to_owned(), v);
                Ok(())
            })
            .unwrap();
        expected
            .spool
            .each(family, |id, v| {
                b.insert(id.to_owned(), v);
                Ok(())
            })
            .unwrap();
        assert_eq!(a, b, "canonical family {family}");
    }
    assert_eq!(actual.spool.sources, expected.spool.sources);
}
#[test]
fn source_frames_preserve_cross_file_calls_and_canonical_facts() {
    compare(frame(), Language::Rust);
}
#[test]
fn mirror_paths_are_resolved_to_captured_logical_source_owners() {
    compare_mapping(frame(), Language::Rust, true);
}
#[test]
fn source_backed_relation_target_without_symbol_row_is_not_lost() {
    let mut raw = frame();
    raw["references"] = json!([]);
    raw["calls"] = json!([]);
    raw["symbols"].as_array_mut().unwrap().truncate(1);
    raw["relations"] = json!([{"from":"run","to":"sysroot-method","file":0,
        "to_file":1,"to_source":{"file":1,"start":3,"end":9},
        "to_kind":"TraitMethod","to_name":"target","to_qn":"Trait::target",
        "kind":"Overrides"}]);
    compare_mapping(raw.clone(), Language::Rust, true);
    compare(raw, Language::Rust);
}
#[test]
fn archive_rejects_utf8_splits_before_emitting_records() {
    let mut raw = frame();
    raw["files"][0]["text"] = json!("é1234567890123456789");
    raw["symbols"][0]["definition"]["start"] = json!(1);
    let t = tempfile::tempdir().unwrap();
    let s = LocalStore::open(t.path()).unwrap();
    let p = t.path().join("bad.json");
    std::fs::write(&p, serde_json::to_vec(&raw).unwrap()).unwrap();
    let a = semantic::native_archive::Archive::open(&p).unwrap();
    let j = Journal::new(t.path().into()).unwrap();
    let mut h = SemanticSnapshot::empty("bad".into(), NativeValue::from("/repo"));
    assert!(
        a.normalize(&mut h, &[], &s.content, "test", Language::Rust, "1", &j)
            .is_err()
    );
    assert_eq!(j.stats()["records"], 0);
}
#[test]
fn archive_does_not_hydrate_provider_owned_nested_coordinates() {
    let mut raw = frame();
    raw["diagnostics"] = json!([{"message":"preserve", "extra":{"__cq_row":0}}]);
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("raw.json");
    std::fs::write(&p, serde_json::to_vec(&raw).unwrap()).unwrap();
    let a = semantic::native_archive::Archive::open(&p).unwrap();
    assert_eq!(a.header().unwrap()["diagnostics"], raw["diagnostics"]);
}
#[test]
#[ignore = "replays retained real compiler exports supplied by CQ_NATIVE_EXPORTS"]
fn real_native_exports_match_legacy_normalization() {
    let manifest = std::env::var_os("CQ_NATIVE_EXPORTS").unwrap();
    let rows: Vec<Value> = serde_json::from_reader(std::fs::File::open(manifest).unwrap()).unwrap();
    assert!(rows.len() >= 4);
    for row in rows {
        let raw: Value = serde_json::from_reader(
            std::fs::File::open(Path::new(row["path"].as_str().unwrap())).unwrap(),
        )
        .unwrap();
        let language = match row["language"].as_str().unwrap() {
            "rust" => Language::Rust,
            "go" => Language::Go,
            "c" => Language::C,
            _ => Language::Cpp,
        };
        eprintln!("native differential {}", row);
        compare(raw, language);
    }
}

#[test]
fn multiple_exports_keep_the_largest_frame_and_accumulate_counts() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let mut s = SemanticSnapshot::empty("transport".into(), NativeValue::from("/repo"));
    s.production = Some(Default::default());
    let journal = Journal::new(t.path().into()).unwrap();
    let p = t.path().join("first.json");
    std::fs::write(&p, serde_json::to_vec(&frame()).unwrap()).unwrap();
    semantic::native_archive::Archive::open(&p)
        .unwrap()
        .normalize(
            &mut s,
            &[],
            &store.content,
            "test",
            Language::Rust,
            "1",
            &journal,
        )
        .unwrap();
    let maximum = s.production.as_ref().unwrap().transport["maximum_file_frame_records"]
        .as_u64()
        .unwrap();
    assert!(maximum > 1);
    let small = json!({"protocol":"codequest-hir-export","schema_version":1,"files":[{"id":3,"path":"/repo/empty.rs","text":""}],"symbols":[],"references":[],"parents":[],"relations":[],"bodies":[],"calls":[],"doc_links":[]});
    let p = t.path().join("second.json");
    std::fs::write(&p, serde_json::to_vec(&small).unwrap()).unwrap();
    semantic::native_archive::Archive::open(&p)
        .unwrap()
        .normalize(
            &mut s,
            &[],
            &store.content,
            "test",
            Language::Rust,
            "1",
            &journal,
        )
        .unwrap();
    let metrics = &s.production.unwrap().transport;
    assert_eq!(metrics["maximum_file_frame_records"], maximum);
    assert_eq!(metrics["native_file_frames"], 5);
    assert_eq!(metrics["native_exports"], 2);
}

#[test]
fn hundred_source_catalogue_preserves_cross_file_reference_closures() {
    let mut raw = frame();
    let mut files = vec![];
    let mut symbols = vec![];
    let mut references = vec![];
    for i in 0..100 {
        files.push(json!({"id":i,"path":format!("/repo/f{i}.rs"),"text":"fn a(){}"}));
        symbols.push(json!({"token":i,"key":format!("s{i}"),"name":format!("a{i}"),"kind":"Function","definition":{"file":i,"start":3,"end":4}}));
        references.push(
            json!({"target":(i+1)%100,"kind":"Reference","source":{"file":i,"start":3,"end":4}}),
        );
    }
    raw["files"] = json!(files);
    raw["symbols"] = json!(symbols);
    raw["references"] = json!(references);
    raw["bodies"] = json!([]);
    raw["calls"] = json!([]);
    compare_mapping(raw, Language::Rust, true);
}
