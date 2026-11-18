use cq_core::{
    model::*,
    ports::ContentRepository,
    semantic::{
        records::{Record, Source, Store},
        stream_ops,
    },
};
use cq_store::{LocalStore, raw::Journal};
use std::{collections::BTreeMap, sync::Mutex};
struct Tracked {
    journal: Journal,
    families: Mutex<Vec<String>>,
}
impl Source for Tracked {
    fn visit(&self, _: &mut dyn FnMut(Record) -> cq_core::Result<()>) -> cq_core::Result<()> {
        panic!("document linking must not scan unrelated record families")
    }
    fn visit_family(
        &self,
        family: &str,
        f: &mut dyn FnMut(Record) -> cq_core::Result<()>,
    ) -> cq_core::Result<()> {
        self.families.lock().unwrap().push(family.into());
        self.journal.visit_family(family, f)
    }
    fn family_len(&self, family: &str) -> cq_core::Result<Option<u64>> {
        self.journal.family_len(family)
    }
    fn get(&self, family: &str, id: &str) -> cq_core::Result<Option<Record>> {
        self.journal.get(family, id)
    }
}
impl Store for Tracked {
    fn put(&self, h: &SemanticSnapshot, r: Record, replace: bool) -> cq_core::Result<()> {
        self.journal.put(h, r, replace)
    }
    fn put_batch(
        &self,
        h: &SemanticSnapshot,
        r: Vec<Record>,
        replace: bool,
    ) -> cq_core::Result<()> {
        self.journal.put_batch(h, r, replace)
    }
    fn task_progress(
        &self,
        p: &str,
        phase: &str,
        label: &str,
        n: u64,
        total: Option<u64>,
        unit: &str,
    ) {
        self.journal.task_progress(p, phase, label, n, total, unit)
    }
}
#[test]
fn documents_drive_symbol_lookup_and_native_targets_remain_authoritative() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("README.md"),
        "# Guide\n\n[run](crate::module::run) [self](#guide) [missing](not_present)\n",
    )
    .unwrap();
    let local = LocalStore::open(&t.path().join("store")).unwrap();
    let mut s = SemanticSnapshot::empty("docs".into(), NativeValue::new(&root));
    let provenance = s.intern_provenance(Provenance {
        provider: "fixture".into(),
        version: "1".into(),
        authority: Authority::AuthorStatement,
        build_run: None,
        note: "authored docs".into(),
    });
    let source = root.join("lib.rs");
    std::fs::write(&source, "fn run(){}\n").unwrap();
    let file = local.content.freeze(&source, "test", 1024).unwrap();
    s.sources = SourceSnapshot::seal(
        vec![file.clone()],
        InputBinding::ImmutableScopeVerified,
        vec![],
    );
    let anchor = SourceAnchor {
        file_revision: file.id.clone(),
        start: 0,
        end: 10,
    };
    let store = Tracked {
        journal: Journal::new(t.path().join("store")).unwrap(),
        families: Mutex::new(vec![]),
    };
    for (id, unit) in [("run-a", "a"), ("run-b", "b")] {
        store
            .put(
                &s,
                Record::Entity(Entity {
                    id: id.into(),
                    stable_handle: None,
                    locations: Default::default(),
                    unit: unit.into(),
                    provider_id: id.into(),
                    kind: EntityKind::Function,
                    name: "run".into(),
                    qualified_name: "module::run".into(),
                    source: Some(anchor.clone()),
                    extent: None,
                    signature: "fn run()".into(),
                    parent: None,
                    external: false,
                    exported: true,
                    attributes: BTreeMap::new(),
                    type_definitions: vec![],
                    type_navigation: vec![],
                    provenance: provenance.clone(),
                }),
                false,
            )
            .unwrap();
    }
    let native_targets = vec![
        Resolution::Resolved("run-b".into()),
        Resolution::Candidates(vec!["run-a".into(), "run-b".into()]),
        Resolution::Unknown("compiler did not resolve".into()),
    ];
    let mut links = native_targets
        .iter()
        .cloned()
        .map(|target| DocumentLink {
            text: "run".into(),
            destination: "module::run".into(),
            target,
            authority: Authority::SemanticResolution,
        })
        .collect::<Vec<_>>();
    links.push(DocumentLink {
        text: "run".into(),
        destination: "module::run".into(),
        target: Resolution::Unknown("explicit authored reference".into()),
        authority: Authority::AuthorStatement,
    });
    store
        .put(
            &s,
            Record::Document(DocumentBlock {
                id: "native-doc".into(),
                file_revision: file.id.clone(),
                kind: DocumentKind::Rustdoc,
                title: "run docs".into(),
                text: "run".into(),
                source: anchor,
                owner: Some("run-a".into()),
                parent: None,
                links,
                provenance,
            }),
            false,
        )
        .unwrap();
    stream_ops::documents(&mut s, &store, &local.content, 1024 * 1024).unwrap();
    let families = store.families.lock().unwrap();
    assert_eq!(families.as_slice(), ["document", "entity", "document"]);
    let Record::Document(d) = store.get("document", "native-doc").unwrap().unwrap() else {
        panic!()
    };
    for (link, expected) in d.links.iter().zip(native_targets) {
        assert_eq!(
            serde_json::to_value(&link.target).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(link.authority, Authority::SemanticResolution);
    }
    assert_eq!(d.links[3].target.resolved(), Some("run-a"));
    let mut author_links = vec![];
    store
        .journal
        .visit_family("document", &mut |r| {
            if let Record::Document(d) = r
                && d.kind == DocumentKind::Markdown
            {
                author_links.extend(d.links);
            }
            Ok(())
        })
        .unwrap();
    assert!(author_links.iter().any(|l|l.destination=="crate::module::run" && matches!(&l.target,Resolution::Candidates(v) if v==&vec!["run-a".to_owned(),"run-b".to_owned()])));
    assert!(
        author_links
            .iter()
            .any(|l| l.destination == "#guide" && l.target.resolved().is_some())
    );
    assert!(
        author_links
            .iter()
            .any(|l| l.destination == "not_present" && matches!(l.target, Resolution::Unknown(_)))
    );
    let p = cq_core::progress::path(&t.path().join("store"), "docs");
    let progress: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    assert_eq!(progress["phase"], "documents.links");
    assert_eq!(progress["completed"], progress["total"]);
    assert!(progress["completed"].as_u64().unwrap() > 0);
}
