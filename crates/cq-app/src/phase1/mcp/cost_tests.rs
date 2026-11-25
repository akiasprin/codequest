use super::cost::Session;
use super::*;
use cq_core::model::*;

#[test]
fn changes_budget_pages_one_sample_without_loss_or_resampling() {
    let (dir, s) = tests::sample_service();
    let repo = dir.path().join("repo");
    for i in 0..160 {
        std::fs::write(repo.join(format!("new-{i:03}-資料.rs")), "// added\n").unwrap();
    }
    let mut session = Session::default();
    let mut page = session
        .call(
            &s,
            "query_codequest",
            json!({"project":"mcp-demo","operation":"changes","compact":true,"max_bytes":1024}),
            true,
        )
        .unwrap();
    let counts = page["counts"].clone();
    let expected: usize = counts
        .as_object()
        .unwrap()
        .values()
        .map(|n| n.as_u64().unwrap() as usize)
        .sum();
    assert!(expected >= 160);
    std::fs::write(
        repo.join("after-sample.rs"),
        "// must not enter an existing cursor\n",
    )
    .unwrap();
    let mut paths = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        assert!(serde_json::to_vec(&page).unwrap().len() <= 1024);
        assert_eq!(page["counts"], counts);
        for key in ["added", "modified", "removed"] {
            for path in page[key].as_array().unwrap() {
                assert!(paths.insert(path.as_str().unwrap().to_owned()));
            }
        }
        pages += 1;
        let Some(cursor) = page["next_cursor"].as_str() else {
            break;
        };
        assert_eq!(page["truncated"], true);
        page = session
            .call(
                &s,
                "query_codequest",
                json!({"project":"mcp-demo","operation":"next","cursor":cursor,"max_bytes":1024}),
                true,
            )
            .unwrap();
        assert!(pages < 200);
    }
    assert!(pages > 1);
    assert_eq!(paths.len(), expected);
    assert!(paths.iter().all(|p| !p.ends_with("after-sample.rs")));
    assert!(
        session
            .call(
                &s,
                "query_codequest",
                json!({"project":"mcp-demo","operation":"status","max_bytes":128}),
                true
            )
            .is_err()
    );
    let revision = s.store.head("mcp-demo").unwrap().unwrap();
    let comparison = session
        .call(
            &s,
            "query_codequest",
            json!({"project":"mcp-demo","operation":"compare","base":revision,"max_bytes":1024}),
            true,
        )
        .unwrap();
    assert!(serde_json::to_vec(&comparison).unwrap().len() <= 1024);
    assert!(comparison["modified_files"].as_array().unwrap().is_empty());
}

fn variant_unit(id: &str, target: &str, test: bool) -> BuildUnit {
    BuildUnit {
        id: id.into(),
        language: Language::Rust,
        name: Observed::Value("demo".into()),
        root: Observed::Unknown("fixture".into()),
        output_kinds: vec!["bin".into()],
        role: if test {
            UnitRole::Test
        } else {
            UnitRole::Binary
        },
        host: Observed::Value("x86_64-unknown-linux-gnu".into()),
        target: Observed::Value(target.into()),
        configuration: ConfigurationObservation::Rust(RustConfiguration {
            explicit_cfg: vec!["feature=\"cvm\"".into()],
            expected_cfg: vec![],
            compiler_cfg: Observed::Value(vec!["feature=\"cvm\"".into()]),
            compiler_cfg_evidence: None,
            test,
            edition: Observed::Value("2024".into()),
            options: vec![NativeValue::from("very-long-compiler-option"); 1000],
            coverage: ConfigurationCoverage::ExplicitAndCompilerCfg,
            gaps: vec![],
        }),
        invocation: Observed::Unknown("fixture".into()),
        artifacts: vec![],
        binding: UnitBinding::ObservedInvocation,
        evidence: vec![],
    }
}

