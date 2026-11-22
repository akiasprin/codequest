//! Regression tests for nested-body graph traversal, TypeScript/TSX semantic
//! indexing, and compact query responses across both the in-memory `QueryEngine`
//! and paged `paged::Engine`:
//! 1. Multi-hop traversal (`impact`, `trace`, `path`) across `Closure` and
//!    `AsyncBlock` nested body boundaries.
//! 2. Frontend TypeScript / TSX semantic indexing (`providers/ts-compiler/index.mjs`
//!    + `cq_core::analyst::ts`).
//! 3. Compact query responses (`compact: true`) and human-readable `references`
//!    (`file`, `line`, `column`, `owner`, `snippet`).

use cq_core::{model::*, ports::ContentRepository, query::*};
use cq_store::LocalStore;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

#[allow(clippy::too_many_arguments)]
fn entity(
    id: &str,
    unit: &str,
    kind: EntityKind,
    name: &str,
    qualified_name: &str,
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
        name: name.into(),
        qualified_name: qualified_name.into(),
        source: Some(source.clone()),
        extent: Some(source.clone()),
        signature: format!("fn {name}()"),
        parent: parent.map(str::to_owned),
        external: false,
        exported: true,
        attributes: BTreeMap::new(),
        type_definitions: vec![],
        type_navigation: vec![],
        provenance: p.clone(),
    }
}

fn nested_body_fixture(store: &LocalStore, root: &Path) -> SemanticSnapshot {
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("pipeline.rs");
    let text = concat!(
        "pub fn leaf_fn() {}\n",
        "pub fn mid_fn() {\n",
        "    let f = || { leaf_fn(); };\n",
        "    f();\n",
        "}\n",
        "pub fn outer_handler() {\n",
        "    let _ = async { mid_fn(); };\n",
        "}\n",
        "pub fn entry_router() {\n",
        "    outer_handler();\n",
        "}\n",
    );
    std::fs::write(&file_path, text).unwrap();
    let f = store.content.freeze(&file_path, "test", 64 * 1024).unwrap();
    let file_id = f.id.clone();
    let span = |needle: &str| {
        let start = text.find(needle).unwrap() as u64;
        SourceAnchor {
            file_revision: file_id.clone(),
            start,
            end: start + needle.len() as u64,
        }
    };

    let mut s = SemanticSnapshot::empty("nested-body-test".into(), NativeValue::new(root));
    s.sources = SourceSnapshot::seal(vec![f], InputBinding::PrePostChecked, vec![]);
    let p = Provenance {
        provider: "test-fixture".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: "synthetic closure/async-block graph".into(),
    };
    let pid = s.intern_provenance(p.clone());

    let leaf_s = span("leaf_fn");
    let mid_s = span("mid_fn");
    let closure_s = span("|| { leaf_fn(); }");
    let leaf_call_s = {
        let pos = text.find("leaf_fn();").unwrap() as u64;
        SourceAnchor {
            file_revision: leaf_s.file_revision.clone(),
            start: pos,
            end: pos + "leaf_fn".len() as u64,
        }
    };
    let outer_s = span("outer_handler");
    let async_s = span("async { mid_fn(); }");
    let mid_call_s = {
        let pos = text.find("mid_fn();").unwrap() as u64;
        SourceAnchor {
            file_revision: leaf_s.file_revision.clone(),
            start: pos,
            end: pos + "mid_fn".len() as u64,
        }
    };
    let router_s = span("entry_router");
    let outer_call_s = {
        let pos = text.find("outer_handler();").unwrap() as u64;
        SourceAnchor {
            file_revision: leaf_s.file_revision.clone(),
            start: pos,
            end: pos + "outer_handler".len() as u64,
        }
    };

    s.entities = vec![
        entity(
            "leaf",
            "u",
            EntityKind::Function,
            "leaf_fn",
            "pipeline::leaf_fn",
            None,
            &leaf_s,
            &pid,
        ),
        entity(
            "mid",
            "u",
            EntityKind::Function,
            "mid_fn",
            "pipeline::mid_fn",
            None,
            &mid_s,
            &pid,
        ),
        entity(
            "closure_body",
            "u",
            EntityKind::Body,
            "Closure",
            "mid::body@closure",
            Some("mid"),
            &closure_s,
            &pid,
        ),
        entity(
            "outer",
            "u",
            EntityKind::Function,
            "outer_handler",
            "pipeline::outer_handler",
            None,
            &outer_s,
            &pid,
        ),
        entity(
            "async_body",
            "u",
            EntityKind::Body,
            "AsyncBlock",
            "outer::body@async",
            Some("outer"),
            &async_s,
            &pid,
        ),
        entity(
            "router",
            "u",
            EntityKind::Function,
            "entry_router",
            "pipeline::entry_router",
            None,
            &router_s,
            &pid,
        ),
    ];
    s.bodies = vec![
        Body {
            id: "closure_body".into(),
            unit: "u".into(),
            owner: "mid".into(),
            parent: None,
            kind: BodyKind::Closure,
            source: closure_s.clone(),
            evaluation: Evaluation::Deferred,
        },
        Body {
            id: "async_body".into(),
            unit: "u".into(),
            owner: "outer".into(),
            parent: None,
            kind: BodyKind::AsyncBlock,
            source: async_s.clone(),
            evaluation: Evaluation::Deferred,
        },
    ];
    s.occurrences = vec![
        Occurrence {
            id: "occ-leaf-call".into(),
            unit: "u".into(),
            owner: Some("closure_body".into()),
            source: leaf_call_s.clone(),
            locations: Default::default(),
            kind: RelationKind::Calls,
            declaration: Resolution::Resolved("leaf".into()),
            executable: Resolution::Resolved("leaf".into()),
            dispatch: Dispatch::Direct,
            evaluation: Evaluation::Deferred,
            arguments: vec![],
            argument_bindings: vec![],
            provenance: pid.clone(),
        },
        Occurrence {
            id: "occ-mid-call".into(),
            unit: "u".into(),
            owner: Some("async_body".into()),
            source: mid_call_s.clone(),
            locations: Default::default(),
            kind: RelationKind::Calls,
            declaration: Resolution::Resolved("mid".into()),
            executable: Resolution::Resolved("mid".into()),
            dispatch: Dispatch::Direct,
            evaluation: Evaluation::Deferred,
            arguments: vec![],
            argument_bindings: vec![],
            provenance: pid.clone(),
        },
        Occurrence {
            id: "occ-outer-call".into(),
            unit: "u".into(),
            owner: Some("router".into()),
            source: outer_call_s.clone(),
            locations: Default::default(),
            kind: RelationKind::Calls,
            declaration: Resolution::Resolved("outer".into()),
            executable: Resolution::Resolved("outer".into()),
            dispatch: Dispatch::Direct,
            evaluation: Evaluation::Immediate,
            arguments: vec![],
            argument_bindings: vec![],
            provenance: pid,
        },
    ];

    s.add_relation(
        "mid",
        "closure_body",
        RelationKind::CreatesBody,
        Some(closure_s),
        None,
        p.clone(),
    );
    s.add_relation(
        "closure_body",
        "leaf",
        RelationKind::Calls,
        Some(leaf_call_s),
        Some("occ-leaf-call".into()),
        p.clone(),
    );
    s.add_relation(
        "outer",
        "async_body",
        RelationKind::CreatesBody,
        Some(async_s),
        None,
        p.clone(),
    );
    s.add_relation(
        "async_body",
        "mid",
        RelationKind::Calls,
        Some(mid_call_s),
        Some("occ-mid-call".into()),
        p.clone(),
    );
    s.add_relation(
        "router",
        "outer",
        RelationKind::Calls,
        Some(outer_call_s),
        Some("occ-outer-call".into()),
        p,
    );
    s.seal().unwrap();
    s
}

