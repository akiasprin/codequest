use cq_core::{model::*, ports::ContentRepository};
use cq_store::{
    LocalStore,
    facts::{objects::Objects, writer},
};
use std::collections::BTreeMap;
pub fn fixture(store: &LocalStore, n: usize, target: usize) -> SemanticSnapshot {
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
pub fn write(
    o: &Objects,
    store: &LocalStore,
    s: &SemanticSnapshot,
    base: Option<&writer::Revision>,
) -> writer::Revision {
    writer::snapshot(o, &store.content, s, base).unwrap()
}
