//! CKG generation assertions ported to the typed CodeQuest shard protocol.
//! Transport fixtures are synthetic; test_foundations.py supplies native builds.
use cq_core::{
    model::*,
    semantic::{generation::*, identity},
};
use cq_store::LocalStore;
use std::collections::BTreeMap;
fn fixture(store: &LocalStore, root: &std::path::Path) -> SemanticSnapshot {
    let mut s = SemanticSnapshot::empty("generation-contract".into(), NativeValue::new(root));
    let p = s.intern_provenance(Provenance {
        provider: "protocol-fixture".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: "synthetic protocol contract".into(),
    });
    for (name, body) in [
        ("alpha", "pub fn alpha() {}\n"),
        ("beta", "pub fn beta() {}\n"),
    ] {
        let path = root.join(format!("{name}.rs"));
        if !path.exists() {
            std::fs::write(&path, body).unwrap()
        }
        use cq_core::ports::ContentRepository;
        let f = store.content.freeze(&path, "fixture", 4096).unwrap();
        let anchor = SourceAnchor {
            file_revision: f.id.clone(),
            start: 0,
            end: f.blob.len,
        };
        s.sources.files.push(f);
        s.entities.push(Entity {
            id: name.into(),
            stable_handle: None,
            locations: Default::default(),
            unit: "unit".into(),
            provider_id: name.into(),
            kind: EntityKind::Function,
            name: name.into(),
            qualified_name: name.into(),
            source: Some(anchor),
            extent: None,
            signature: "fn()".into(),
            parent: None,
            external: false,
            exported: true,
            attributes: BTreeMap::new(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: p.clone(),
        });
    }
    s.sources = SourceSnapshot::seal(s.sources.files, InputBinding::PrePostChecked, vec![]);
    identity::assign(&mut s).unwrap();
    s
}
fn setup() -> (tempfile::TempDir, LocalStore, Generation) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&dir.path().join("store")).unwrap();
    let s = fixture(&store, dir.path());
    let g = project(&s, 1).unwrap();
    g.validate(&store.content).unwrap();
    (dir, store, g)
}
fn resign(delta: &mut Delta, prior: &Generation) {
    let mut shards = if delta.base_generation.is_some() {
        prior.shards.clone()
    } else {
        BTreeMap::new()
    };
    for k in &delta.deletes {
        shards.remove(k);
    }
    for s in &delta.upserts {
        shards.insert(s.key.clone(), s.clone());
    }
    delta.manifest = shards
        .iter()
        .map(|(k, s)| ManifestEntry {
            key: k.clone(),
            digest: s.digest().unwrap(),
        })
        .collect();
    delta.generation = digest(&serde_json::to_vec(&(&delta.universe, &delta.manifest)).unwrap());
}
#[test]
fn full_noop_delta_reload_and_isolated_checkpoint_have_separate_contracts() {
    let (dir, store, first) = setup();
    let initial = first.delta(None);
    let first = Generation::prepare(None, &initial, &store.content).unwrap();
    let noop = first.delta(Some(&first));
    assert!(noop.upserts.is_empty());
    assert!(noop.deletes.is_empty());
    assert_eq!(
        Generation::prepare(Some(&first), &noop, &store.content)
            .unwrap()
            .generation,
        first.generation
    );
    std::fs::write(
        dir.path().join("alpha.rs"),
        "pub fn alpha() { /* changed */ }\n",
    )
    .unwrap();
    let edited = project(&fixture(&store, dir.path()), 2).unwrap();
    let delta = edited.delta(Some(&first));
    assert_eq!(delta.upserts.len(), 1);
    assert!(delta.upserts[0].key.ends_with("alpha.rs"));
    assert!(Generation::prepare(None, &delta, &store.content).is_err());
    let next = Generation::prepare(Some(&first), &delta, &store.content).unwrap();
    assert_ne!(next.generation, first.generation);
    assert!(Generation::prepare(None, &next.delta(None), &store.content).is_ok());
    let mut other = next.clone();
    other.universe = digest(b"another-config");
    other.generation = digest(&serde_json::to_vec(&(&other.universe, &other.manifest)).unwrap());
    let full = other.delta(Some(&next));
    assert!(full.base_generation.is_none());
    assert!(Generation::prepare(Some(&next), &full, &store.content).is_ok());
}
macro_rules! rejects {
    ($name:ident,$edit:expr) => {
        #[test]
        fn $name() {
            let (_dir, store, prior) = setup();
            let mut delta = prior.delta(Some(&prior));
            ($edit)(&mut delta, &prior);
            let before = serde_json::to_vec(&prior).unwrap();
            assert!(Generation::prepare(Some(&prior), &delta, &store.content).is_err());
            assert_eq!(
                before,
                serde_json::to_vec(&prior).unwrap(),
                "failed transaction mutated published generation"
            );
        }
    };
}
rejects!(rejects_unknown_protocol, |d: &mut Delta, _: &Generation| {
    d.version = 2
});
rejects!(rejects_empty_generation, |d: &mut Delta, _: &Generation| d
    .generation
    .clear());