fn entity_ids_from_walk(page: &QueryPage) -> HashSet<String> {
    page.items
        .iter()
        .filter_map(|v| v["entity"]["id"].as_str().map(str::to_owned))
        .collect()
}

fn run_both(
    store: &LocalStore,
    snapshot: &SemanticSnapshot,
    revision: &str,
    req: QueryRequest,
) -> (QueryPage, QueryPage) {
    let mem = QueryEngine::new(snapshot, &store.content)
        .execute(&req, None)
        .unwrap()
        .page;
    let reader = store.open_facts(&snapshot.project, Some(revision)).unwrap();
    let paged = paged::Engine {
        reader: &reader,
        model: None,
        control: None,
    }
    .execute(&req, None)
    .unwrap()
    .page;
    (mem, paged)
}

#[test]
fn closure_and_async_block_traversal_in_both_engines() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&dir.path().join("store")).unwrap();
    let root = dir.path().join("repo");
    let snapshot = nested_body_fixture(&store, &root);
    let rev = store
        .publish_paged_semantics(&snapshot, None)
        .unwrap()
        .revision;

    // 1. impact depth=1 from leaf_fn must ascend through closure_body to mid_fn.
    let (mem_d1, paged_d1) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "impact".into(),
            subject: Some("leaf".into()),
            depth: 1,
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_d1), ("paged", &paged_d1)] {
        let ids = entity_ids_from_walk(page);
        assert!(
            ids.contains("closure_body") && ids.contains("mid"),
            "{label} impact depth=1 should cross Closure to mid_fn; got {ids:?}"
        );
        assert!(
            !ids.contains("outer"),
            "{label} impact depth=1 should not overshoot to outer; got {ids:?}"
        );
    }

    // 2. impact depth=3 from leaf_fn must traverse Closure -> mid -> AsyncBlock -> outer -> router.
    let (mem_d3, paged_d3) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "impact".into(),
            subject: Some("leaf".into()),
            depth: 3,
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_d3), ("paged", &paged_d3)] {
        let ids = entity_ids_from_walk(page);
        for expected in [
            "leaf",
            "closure_body",
            "mid",
            "async_body",
            "outer",
            "router",
        ] {
            assert!(
                ids.contains(expected),
                "{label} impact depth=3 missing {expected}; got {ids:?}"
            );
        }
    }

    // 3. outbound trace from router must descend through AsyncBlock and Closure to leaf.
    let (mem_out, paged_out) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "trace".into(),
            subject: Some("router".into()),
            direction: "outbound".into(),
            depth: 3,
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_out), ("paged", &paged_out)] {
        let ids = entity_ids_from_walk(page);
        for expected in [
            "router",
            "outer",
            "async_body",
            "mid",
            "closure_body",
            "leaf",
        ] {
            assert!(
                ids.contains(expected),
                "{label} outbound trace missing {expected}; got {ids:?}"
            );
        }
    }

    // 4. path from router to leaf must find a path across both nested bodies.
    let (mem_path, paged_path) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "path".into(),
            subject: Some("router".into()),
            target: Some("leaf".into()),
            depth: 4,
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_path), ("paged", &paged_path)] {
        assert_eq!(page.state, "results", "{label} path should resolve");
        assert_eq!(page.items.len(), 1, "{label} path should return 1 path");
    }
}

