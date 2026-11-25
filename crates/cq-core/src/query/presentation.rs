use super::*;
use serde_json::json;

pub fn validate_scope(q: &QueryRequest) -> crate::Result<()> {
    if q.file.is_some() && q.directory.is_some() {
        return Err(crate::Error::new(
            "InvalidScope",
            "use file or directory, not both",
        ));
    }
    if !["summary", "full"].contains(&q.configuration_view.as_str()) {
        return Err(crate::Error::new(
            "InvalidConfigurationView",
            "choose summary or full",
        ));
    }
    if let Some(kind) = &q.kind
        && !kind.eq_ignore_ascii_case("Callable")
        && serde_json::from_value::<EntityKind>(json!(kind)).is_err()
        && ![
            "project",
            "package",
            "compilationunit",
            "module",
            "file",
            "function",
            "method",
            "struct",
            "enum",
            "variant",
            "union",
            "trait",
            "interface",
            "class",
            "type",
            "typealias",
            "field",
            "parameter",
            "variable",
            "constant",
            "macro",
            "impl",
            "body",
            "document",
            "docsection",
            "resource",
            "task",
            "route",
            "artifact",
            "symbol",
        ]
        .contains(&kind.to_ascii_lowercase().as_str())
    {
        return Err(crate::Error::new(
            "InvalidKind",
            "use an entity kind, or Callable for functions and methods",
        ));
    }
    Ok(())
}

pub fn symbol_context(unit: &BuildUnit) -> Value {
    let summary = unit_summary(unit);
    let mut out =
        json!({"crate":summary["name"],"target":summary["target"],"role":summary["role"]});
    for key in ["test", "features", "cfg"] {
        if let Some(v) = summary.get(key) {
            out[key] = v.clone();
        }
    }
    out
}

pub fn unit_summary(unit: &BuildUnit) -> Value {
    let mut out = json!({"id":unit.id,"name":unit.name.value(),"language":unit.language,
        "role":unit.role,"target":unit.target.value(),"host":unit.host.value()});
    if let Some(rust) = unit.configuration.rust() {
        let cfg = rust.compiler_cfg.value().unwrap_or(&rust.explicit_cfg);
        out["features"] = json!(
            cfg.iter()
                .filter_map(|c| c
                    .strip_prefix("feature=\"")
                    .and_then(|s| s.strip_suffix('"')))
                .collect::<Vec<_>>()
        );
        out["test"] = json!(rust.test);
        out["edition"] = json!(rust.edition.value());
        out["cfg"] = json!(rust.explicit_cfg);
    }
    out["continue_with"] =
        json!({"operation":"build_configuration","subject":unit.id,"configuration_view":"full"});
    out
}

pub fn kind_matches(kind: EntityKind, requested: &str) -> bool {
    kind.label().eq_ignore_ascii_case(requested)
        || requested.eq_ignore_ascii_case("Callable")
            && matches!(kind, EntityKind::Function | EntityKind::Method)
}

pub fn semantic_file_language(q: &QueryRequest, indexed: &[Language]) -> Option<Gap> {
    if !super::disclosure::compilation_absence(q, true, true) {
        return None;
    }
    let path = q.file.as_deref()?;
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    let (name, supported, present) = match ext.as_str() {
        "rs" => ("Rust", true, indexed.contains(&Language::Rust)),
        "go" => ("Go", true, indexed.contains(&Language::Go)),
        "c" => ("C", true, indexed.contains(&Language::C)),
        "cc" | "cpp" | "cxx" | "hpp" => ("Cpp", true, indexed.contains(&Language::Cpp)),
        "h" => (
            "C/Cpp",
            true,
            indexed.contains(&Language::C) || indexed.contains(&Language::Cpp),
        ),
        "ts" | "tsx" | "js" | "jsx" | "mjs" => {
            ("TypeScript", true, indexed.contains(&Language::TypeScript))
        }
        "py" => ("Python", false, false),
        "sh" => ("Shell", false, false),
        _ => return None,
    };
    if present || supported && indexed.is_empty() {
        return None;
    }
    Some(Gap::new(
        if supported {
            "LanguageNotIndexed"
        } else {
            "UnsupportedSemanticLanguage"
        },
        path,
        format!(
            "{name} semantic facts are unavailable for this file in this revision; use source/text tools. An empty graph lookup cannot establish absence."
        ),
    ))
}

pub fn unavailable_language_page(
    revision: &str,
    gap: Gap,
    q: &QueryRequest,
) -> crate::Result<QueryPage> {
    let mut page = QueryPage {
        ranking: None,
        provenance_records: Default::default(),
        revision: revision.into(),
        context: None,
        items: vec![],
        total: None,
        total_kind: "unknown".into(),
        truncated: false,
        next_cursor: None,
        state: if gap.code == "LanguageNotIndexed" {
            "language_not_indexed"
        } else {
            "unsupported_semantic_language"
        }
        .into(),
        coverage: vec![],
        gaps: vec![gap],
        hidden: vec![],
        work: 0,
        elapsed_ms: 0,
    };
    super::disclosure::apply(&mut page, q)?;
    Ok(page)
}