rejects!(
    rejects_malformed_universe,
    |d: &mut Delta, _: &Generation| d.universe = "foreign".into()
);
rejects!(rejects_zero_sequence, |d: &mut Delta, _: &Generation| d
    .sequence =
    0);
rejects!(rejects_stale_sequence, |d: &mut Delta, p: &Generation| d
    .sequence =
    p.sequence);
rejects!(rejects_foreign_project, |d: &mut Delta, _: &Generation| d
    .project =
    "other".into());
rejects!(rejects_foreign_root, |d: &mut Delta, _: &Generation| d
    .root =
    NativeValue::from("/other"));
rejects!(rejects_stale_base, |d: &mut Delta, _: &Generation| d
    .base_generation =
    Some(digest(b"stale")));
rejects!(
    rejects_delta_in_another_universe,
    |d: &mut Delta, _: &Generation| d.universe = digest(b"other-universe")
);
rejects!(
    rejects_unchanged_generation_without_base,
    |d: &mut Delta, _: &Generation| d.base_generation = None
);
rejects!(
    rejects_noncanonical_manifest,
    |d: &mut Delta, _: &Generation| d.manifest.reverse()
);
rejects!(
    rejects_duplicate_manifest,
    |d: &mut Delta, _: &Generation| d.manifest.push(d.manifest[0].clone())
);
rejects!(
    rejects_mismatched_shard_digest,
    |d: &mut Delta, _: &Generation| d.manifest[0].digest = digest(b"bad")
);
rejects!(
    rejects_mismatched_generation_digest,
    |d: &mut Delta, _: &Generation| d.generation = digest(b"bad")
);
rejects!(rejects_missing_delete, |d: &mut Delta, _: &Generation| d
    .deletes
    .push("/missing".into()));
