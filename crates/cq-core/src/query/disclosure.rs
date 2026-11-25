//! Presentation over an already bounded result page; it never changes graph facts.
use super::*;
use crate::{Error, Result};
use serde_json::json;
use std::collections::HashMap;

pub(super) fn reports_coverage(q: &QueryRequest) -> bool {
    matches!(q.operation.as_str(), "coverage" | "capabilities")
}

/// Only an exhausted semantic lookup needs the compilation caveat. Positive
/// matches and frozen text reads do not inherit unrelated project diagnostics.
pub(super) fn compilation_absence(q: &QueryRequest, empty: bool, complete: bool) -> bool {
    empty
        && complete
        && matches!(
            q.operation.as_str(),
            "search"
                | "semantic"
                | "entrypoints"
                | "definition"
                | "type_definition"
                | "references"
                | "callers"
                | "callees"
                | "implementations"
                | "relations"
                | "members"
                | "bodies"
                | "trace"
                | "impact"
                | "path"
                | "at"
        )
}

pub(super) fn is_overview(q: &QueryRequest) -> bool {
    q.operation == "search" && q.search_view == SearchView::Overview
}

pub(super) fn visible(e: &Entity, q: &QueryRequest) -> bool {
    !is_overview(q)
        || q.kind.is_some()
        || !matches!(
            e.kind,
            EntityKind::Variable | EntityKind::Parameter | EntityKind::Body
        )
}

/// Parent links are compiler facts. A missing module falls back to a source file,
/// explicitly labelled as such; a path is never promoted to a semantic module.
pub(super) fn module(
    e: &Entity,
    mut get: impl FnMut(&str) -> Result<Option<Entity>>,
) -> Result<Option<Value>> {
    let mut node = e.clone();
    let mut visited = std::collections::HashSet::new();
    for _ in 0..64 {
        if !visited.insert(node.id.clone()) {
            break;
        }
        if node.kind == EntityKind::Module {
            return Ok(Some(json!({"id":node.id,"name":node.qualified_name})));
        }
        let Some(parent) = node.parent.as_deref() else {
            break;
        };
        let Some(parent) = get(parent)? else {
            break;
        };
        node = parent;
    }
    Ok(None)
}

pub(super) fn apply(page: &mut QueryPage, q: &QueryRequest) -> Result<Vec<Value>> {
    let overview = is_overview(q);
    let all = std::mem::take(&mut page.items);
    let original_hidden = page.hidden.clone();
    let original_truncated = page.truncated;
    let mut take = all.len();
    loop {
        page.items = all[..take].to_vec();
        page.hidden = original_hidden.clone();
        page.truncated = original_truncated || take < all.len();
        if take < all.len() {
            if let Some(remaining) = page
                .hidden
                .iter_mut()
                .find(|h| h.category == "remaining_query_results")
            {
                remaining.count = remaining.count.map(|n| n + all.len() - take);
            } else {
                page.hidden.push(HiddenGroup {category:"remaining_query_results".into(),count:Some(all.len()-take),
                    reason:"grouped output byte budget; remaining declarations are queued for continuation".into(),
                    continue_with:json!({"operation":"next"})});
            }
        }
        if overview {
            render(page, q);
            page.ranking.as_mut().unwrap()["disclosure"]["byte_limited"] = json!(take < all.len());
        }
        let value = if q.compact {
            page.to_compact_json()
        } else {
            serde_json::to_value(&*page)?
        };
        if serde_json::to_vec(&value)?.len() <= q.max_bytes {
            return Ok(all[take..].to_vec());
        }
        if take <= 1 {
            return Err(Error::new(
                "OutputBudgetTooSmall",
                "one result and its metadata exceed max_bytes; narrow the result or raise max_bytes",
            ));
        }
        // A conservative reduction leaves room for the opaque cursor added by
        // the service. Deferred facts are returned to the engine, never dropped.
        take = (take * 2 / 3).max(1);
    }
}

fn render(page: &mut QueryPage, q: &QueryRequest) {
    let mut positions = HashMap::new();
    let mut groups: Vec<Value> = vec![];
    for mut item in std::mem::take(&mut page.items) {
        let module = item
            .as_object_mut()
            .unwrap()
            .remove("_search_module")
            .unwrap_or(Value::Null);
        let unit = item
            .as_object_mut()
            .unwrap()
            .remove("_search_unit")
            .unwrap_or(Value::Null);
        let file = item["file"].clone();
        // Distinct compilation instances remain distinct, including in presentation.
        let key = json!([unit, module["id"], file]).to_string();
        let i = *positions.entry(key).or_insert_with(|| {
            let i = groups.len();
            let mut request = expand(q);
            // A file is a scope hint, not proof that all symbols belong to one module.
            if file.is_string() {
                request["file"] = file.clone();
            }
            groups.push(
                json!({"kind":if module.is_null(){"FileMatches"}else{"ModuleMatches"},
                "module":module,"file":file,"unit":unit,"matches_in_page":0,
                "symbols":[],"collapsed_in_page":0,"continue_with":request}),
            );
            i
        });
        let group = &mut groups[i];
        group["matches_in_page"] = json!(group["matches_in_page"].as_u64().unwrap() + 1);
        let symbols = group["symbols"].as_array_mut().unwrap();
        if symbols.len() < 3 {
            // The enclosing group supplies the file; full evidence is available via id.
            item.as_object_mut().unwrap().remove("file");
            symbols.push(item);
        } else {
            group["collapsed_in_page"] = json!(group["collapsed_in_page"].as_u64().unwrap() + 1);
        }
    }
    page.items = groups;
    let ranking = page.ranking.get_or_insert_with(|| json!({}));
    ranking["disclosure"] = json!({"group_scope":"current_page","limit_unit":"matching_declarations",
        "symbols_per_group":3,"complete_module_counts":false});
    if q.kind.is_none() {
        let request = expand(q);
        page.hidden.push(HiddenGroup {
            category: "local_symbols".into(),
            count: None,
            reason:
                "parameters, variables and bodies are collapsed; matching count was not computed"
                    .into(),
            continue_with: request,
        });
    }
}

fn expand(q: &QueryRequest) -> Value {
    let mut value =
        json!({"operation":"search","text":q.text,"search_view":"symbols","limit":q.limit});
    for (key, entry) in [
        ("context", json!(q.context)),
        ("file", json!(q.file)),
        ("directory", json!(q.directory)),
        ("kind", json!(q.kind)),
        ("authority", json!(q.authority)),
    ] {
        if !entry.is_null() {
            value[key] = entry;
        }
    }
    if q.include_external {
        value["include_external"] = json!(true);
    }
    if q.regex {
        value["regex"] = json!(true);
    }
    value
}