#[test]
fn compact_payloads_and_readable_references_in_both_engines() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&dir.path().join("store")).unwrap();
    let root = dir.path().join("repo");
    let snapshot = nested_body_fixture(&store, &root);
    let rev = store
        .publish_paged_semantics(&snapshot, None)
        .unwrap()
        .revision;

    // Compare compact vs full references on leaf_fn.
    let (mem_c, paged_c) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "references".into(),
            subject: Some("leaf".into()),
            compact: true,
            ..Default::default()
        },
    );
    let (mem_f, paged_f) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "references".into(),
            subject: Some("leaf".into()),
            compact: false,
            ..Default::default()
        },
    );

    for (label, compact, full) in [("memory", &mem_c, &mem_f), ("paged", &paged_c, &paged_f)] {
        assert!(
            compact.provenance_records.is_empty(),
            "{label} compact mode should omit provenance_records"
        );
        assert!(
            !full.provenance_records.is_empty(),
            "{label} non-compact mode should preserve provenance_records"
        );
        let compact_bytes = serde_json::to_vec(&compact.to_compact_json())
            .unwrap()
            .len();
        let full_bytes = serde_json::to_vec(full).unwrap().len();
        assert!(
            compact_bytes < full_bytes,
            "{label} compact payload ({compact_bytes}B) should be smaller than full ({full_bytes}B)"
        );

        let item = &compact.items[0];
        assert_eq!(
            item["file"], "src/pipeline.rs",
            "{label} should emit relative file path"
        );
        assert_eq!(item["line"], 3, "{label} should emit 1-based line number");
        assert!(
            item["column"].as_u64().is_some(),
            "{label} should emit column"
        );
        assert_eq!(
            item["owner"], "pipeline::mid_fn",
            "{label} should resolve enclosing function owner across Closure"
        );
        assert!(
            item["snippet"]
                .as_str()
                .is_some_and(|s| s.contains("leaf_fn()")),
            "{label} should include readable line snippet; got {item}"
        );
        assert!(
            item.get("source").is_none(),
            "{label} compact reference should omit raw byte-span source object"
        );
    }

    // Compare compact vs full search payloads.
    let (mem_sc, paged_sc) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "search".into(),
            text: "fn".into(),
            compact: true,
            ..Default::default()
        },
    );
    let (mem_sf, paged_sf) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "search".into(),
            text: "fn".into(),
            compact: false,
            ..Default::default()
        },
    );
    for (label, compact, full) in [
        ("memory", &mem_sc, &mem_sf),
        ("paged", &paged_sc, &paged_sf),
    ] {
        let cb = serde_json::to_vec(&compact.to_compact_json())
            .unwrap()
            .len();
        let fb = serde_json::to_vec(full).unwrap().len();
        assert!(
            cb < fb,
            "{label} compact search ({cb}B) should be smaller than full ({fb}B)"
        );
        let first: &Value = &compact.items[0];
        assert!(
            first["file"]
                .as_str()
                .is_some_and(|p| p == "src/pipeline.rs")
        );
        assert!(first["line"].as_u64().is_some());
        assert!(first.get("provenance").is_none());
        assert!(first.get("source").is_none());
    }
}