rejects!(rejects_duplicate_delete, |d: &mut Delta, p: &Generation| {
    let k = p.manifest[0].key.clone();
    d.deletes = vec![k.clone(), k];
});
rejects!(
    rejects_noncanonical_delete_order,
    |d: &mut Delta, p: &Generation| {
        d.deletes = p
            .shards
            .keys()
            .filter(|k| k.as_str() != "$metadata")
            .cloned()
            .rev()
            .collect();
        resign(d, p);
    }
);
rejects!(
    rejects_empty_semantic_identity_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.entities[0].id.clear();
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_upsert_delete_collision,
    |d: &mut Delta, p: &Generation| {
        let s = p.shards.values().next().unwrap().clone();
        d.deletes = vec![s.key.clone()];
        d.upserts = vec![s];
    }
);
rejects!(rejects_duplicate_upsert, |d: &mut Delta, p: &Generation| {
    let s = p.shards.values().next().unwrap().clone();
    d.upserts = vec![s.clone(), s];
});
rejects!(
    rejects_source_key_mismatch,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| s.source.is_some())
            .unwrap()
            .clone();
        s.key = "/foreign.rs".into();
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(rejects_empty_shard_key, |d: &mut Delta, p: &Generation| {
    let mut s = p
        .shards
        .values()
        .find(|s| s.source.is_some())
        .unwrap()
        .clone();
    s.key.clear();
    d.upserts = vec![s];
    resign(d, p);
});
rejects!(
    rejects_invalid_source_bounds_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.entities[0].source.as_mut().unwrap().end = 9000;
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_dangling_parent_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.entities[0].parent = Some("foreign".into());
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_dangling_provenance_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.entities[0].provenance = ProvenanceId(digest(b"missing"));
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_dangling_edge_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.relations.push(Relation {
            id: "edge".into(),
            from: s.entities[0].id.clone(),
            to: "missing".into(),
            kind: RelationKind::Calls,
            occurrence: None,
            source: None,
            provenance: s.entities[0].provenance.clone(),
        });
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_dangling_edge_source_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.relations.push(Relation {
            id: "dangling-source".into(),
            from: "missing".into(),
            to: s.entities[0].id.clone(),
            kind: RelationKind::Calls,
            source: None,
            occurrence: None,
            provenance: s.entities[0].provenance.clone(),
        });
        d.upserts = vec![s];
        resign(d, p);
    }
);
#[test]
fn checkpoint_cache_corruption_is_not_a_valid_generation() {
    let (dir, store, _) = setup();
    let mut s = fixture(&store, dir.path());
    s.seal().unwrap();
    store.publish_semantics(&s, None).unwrap();
    assert!(
        store
            .load_checkpoint(&s.project, &s.revision)
            .unwrap()
            .is_some()
    );
    let path = store
        .revision_path(&s.revision)
        .unwrap()
        .join("checkpoint.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["coordinates"]["generation"] = serde_json::json!(digest(b"tampered"));
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(store.load_checkpoint(&s.project, &s.revision).is_err());
    assert_eq!(store.head(&s.project).unwrap(), Some(s.revision));
}

#[test]
fn semantic_deltas_exclude_unreferenced_build_evidence_without_using_extensions() {
    use cq_core::ports::ContentRepository;
    let (dir, store, _) = setup();
    let mut s = fixture(&store, dir.path());
    let output = dir.path().join("compiler-output.d");
    std::fs::write(&output, "first compiler run").unwrap();
    s.sources.files.push(
        store
            .content
            .freeze(&output, "build_evidence", 4096)
            .unwrap(),
    );
    let before = project(&s, 1).unwrap();
    std::fs::write(&output, "second compiler run").unwrap();
    s.sources.files.pop();
    let evidence = store
        .content
        .freeze(&output, "build_evidence", 4096)
        .unwrap();
    s.sources.files.push(evidence.clone());
    let after = project(&s, 2).unwrap();
    assert!(after.delta(Some(&before)).upserts.is_empty());
    // The same extension is retained when a semantic fact actually names it.
    s.entities[0].source = Some(SourceAnchor {
        file_revision: evidence.id,
        start: 0,
        end: 1,
    });
    let anchored = project(&s, 3).unwrap();
    assert!(anchored.shards.contains_key(&output.display().to_string()));
    anchored.validate(&store.content).unwrap();
}

rejects!(
    rejects_duplicate_semantic_entities_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        s.entities.push(s.entities[0].clone());
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_same_id_conflicting_declarations_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| !s.entities.is_empty())
            .unwrap()
            .clone();
        let mut e = s.entities[0].clone();
        e.name = "conflict".into();
        s.entities.push(e);
        d.upserts = vec![s];
        resign(d, p);
    }
);
rejects!(
    rejects_source_digest_corruption_after_resigning,
    |d: &mut Delta, p: &Generation| {
        let mut s = p
            .shards
            .values()
            .find(|s| s.source.is_some())
            .unwrap()
            .clone();
        s.source.as_mut().unwrap().blob.digest = digest(b"missing frozen bytes");
        d.upserts = vec![s];
        resign(d, p);
    }
);
#[test]
fn cross_shard_deletion_cannot_leave_an_edge_target_dangling() {
    let (dir, store, _) = setup();
    let mut s = fixture(&store, dir.path());
    s.relations.push(Relation {
        id: "cross-file".into(),
        from: s.entities[0].id.clone(),
        to: s.entities[1].id.clone(),
        kind: RelationKind::Calls,
        source: s.entities[0].source.clone(),
        occurrence: None,
        provenance: s.entities[0].provenance.clone(),
    });
    let prior = project(&s, 1).unwrap();
    prior.validate(&store.content).unwrap();
    let mut d = prior.delta(Some(&prior));
    d.deletes
        .push(dir.path().join("beta.rs").display().to_string());
    resign(&mut d, &prior);
    assert!(Generation::prepare(Some(&prior), &d, &store.content).is_err());
    prior.validate(&store.content).unwrap();
}

