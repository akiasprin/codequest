//! Large native import fixtures run outside protocol/residency unit tests.
use cq_core::model::digest;
use cq_store::{
    facts::{
        objects::Objects,
        writer::{Revision, Writer},
    },
    ladybug::Projection,
};
use serde_json::json;

#[test]
fn copied_slot_ids_preserve_quotes_commas_unicode_and_multiline_text() {
    let rows = [
        json!({"family":"Relation","value":{"id":"edge-1","from":"a,\"quoted\"\n行","to":"b\r\nsecond","kind":"Calls","occurrence":null,"source":null,"provenance":"p"}}),
        json!({"family":"Relation","value":{"id":"edge-2","from":"b\r\nsecond","to":"a,\"quoted\"\n行","kind":"MayCall","occurrence":null,"source":null,"provenance":"p"}}),
    ];
    let bytes = rows
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect::<String>();
    let report = profile_native_relations(bytes.as_bytes());
    assert_eq!(report["physical_counts"]["links"], 2);
    assert_eq!(report["endpoint_count"], 2);
}
#[test]
fn real_brocade_fixture_preserves_all_nineteen_native_relationship_kinds() {
    let bytes = &include_bytes!("fixtures/brocade-native-relations.jsonl")[..];
    let origin: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/brocade-native-relations.origin.json"
    ))
    .unwrap();
    assert_eq!(origin["sha256"], digest(bytes));
    assert_eq!(origin["kinds"].as_object().unwrap().len(), 19);
    let report = profile_native_relations(bytes);
    assert_eq!(report["unique_relations"], 37);
    assert!(report["passed"].as_bool().unwrap());
}
#[test]
#[ignore = "CQ_RELATION_SAMPLE and CQ_NATIVE_IMPORT_REPORT: retained real relation projection"]
fn retained_real_relations_profile_native_import_and_exact_replay() {
    let input = std::env::var("CQ_RELATION_SAMPLE").unwrap();
    let report = std::env::var("CQ_NATIVE_IMPORT_REPORT").unwrap();
    let mut result = profile_native_relations(std::io::BufReader::new(
        std::fs::File::open(&input).unwrap(),
    ));
    result["source"] = json!(input);
    std::fs::write(report, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
}
fn profile_native_relations(reader: impl std::io::Read) -> serde_json::Value {
    use std::time::Instant;
    let t = tempfile::tempdir().unwrap();
    let o = Objects::buffered(t.path().join("objects")).unwrap();
    let mut w = Writer::initial(&o).unwrap();
    let mut expected = std::collections::BTreeMap::new();
    let mut endpoints = std::collections::BTreeSet::new();
    let started = Instant::now();
    for r in serde_json::Deserializer::from_reader(reader)
        .into_iter::<cq_core::semantic::records::Record>()
    {
        let r = r.unwrap();
        assert_eq!(r.family(), "relation");
        let value = r.value().unwrap();
        for field in ["from", "to"] {
            endpoints.insert(value[field].as_str().unwrap().to_owned());
        }
        expected.insert(r.id().to_owned(), value);
    }
    for (id, value) in &expected {
        w.append("relation", id, value).unwrap();
    }
    let (roots, changes) = w.finish_inventory().unwrap();
    o.sync_pending().unwrap();
    let prepare_seconds = started.elapsed().as_secs_f64();
    let mut r = Revision {
        format: 2,
        project: "real-native-component".into(),
        revision: digest(b"real-native-component"),
        parent: None,
        sequence: 1,
        roots,
        changes,
        metadata: json!({}),
        seal: String::new(),
    };
    r.seal().unwrap();
    let p = Projection::open(&t.path().join("native.lbdb"), true).unwrap();
    let start = Instant::now();
    p.import(&o, &r).unwrap();
    let import_seconds = start.elapsed().as_secs_f64();
    let stats = p.import_stats();
    let counts = p.physical_counts().unwrap();
    assert_eq!(counts["facts"], expected.len());
    assert_eq!(counts["links"], expected.len());
    assert_eq!(stats.slots_inserted as usize, endpoints.len());
    assert!(p.receipt(&r).unwrap());
    // Check actual native label/endpoint tuples against the retained input.
    let native_expected = expected
        .iter()
        .map(|(key, v)| {
            (
                digest(
                    format!("relation:{key}:{}", digest(&serde_json::to_vec(v).unwrap()))
                        .as_bytes(),
                ),
                v,
            )
        })
        .collect::<std::collections::HashMap<_, _>>();

    assert_eq!(
        p.rows("MATCH (s:Slot) RETURN count(s)")
            .unwrap()
            .first()
            .unwrap()[0],
        lbug::Value::Int64(endpoints.len() as i64)
    );
    let mut seen = std::collections::HashSet::new();
    for row in p
        .rows("MATCH (a:Slot)-[r:LINK]->(b:Slot) RETURN a.id,b.id,r.id,r.kind")
        .unwrap()
    {
        let [
            lbug::Value::String(from),
            lbug::Value::String(to),
            lbug::Value::String(id),
            lbug::Value::String(kind),
        ] = row.as_slice()
        else {
            panic!("native edge schema")
        };
        // Native relation identities include the canonical payload digest.
        let value = native_expected.get(id).expect("foreign native edge");
        assert!(seen.insert(id.clone()), "duplicate native edge identity");
        assert_eq!(value["from"], *from);
        assert_eq!(value["to"], *to);
        assert_eq!(value["kind"], *kind);
    }
    assert_eq!(seen.len(), native_expected.len());
    drop(p);
    let start = Instant::now();
    let p = Projection::open(&t.path().join("native.lbdb"), false).unwrap();
    p.import(&o, &r).unwrap();
    assert_eq!(p.physical_counts().unwrap(), counts);
    let replay_seconds = start.elapsed().as_secs_f64();
    json!({"passed":true,"unique_relations":expected.len(),"endpoint_count":endpoints.len(),"prepare_seconds":prepare_seconds,"import_seconds":import_seconds,"reopen_replay_seconds":replay_seconds,"physical_counts":counts,"stats":stats,"scope":"real retained relation payloads; exact native endpoints/labels/identities and reopen replay checked; entity closure, compiler, whole-store audit/publication excluded"})
}

#[test]
fn performance_native_import_batches_many_edges_without_repeated_slot_merges() {
    let t = tempfile::tempdir().unwrap();
    let o = Objects::buffered(t.path().join("objects")).unwrap();
    let mut w = Writer::initial(&o).unwrap();
    for id in ["a", "b"] {
        w.append(
            "entity",
            id,
            &json!({"id":id,"kind":"Function","name":id,"unit":"u","external":false}),
        )
        .unwrap();
    }
    let edges = 40_000;
    for i in 0..edges {
        let id = format!("r{i}");
        w.append(
            "relation",
            &id,
            &json!({"id":id,"from":"a","to":"b","kind":"Calls"}),
        )
        .unwrap();
    }
    let (roots, changes) = w.finish_inventory().unwrap();
    o.sync_pending().unwrap();
    let mut r = Revision {
        format: 2,
        project: "batch-regression".into(),
        revision: digest(b"many-edges"),
        parent: None,
        sequence: 1,
        roots,
        metadata: json!({}),
        changes,
        seal: String::new(),
    };
    r.seal().unwrap();
    let p = Projection::open(&t.path().join("native.lbdb"), true).unwrap();
    p.import(&o, &r).unwrap();
    let counts = p.physical_counts().unwrap();
    assert_eq!(counts["facts"], edges + 2);
    assert_eq!(counts["links"], edges);
    assert!(p.receipt(&r).unwrap());
    assert_eq!(p.import_stats().slots_inserted, 2);
    assert_eq!(p.import_stats().slot_copy_statements, 1);
    assert!(
        p.import_stats().transactions <= 4,
        "the 40000-edge fixture exceeded its transaction bound: {}",
        p.import_stats().transactions
    );
    p.import(&o, &r).unwrap();
    assert_eq!(p.physical_counts().unwrap(), counts);
}
