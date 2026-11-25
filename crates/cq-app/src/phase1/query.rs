use super::Options;
use cq_core::{Error, Result, query::QueryRequest};
use serde_json::{Value, json};
pub fn arguments(options: &Options) -> Result<Value> {
    if let Some(text) = options.text("--json") {
        return Ok(serde_json::from_str(&text)?);
    }
    if let Some(path) = options.path("--request") {
        return Ok(serde_json::from_slice(&std::fs::read(path)?)?);
    }
    let mut value = serde_json::Map::new();
    for (flag, key) in [
        ("--subject", "subject"),
        ("--from", "subject"),
        ("--to", "target"),
        ("--target", "target"),
        ("--base", "base"),
        ("--operation", "operation"),
        ("--authority", "authority"),
        ("--mode", "mode"),
        ("--search-view", "search_view"),
        ("--text", "text"),
        ("--file", "file"),
        ("--directory", "directory"),
        ("--configuration-view", "configuration_view"),
        ("--file-revision", "file_revision"),
        ("--context", "context"),
        ("--kind", "kind"),
        ("--document-kind", "document_kind"),
        ("--direction", "direction"),
        ("--relation", "relation"),
        ("--cursor", "cursor"),
        ("--id", "id"),
        ("--action", "action"),
        ("--author", "author"),
        ("--input", "input"),
    ] {
        if let Some(text) = options.text(flag) {
            value.insert(key.into(), json!(text));
        }
    }
    for (flag, key) in [
        ("--limit", "limit"),
        ("--line", "line"),
        ("--column", "column"),
        ("--max-lines", "max_lines"),
        ("--max-depth", "depth"),
        ("--depth", "depth"),
        ("--max-bytes", "max_bytes"),
        ("--max-work", "max_work"),
        ("--max-job-bytes", "max_job_bytes"),
        ("--offset", "offset"),
        ("--timeout-ms", "timeout_ms"),
    ] {
        if let Some(text) = options.text(flag) {
            value.insert(
                key.into(),
                json!(
                    text.parse::<u64>()
                        .map_err(|_| Error::new("InvalidArguments", flag))?
                ),
            );
        }
    }
    for (flag, key) in [
        ("--regex", "regex"),
        ("--include-candidates", "include_candidates"),
        ("--include-external", "include_external"),
        ("--exact-semantic", "exact_semantic"),
        ("--compact", "compact"),
    ] {
        if options.yes(flag) {
            value.insert(key.into(), json!(true));
        }
    }
    if !options.trailing.is_empty() {
        let text = options
            .trailing
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if ["search", "search_code", "semantic", "tour", "doc_search"]
            .contains(&options.command.as_str())
        {
            value.entry("text").or_insert(json!(text));
        } else {
            value.entry("subject").or_insert(json!(text));
        }
    }
    Ok(Value::Object(value))
}
pub fn request(operation: &str, args: Value) -> Result<QueryRequest> {
    let mut args = args;
    if !args.is_object() {
        return Err(Error::new(
            "InvalidArguments",
            "query arguments must be an object",
        ));
    }
    args["operation"] = json!(if operation == "query" {
        args["operation"].as_str().unwrap_or("overview").to_owned()
    } else if operation == "rustdoc" {
        "docs".into()
    } else {
        operation.into()
    });
    if operation == "rustdoc" {
        args["document_kind"] = json!("Rustdoc");
    }
    Ok(serde_json::from_value(args)?)
}

/// Leave room for cursor hashes and service diagnostics when packing groups.
/// Saved continuations retain the caller's budget, not this execution allowance.
pub fn execution_budget(q: &QueryRequest) -> Result<QueryRequest> {
    let mut run = q.clone();
    {
        // Two 64-byte cursor occurrences plus diagnostics fit these bounds.
        let envelope = if q.compact { 512 } else { 1024 };
        run.max_bytes = q
            .max_bytes
            .checked_sub(envelope)
            .filter(|n| *n >= 128)
            .ok_or_else(|| {
                Error::new(
                    "OutputBudgetTooSmall",
                    "grouped output needs room for cursor and diagnostics",
                )
            })?;
    }
    Ok(run)
}