#[test]
fn navigation_exposes_variant_identity_and_summarizes_configuration() {
    let (_dir, service) = tests::sample_service();
    let mut snapshot = service.snapshot("mcp-demo", None).unwrap().as_ref().clone();
    snapshot.previous = Some(snapshot.revision.clone());
    let template = snapshot.entities[0].clone();
    snapshot.entities.clear();
    snapshot.relations.clear();
    snapshot.occurrences.clear();
    snapshot.bodies.clear();
    snapshot.units = vec![
        variant_unit("arm", "aarch64-unknown-linux-musl", false),
        variant_unit("x86", "x86_64-unknown-linux-gnu", true),
    ];
    snapshot.contexts = snapshot
        .units
        .iter()
        .map(|u| AnalysisContext {
            id: format!("ctx-{}", u.id),
            entry_unit: u.id.clone(),
            units: vec![u.id.clone()],
            status: Support::Supported,
            gaps: vec![],
        })
        .collect();
    for unit in ["arm", "x86"] {
        let mut e = template.clone();
        e.id = format!("method-{unit}");
        e.unit = unit.into();
        e.provider_id = e.id.clone();
        e.kind = EntityKind::Method;
        e.name = "reconcile".into();
        e.qualified_name = "Inner::reconcile".into();
        snapshot.entities.push(e);
    }
    snapshot.seal().unwrap();
    service
        .store
        .publish_paged_semantics(&snapshot, snapshot.previous.as_deref())
        .unwrap();
    let mut session = Session::default();
    let page = session.call(&service,"search_graph",json!({"project":"mcp-demo","text":"reconcile","kind":"Callable","file":"lib.rs","evidence":false,"max_bytes":4500}),true).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let variants: std::collections::BTreeSet<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["build_context"]["target"].as_str().unwrap())
        .collect();
    assert_eq!(variants.len(), 2);
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["unit"].is_string())
    );
    let selected = session.call(&service,"search_graph",json!({"project":"mcp-demo","text":"reconcile","context":"x86","kind":"Callable","evidence":false}),true).unwrap();
    assert_eq!(selected["items"].as_array().unwrap().len(), 1);
    assert_eq!(selected["items"][0]["build_context"]["test"], true);
    let configuration = session.call(&service,"query_codequest",json!({"project":"mcp-demo","operation":"build_configuration","subject":"method-arm","max_bytes":1500}),true).unwrap();
    assert_eq!(
        configuration["items"][0]["target"],
        "aarch64-unknown-linux-musl"
    );
    assert_eq!(configuration["items"][0]["features"], json!(["cvm"]));
    assert!(configuration["items"][0]["continue_with"].is_object());
    assert!(serde_json::to_vec(&configuration).unwrap().len() <= 1500);
    let full = session.call(&service,"query_codequest",json!({"project":"mcp-demo","operation":"build_configuration","subject":"method-arm","configuration_view":"full","max_bytes":262144}),true).unwrap();
    assert_eq!(
        full["items"][0]["unit"]["configuration"]["observations"]["options"]
            .as_array()
            .unwrap()
            .len(),
        1000
    );
    let filtered = session.call(&service,"search_graph",json!({"project":"mcp-demo","text":"reconcile","file":"lib.rs","kind":"Function","evidence":false}),true).unwrap();
    assert!(filtered["items"].as_array().unwrap().is_empty());
    assert!(
        filtered["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["code"] == "KindFilterExcludesExactName")
    );
}