fn find_ts_module() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CQ_TS_MODULE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let output = std::process::Command::new("node")
        .args([
            "-e",
            "console.log(require('path').dirname(require.resolve('typescript/package.json')))",
        ])
        .output()
        .ok()?;
    if output.status.success() {
        let resolved = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if resolved.exists() {
            return Some(resolved);
        }
    }
    None
}

#[test]
fn typescript_tsx_indexing_and_jsx_closure_impact_in_both_engines() {
    let Some(ts_module) = find_ts_module() else {
        eprintln!("skipping TS test: no local typescript module found");
        return;
    };
    let provider = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../providers/ts-compiler/index.mjs")
        .canonicalize()
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(&dir.path().join("store")).unwrap();
    let repo = dir.path().join("polyglot");
    let src = repo.join("frontend/src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        repo.join("frontend/tsconfig.json"),
        r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "jsx": "preserve",
    "strict": false,
    "noEmit": true
  },
  "include": ["src"]
}
"#,
    )
    .unwrap();

    // Include UTF-8 multi-byte CJK comments to exercise UTF-16 -> UTF-8 offset conversion.
    std::fs::write(
        src.join("api.ts"),
        concat!(
            "// 中文注释：测试 UTF-8 字节偏移与符号解析 🚀\n",
            "export interface PolicyConfig {\n",
            "  enabled: boolean;\n",
            "  limit: number;\n",
            "}\n",
            "export async function savePolicy(cfg: PolicyConfig): Promise<void> {\n",
            "  void cfg;\n",
            "}\n",
        ),
    )
    .unwrap();

    std::fs::write(
        src.join("settings.tsx"),
        concat!(
            "// 前端设置组件\n",
            "import { savePolicy, type PolicyConfig } from './api';\n",
            "export function SettingsPanel(props: { initial: PolicyConfig }) {\n",
            "  const onSave = async (next: PolicyConfig) => {\n",
            "    await savePolicy(next);\n",
            "  };\n",
            "  return <button onClick={() => onSave(props.initial)}>保存</button>;\n",
            "}\n",
            "export function App(props: { cfg: PolicyConfig }) {\n",
            "  return <SettingsPanel initial={props.cfg} />;\n",
            "}\n",
        ),
    )
    .unwrap();

    let mut snapshot = SemanticSnapshot::empty("ts-indexing-test".into(), NativeValue::new(&repo));
    let stats = cq_core::analyst::ts::analyze_in_memory(
        &repo,
        &repo,
        &provider,
        Some(&ts_module),
        &mut snapshot,
        &store.content,
    )
    .unwrap();
    assert_eq!(stats.tsconfigs, 1);
    assert_eq!(stats.files, 2);
    assert!(stats.entities >= 6);

    snapshot.seal().unwrap();
    let rev = store
        .publish_paged_semantics(&snapshot, None)
        .unwrap()
        .revision;

    // 1. Outline on TSX file lists exported components cleanly.
    let (mem_outline, paged_outline) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "outline".into(),
            file: Some("frontend/src/settings.tsx".into()),
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_outline), ("paged", &paged_outline)] {
        let names: HashSet<_> = page
            .items
            .iter()
            .filter_map(|v| v["name"].as_str())
            .collect();
        assert!(
            names.contains("SettingsPanel") && names.contains("App"),
            "{label} TSX outline missing components; got {names:?}"
        );
    }

    // 2. References on PolicyConfig across api.ts and settings.tsx with readable line & snippet.
    let (mem_refs, paged_refs) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "references".into(),
            subject: Some("frontend/src/api::PolicyConfig".into()),
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_refs), ("paged", &paged_refs)] {
        assert!(
            page.items
                .iter()
                .any(|v| v["file"] == "frontend/src/settings.tsx" && v["line"].as_u64().is_some()),
            "{label} references on PolicyConfig should include frontend/src/settings.tsx; got {:?}",
            page.items
        );
    }

    // 3. Impact on savePolicy crosses nested function/closure -> SettingsPanel -> JSX <SettingsPanel /> -> App!
    let (mem_imp, paged_imp) = run_both(
        &store,
        &snapshot,
        &rev,
        QueryRequest {
            operation: "impact".into(),
            subject: Some("frontend/src/api::savePolicy".into()),
            depth: 4,
            compact: true,
            ..Default::default()
        },
    );
    for (label, page) in [("memory", &mem_imp), ("paged", &paged_imp)] {
        let names: HashSet<_> = page
            .items
            .iter()
            .filter_map(|v| v["entity"]["name"].as_str())
            .collect();
        assert!(
            names.contains("savePolicy")
                && names.contains("SettingsPanel")
                && names.contains("App"),
            "{label} TS impact should traverse savePolicy -> SettingsPanel -> App; got {names:?}"
        );
    }
}
