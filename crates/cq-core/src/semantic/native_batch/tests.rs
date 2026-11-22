use super::*;

#[test]
fn cached_dependencies_preserve_transitive_native_closure_and_variants() {
    let files = (0..11)
        .map(|i| json!({"id":i,"path":format!("/repo/{i}.rs"),"text":"x"}))
        .collect::<Vec<_>>();
    let mut symbols = (0..11)
        .map(|i| {
            json!({"token":i,"key":format!("s{i}"),"kind":"Function",
        "definition":{"file":i,"start":0,"end":1}})
        })
        .collect::<Vec<_>>();
    symbols[9]["key"] = json!("s3");
    symbols[10]["definition"]["file"] = json!(6);
    symbols[4]["type_definitions"] = json!([{"source":{"file":5,"start":0,"end":1}}]);
    symbols[6]["type_definitions"] = json!([{"source":{"file":1,"start":0,"end":1}}]);
    symbols.push(json!({"token":11,"key":"unowned","kind":"Type",
        "type_definitions":[{"source":{"file":4,"start":0,"end":1}}]}));
    let raw = json!({"protocol":"codequest-hir-export","schema_version":1,"files":files,"symbols":symbols,
        "references":[{"source":{"file":0,"start":0,"end":1},"target":1}],
        "parents":[{"child":1,"parent_token":2},{"child":2,"key":"s3","kind":"Function"}],
        "nominal_members":[{"child":3,"owner":4}],
        "parameters":[{"owner":{"file":5,"start":0,"end":1},"slots":[{"bindings":[{"file":6,"start":0,"end":1}]}]}],
        "doc_links":[{"source":{"file":0,"start":0,"end":1},"targets":[{"file":7,"start":0,"end":1}]}],
        "relations":[],"bodies":[],"calls":[]});
    super::super::provider_contract::validate(&raw).unwrap();
    let catalogue = Catalogue::new(&raw).unwrap();
    let frame = catalogue.frame(Some(0)).unwrap();
    let tokens = |v: &Value| {
        rows(v, "symbols")
            .iter()
            .map(|s| s["token"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(tokens(&frame), vec![0, 1, 2, 3, 4, 5, 6, 7, 9, 10]);
    assert_eq!(rows(&frame, "parameters").len(), 1);
    assert_eq!(rows(&frame, "parents").len(), 2);
    assert_eq!(rows(&frame, "nominal_members").len(), 1);
    assert_eq!(
        tokens(&catalogue.frame(None).unwrap()),
        vec![1, 2, 3, 4, 5, 6, 9, 10, 11]
    );
    assert!(!catalogue.dependencies.contains_key(&8));
}