#[test]
fn semantic_language_and_directory_misuse_are_explicit_without_broadening_filters() {
    let (_dir, s) = tests::sample_service();
    let mut session = Session::default();
    for (file, state) in [
        ("scripts/install.py", "unsupported_semantic_language"),
        ("components/managed.go", "language_not_indexed"),
    ] {
        // Declare the actual indexed language, as production catalogues do.
        let mut snap = s.snapshot("mcp-demo", None).unwrap().as_ref().clone();
        if snap.units.is_empty() {
            snap.units
                .push(variant_unit("u", "x86_64-unknown-linux-gnu", false));
            snap.previous = Some(snap.revision.clone());
            snap.seal().unwrap();
            s.store
                .publish_paged_semantics(&snap, snap.previous.as_deref())
                .unwrap();
        }
        let result=session.call(&s,"search_graph",json!({"project":"mcp-demo","text":"install","file":file,"evidence":false,"max_bytes":1500}),true).unwrap();
        assert_eq!(result["state"], state);
        assert_eq!(result["total_kind"], "unknown");
    }
    let wrong = session
        .call(
            &s,
            "search_graph",
            json!({"project":"mcp-demo","text":"leaf","file":"repo","evidence":false}),
            true,
        )
        .unwrap_err();
    assert_eq!(wrong.code(), "DirectoryPassedAsFile");
    let subtree = session
        .call(
            &s,
            "search_graph",
            json!({"project":"mcp-demo","text":"leaf","directory":"repo","evidence":false}),
            true,
        )
        .unwrap();
    assert!(!subtree["items"].as_array().unwrap().is_empty());
    assert!(session.call(&s,"search_graph",json!({"project":"mcp-demo","text":"leaf","file":"lib.rs","directory":"repo","evidence":false}),true).is_err());
}

#[test]
fn source_overlap_receipts_force_repeat_and_connection_reset() {
    let (_dir, s) = tests::sample_service();
    let mut session = Session::default();
    assert!(
        session
            .call(&s, "query_codequest", json!([]), true)
            .is_err()
    );
    let first = session
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":2}),
            true,
        )
        .unwrap();
    assert_eq!(first["source_delivery"]["new_lines"], 2);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert!(
        first["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("caller")
    );
    assert!(first["items"][0]["source"]["file_revision"].is_string());
    let again = session
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":1}),
            true,
        )
        .unwrap();
    assert_eq!(again["receipt_hit"], true);
    assert_eq!(again["source_delivery"]["reused_lines"], 1);
    let forced=session.call(&s,"get_code_snippet",json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":1,"force_repeat":true}),true).unwrap();
    assert_eq!(forced["source_delivery"]["new_lines"], 1);
    let other = Session::default()
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":1}),
            true,
        )
        .unwrap();
    assert_eq!(other["source_delivery"]["new_lines"], 1);
    session
        .call(
            &s,
            "query_codequest",
            json!({"operation":"reset_evidence"}),
            true,
        )
        .unwrap();
    let reset = session
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":1}),
            true,
        )
        .unwrap();
    assert_eq!(reset["source_delivery"]["new_lines"], 1);
}

#[test]
fn batch_pins_revision_reports_errors_and_preserves_source_coverage() {
    let (_dir, s) = tests::sample_service();
    let mut session = Session::default();
    let before = s.store.head("mcp-demo").unwrap().unwrap();
    let batch=session.call(&s,"query_codequest",json!({"project":"mcp-demo","evidence":false,"queries":[
        {"operation":"definition","subject":"leaf"},{"operation":"delete"},
        {"operation":"definition","subject":"missing"},{"operation":"definition","subject":"caller","revision":"wrong"}
    ]}),true).unwrap();
    assert_eq!(batch["revision"], before);
    assert_eq!(batch["items"][0]["status"], "ok");
    assert!(
        batch["items"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .all(|r| r["status"] == "error")
    );
    assert_eq!(s.store.head("mcp-demo").unwrap().unwrap(), before);
    let source = Session::default()
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","ranges":[
                {"file":"lib.rs","line":1,"max_lines":1},{"file":"lib.rs","line":1,"max_lines":2}
            ]}),
            true,
        )
        .unwrap();
    assert_eq!(source["state"], "batch_complete");
    assert_eq!(
        source["items"][0]["result"]["source_delivery"]["new_lines"],
        1
    );
    assert_eq!(
        source["items"][1]["result"]["source_delivery"]["new_lines"],
        1
    );
    assert_eq!(
        source["items"][1]["result"]["source_delivery"]["reused_lines"],
        1
    );
    assert!(session.call(&s,"query_codequest",json!({"project":"mcp-demo","queries":vec![json!({"operation":"definition","subject":"leaf"});9]}),true).is_err());
}

