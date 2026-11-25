use cq_core::{model::*, ports::ContentRepository};
use cq_store::LocalStore;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

#[test]
fn pipelined_navigation_keeps_source_receipts_and_reset_in_request_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let store = LocalStore::open(&root).unwrap();
    let file = temp.path().join("lib.rs");
    std::fs::write(&file, "pub fn leaf() {}\npub fn caller() { leaf(); }\n").unwrap();
    let frozen = store.content.freeze(&file, "fixture", 1024).unwrap();
    let mut snapshot = SemanticSnapshot::empty("mcp-stdio".into(), NativeValue::new(temp.path()));
    let provenance = snapshot.intern_provenance(Provenance {
        provider: "stdio-contract".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: "controlled immutable facts".into(),
    });
    snapshot.sources = SourceSnapshot::seal(
        vec![frozen.clone()],
        InputBinding::ImmutableScopeVerified,
        vec![],
    );
    for (id, start, end) in [("leaf", 0, 17), ("caller", 17, frozen.blob.len)] {
        let anchor = SourceAnchor {
            file_revision: frozen.id.clone(),
            start,
            end,
        };
        snapshot.entities.push(Entity {
            id: id.into(),
            unit: "unit".into(),
            provider_id: id.into(),
            kind: EntityKind::Function,
            name: id.into(),
            qualified_name: format!("demo::{id}"),
            signature: "fn()".into(),
            source: Some(anchor.clone()),
            extent: Some(anchor),
            parent: None,
            external: false,
            exported: true,
            attributes: BTreeMap::new(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: provenance.clone(),
            stable_handle: None,
            locations: Default::default(),
        });
    }
    snapshot.seal().unwrap();
    store.publish_paged_semantics(&snapshot, None).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cq"))
        .args(["mcp", "--store"])
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let calls = [
        ("search_graph", json!({"text":"leaf","evidence":false})),
        ("search_graph", json!({"text":"caller","evidence":false})),
        (
            "get_code_snippet",
            json!({"file":"lib.rs","line":1,"max_lines":2}),
        ),
        (
            "get_code_snippet",
            json!({"file":"lib.rs","line":1,"max_lines":2}),
        ),
        ("query_codequest", json!({"operation":"reset_evidence"})),
        (
            "get_code_snippet",
            json!({"file":"lib.rs","line":1,"max_lines":2}),
        ),
    ];
    for (i, (name, mut arguments)) in calls.into_iter().enumerate() {
        arguments["project"] = json!("mcp-stdio");
        writeln!(input, "{}", json!({"jsonrpc":"2.0","id":i,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap();
    }
    input.flush().unwrap();
    drop(input);
    let mut order = vec![];
    let mut responses = BTreeMap::new();
    for _ in 0..6 {
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_ne!(response["result"]["isError"], true, "{response}");
        let id = response["id"].as_u64().unwrap();
        order.push(id);
        assert!(
            responses
                .insert(
                    id,
                    serde_json::from_str::<Value>(
                        response["result"]["content"][0]["text"].as_str().unwrap()
                    )
                    .unwrap()
                )
                .is_none()
        );
    }
    assert_eq!(&order[2..], &[2, 3, 4, 5]);
    assert_eq!(responses[&2]["source_delivery"]["new_lines"], 2);
    assert_eq!(responses[&3]["source_delivery"]["reused_lines"], 2);
    assert_eq!(responses[&3]["receipt_hit"], true);
    assert_eq!(responses[&5]["source_delivery"]["new_lines"], 2);
    assert!(child.wait().unwrap().success());
}