#[test]
fn published_snapshot_and_delta_corruption_are_independently_detected() {
    for file in ["snapshot.json", "generation-delta.json"] {
        let (dir, store, _) = setup();
        let mut s = fixture(&store, dir.path());
        s.seal().unwrap();
        store.publish_semantics(&s, None).unwrap();
        let path = store.revision_path(&s.revision).unwrap().join(file);
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if file == "snapshot.json" {
            v["entities"][0]["name"] = serde_json::json!("silently changed");
        } else {
            v["base_generation"] = serde_json::json!(digest(b"foreign base"));
        }
        std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(
            if file == "snapshot.json" {
                store.load_semantics(&s.project, Some(&s.revision)).is_err()
            } else {
                store.generation_delta(&s.project, &s.revision).is_err()
            },
            "{file}"
        );
        assert_eq!(store.head(&s.project).unwrap(), Some(s.revision));
    }
}
#[test]
fn conflicting_relation_identity_survives_sealing_and_prevents_publication() {
    let (dir, store, _) = setup();
    let mut s = fixture(&store, dir.path());
    let from = s.entities[0].id.clone();
    let to = s.entities[1].id.clone();
    let provenance = s.entities[0].provenance.clone();
    for kind in [RelationKind::Calls, RelationKind::References] {
        s.relations.push(Relation {
            id: "collision".into(),
            from: from.clone(),
            to: to.clone(),
            kind,
            occurrence: None,
            source: None,
            provenance: provenance.clone(),
        });
    }
    s.seal().unwrap();
    assert_eq!(s.relations.len(), 2);
    assert!(store.publish_semantics(&s, None).is_err());
    assert_eq!(store.head(&s.project).unwrap(), None);
}

#[test]
fn sourceless_facts_keep_compilation_context_partitions_in_checkpoints() {
    let (dir, store, _) = setup();
    let mut s = fixture(&store, dir.path());
    for (e, unit) in s.entities.iter_mut().zip(["alpha-unit", "beta-unit"]) {
        e.unit = unit.into();
        e.source = None;
    }
    identity::assign(&mut s).unwrap();
    s.seal().unwrap();
    store.publish_semantics(&s, None).unwrap();
    let g = store
        .load_checkpoint(&s.project, &s.revision)
        .unwrap()
        .unwrap();
    let populated: Vec<_> = g
        .shards
        .values()
        .filter(|s| !s.entities.is_empty())
        .collect();
    assert_eq!(populated.len(), 2);
    for shard in populated {
        assert!(shard.source.is_none());
        let units: std::collections::BTreeSet<_> = shard.entities.iter().map(|e| &e.unit).collect();
        assert_eq!(units.len(), 1);
    }
    assert_eq!(g.materialize().entities.len(), 2);
}

#[test]
fn publishing_another_root_cannot_replace_an_existing_project_namespace() {
    let (dir, store, _) = setup();
    let mut s = fixture(&store, dir.path());
    s.seal().unwrap();
    store.publish_semantics(&s, None).unwrap();
    let head = s.revision.clone();
    s.root = NativeValue::new(dir.path().join("other-root"));
    s.seal().unwrap();
    assert!(store.publish_semantics(&s, Some(&head)).is_err());
    assert_eq!(store.head(&s.project).unwrap(), Some(head));
}

#[test]
fn checkpoint_output_limits_are_checked_before_reading_payloads() {
    for overflow in [false, true] {
        let (dir, store, _) = setup();
        let mut s = fixture(&store, dir.path());
        s.seal().unwrap();
        store.publish_semantics(&s, None).unwrap();
        let path = store
            .revision_path(&s.revision)
            .unwrap()
            .join("checkpoint.json");
        let mut record: cq_store::checkpoint::Checkpoint =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record.blobs.values_mut().next().unwrap().len = if overflow {
            u64::MAX
        } else {
            1024 * 1024 * 1024 + 1
        };
        record.seal().unwrap();
        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        let error = store
            .load_checkpoint(&s.project, &s.revision)
            .unwrap_err()
            .to_string();
        assert!(error.contains("InvalidCheckpoint"), "{error}");
        assert!(
            !error.contains("MissingContent"),
            "payload was read before checking budgets: {error}"
        );
    }
}