#[test]
fn navigation_attaches_bounded_frozen_evidence_and_failed_delivery_does_not_consume_it() {
    let (_dir, s) = tests::sample_service();
    let mut session = Session::default();
    let page = session
        .call(
            &s,
            "query_codequest",
            json!({"project":"mcp-demo","operation":"callers","subject":"leaf"}),
            true,
        )
        .unwrap();
    assert!(page["evidence"].as_array().is_some_and(|v| !v.is_empty()));
    let e = &page["evidence"][0];
    assert_eq!(e["complete_control_flow"], false);
    assert!(e["items"][0]["text"].as_str().unwrap().contains("leaf"));
    assert_eq!(e["revision"], page["revision"]);
    let mut session = Session::default();
    assert!(session.call(&s,"get_code_snippet",json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":1,"max_bytes":128}),true).is_err());
    let page = session
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":1}),
            true,
        )
        .unwrap();
    assert_eq!(page["source_delivery"]["new_lines"], 1);
}

#[test]
fn tool_schemas_are_specific_and_legacy_arguments_remain_accepted() {
    let tools = super::tools();
    let source = tools
        .iter()
        .find(|t| t["name"] == "get_code_snippet")
        .unwrap();
    assert!(source["inputSchema"]["properties"].get("ranges").is_some());
    assert!(source["inputSchema"]["properties"].get("depth").is_none());
    let index = tools
        .iter()
        .find(|t| t["name"] == "index_repository")
        .unwrap();
    assert!(index["inputSchema"]["properties"].get("root").is_some());
    assert!(
        index["inputSchema"]["properties"]
            .get("force_repeat")
            .is_none()
    );
    assert_eq!(index["inputSchema"]["additionalProperties"], true);
}

#[test]
fn over_budget_batch_is_atomic_and_explicit_mutations_keep_their_original_contract() {
    let (_dir, s) = tests::sample_service();
    let mut session = Session::default();
    let entries = vec![
        json!({"operation":"source","file":"lib.rs","line":1,"max_lines":2,"note":"x".repeat(1500)});
        8
    ];
    assert!(
        session
            .call(
                &s,
                "query_codequest",
                json!({"project":"mcp-demo","max_bytes":2048,"queries":entries}),
                true
            )
            .is_err()
    );
    let read = session
        .call(
            &s,
            "get_code_snippet",
            json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":2}),
            true,
        )
        .unwrap();
    assert_eq!(
        read["source_delivery"]["new_lines"], 2,
        "undelivered batches cannot consume receipt state"
    );
    let deleted = session
        .call(
            &s,
            "delete_project",
            json!({"project":"mcp-demo","max_bytes":1}),
            true,
        )
        .unwrap();
    assert_eq!(
        deleted["state"], "index_deleted",
        "a write must not succeed and then be hidden by a presentation budget error"
    );
}

#[test]
fn source_continuations_keep_the_requested_view_and_receipts() {
    let (_dir, s) = tests::sample_service();
    for view in ["blocks", "lines"] {
        let mut session = Session::default();
        let first=session.call(&s,"get_code_snippet",json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":2,"limit":1,"source_view":view}),true).unwrap();
        let cursor = first["next_cursor"].as_str().unwrap();
        let next = session
            .call(
                &s,
                "query_codequest",
                json!({"project":"mcp-demo","operation":"next","cursor":cursor}),
                true,
            )
            .unwrap();
        assert_eq!(next["items"][0]["line"], 2);
        assert!(
            next["items"][0]["text"]
                .as_str()
                .unwrap()
                .contains("caller")
        );
        if view == "blocks" {
            assert_eq!(next["source_delivery"]["new_lines"], 1);
            let reused = session
                .call(
                    &s,
                    "get_code_snippet",
                    json!({"project":"mcp-demo","file":"lib.rs","line":1,"max_lines":24}),
                    true,
                )
                .unwrap();
            assert_eq!(reused["receipt_hit"], true);
            assert_eq!(reused["source_delivery"]["reused_lines"], 2);
        } else {
            assert!(next.get("source_delivery").is_none());
        }
    }
}
