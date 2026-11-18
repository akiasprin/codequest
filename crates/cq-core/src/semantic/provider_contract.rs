//! Validate native transport identities and byte coordinates before mutating a
//! snapshot. Missing optional evidence is legal; corrupt supplied evidence is not.
use crate::{Error, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn invalid(message: impl Into<String>) -> Error {
    Error::new("InvalidProviderOutput", message)
}

pub(super) fn validate(raw: &Value) -> Result<()> {
    validate_impl(raw, true)
}
pub(super) fn validate_metadata(raw: &Value) -> Result<()> {
    validate_impl(raw, false)
}
fn validate_impl(raw: &Value, ranges: bool) -> Result<()> {
    if raw["protocol"] != "codequest-hir-export" || raw["schema_version"] != 1 {
        return Err(Error::new("ProtocolMismatch", "unsupported native export"));
    }
    for field in [
        "files",
        "symbols",
        "references",
        "parents",
        "relations",
        "bodies",
        "calls",
        "doc_links",
    ] {
        if !raw[field].is_array() {
            return Err(invalid(format!("missing array {field}")));
        }
    }
    let mut files = HashMap::new();
    let mut paths = HashSet::new();
    for f in raw["files"].as_array().unwrap() {
        let (Some(id), Some(path), Some(text)) =
            (f["id"].as_u64(), f["path"].as_str(), f["text"].as_str())
        else {
            return Err(invalid("file identity/path/text missing"));
        };
        if f.get("unit").is_some() && !f["unit"].is_string() {
            return Err(invalid("file unit must be a string"));
        }
        if path.is_empty()
            || files.insert(id, text).is_some()
            || !paths.insert((path, f["unit"].as_str().unwrap_or("")))
        {
            return Err(invalid("duplicate/empty native file identity"));
        }
    }
    let mut tokens = HashSet::new();
    for s in raw["symbols"].as_array().unwrap() {
        let Some(token) = s["token"].as_u64() else {
            return Err(invalid("symbol token missing"));
        };
        for flag in [
            "external",
            "exported",
            "local",
            "trait_member",
            "const_function",
            "is_definition",
        ] {
            if !s[flag].is_null() && !s[flag].is_boolean() {
                return Err(invalid(format!("symbol flag {flag} must be boolean")));
            }
        }
        if !tokens.insert(token) || !s["key"].is_string() || !s["kind"].is_string() {
            return Err(invalid("duplicate token or missing symbol key/kind"));
        }
    }
    for r in raw["references"].as_array().unwrap() {
        if !r["kind"].is_null()
            && !r["kind"].as_str().is_some_and(|k| {
                [
                    "Call",
                    "Type",
                    "Access",
                    "Import",
                    "Export",
                    "Instantiate",
                    "Decorate",
                    "Reference",
                ]
                .contains(&k)
            })
        {
            return Err(invalid("unknown native reference role"));
        }
        if !r["is_definition"].is_null() && !r["is_definition"].is_boolean() {
            return Err(invalid("reference definition flag"));
        }
        if !r["target"].as_u64().is_some_and(|t| tokens.contains(&t)) {
            return Err(invalid("reference target absent"));
        }
        if !r["call_index"].is_null()
            && !r["call_index"]
                .as_u64()
                .is_some_and(|i| i < raw["calls"].as_array().unwrap().len() as u64)
        {
            return Err(invalid("reference call index absent"));
        }
    }
    let mut parents = HashMap::new();
    for p in raw["parents"].as_array().unwrap() {
        let Some(child) = p["child"].as_u64().filter(|t| tokens.contains(t)) else {
            return Err(invalid("parent child absent"));
        };
        if !p["parent_token"].is_null() && !p["parent_token"].is_u64() {
            return Err(invalid("parent token must be native integer"));
        }
        if p["parent_token"].is_null()
            && (!p["key"].as_str().is_some_and(|k| !k.is_empty()) || !p["kind"].is_string())
        {
            return Err(invalid("unindexed parent requires native key/kind"));
        }
        if let Some(parent) = p["parent_token"].as_u64() {
            if !tokens.contains(&parent) {
                return Err(invalid("parent token absent"));
            }
            if let Some(old) = parents.insert(child, parent)
                && old != parent
            {
                return Err(invalid("conflicting native parents"));
            }
        }
    }
    let mut finished = HashSet::new();
    for child in parents.keys() {
        let mut chain = HashSet::new();
        let mut next = *child;
        while let Some(parent) = parents.get(&next) {
            if finished.contains(&next) {
                break;
            }
            if !chain.insert(next) {
                return Err(invalid("native parent cycle"));
            }
            next = *parent;
        }
        finished.extend(chain);
    }
    for p in raw["nominal_members"].as_array().into_iter().flatten() {
        for key in ["child", "owner"] {
            if !p[key].as_u64().is_some_and(|t| tokens.contains(&t)) {
                return Err(invalid("nominal membership endpoint absent"));
            }
        }
    }
    if ranges {
        validate_ranges(raw, &files)
    } else {
        Ok(())
    }
}

fn validate_ranges(value: &Value, files: &HashMap<u64, &str>) -> Result<()> {
    match value {
        Value::Object(object) => {
            if object.contains_key("start") || object.contains_key("end") {
                let (Some(file), Some(start), Some(end)) = (
                    value["file"].as_u64(),
                    value["start"].as_u64(),
                    value["end"].as_u64(),
                ) else {
                    return Err(invalid("incomplete native byte range"));
                };
                let Some(text) = files.get(&file) else {
                    return Err(invalid("range file absent"));
                };
                if start > end
                    || end > text.len() as u64
                    || !text.is_char_boundary(start as usize)
                    || !text.is_char_boundary(end as usize)
                {
                    return Err(invalid("native range out of bounds or splits UTF-8"));
                }
            }
            for child in object.values() {
                validate_ranges(child, files)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_ranges(child, files)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn frame() -> Value {
        json!({"protocol":"codequest-hir-export","schema_version":1,
            "files":[{"id":0,"path":"/source.rs","text":"é fn f() {}"}],
            "symbols":[{"token":0,"key":"f","kind":"Function","definition":{"file":0,"start":6,"end":7}},
                {"token":1,"key":"external","kind":"FutureNativeKind","definition":null}],
            "references":[{"target":0,"source":{"file":0,"start":6,"end":7}}],
            "parents":[],"relations":[],"bodies":[],"calls":[],"doc_links":[]})
    }
    macro_rules! rejects {
        ($name:ident, $edit:expr) => {
            #[test]
            fn $name() {
                let mut raw = frame();
                ($edit)(&mut raw);
                assert!(
                    validate(&raw).is_err(),
                    "accepted malformed provider frame: {raw}"
                );
            }
        };
    }
    #[test]
    fn accepts_native_zero_ids_external_symbols_and_unknown_kinds() {
        validate(&frame()).unwrap();
    }
    rejects!(rejects_protocol, |v: &mut Value| v["protocol"] =
        json!("other"));
    rejects!(rejects_schema, |v: &mut Value| v["schema_version"] =
        json!(2));
    rejects!(rejects_missing_files, |v: &mut Value| v
        .as_object_mut()
        .unwrap()
        .remove("files"));
    rejects!(rejects_missing_symbols, |v: &mut Value| v["symbols"] =
        Value::Null);
    rejects!(
        rejects_missing_references,
        |v: &mut Value| v["references"] = json!({})
    );
    rejects!(rejects_missing_parents, |v: &mut Value| v["parents"] =
        Value::Null);
    rejects!(rejects_missing_relations, |v: &mut Value| v["relations"] =
        Value::Null);
    rejects!(rejects_missing_bodies, |v: &mut Value| v["bodies"] =
        Value::Null);
    rejects!(rejects_missing_calls, |v: &mut Value| v["calls"] =
        Value::Null);
    rejects!(rejects_missing_doc_links, |v: &mut Value| v["doc_links"] =
        Value::Null);
    rejects!(rejects_file_identity_missing, |v: &mut Value| v["files"]
        [0]["id"] =
        Value::Null);
    rejects!(
        rejects_file_text_missing,
        |v: &mut Value| v["files"][0]["text"] = Value::Null
    );
    rejects!(rejects_duplicate_file_id, |v: &mut Value| {
        let x = v["files"][0].clone();
        v["files"].as_array_mut().unwrap().push(x);
    });
    rejects!(rejects_duplicate_file_path, |v: &mut Value| {
        let mut x = v["files"][0].clone();
        x["id"] = json!(1);
        v["files"].as_array_mut().unwrap().push(x);
    });
    rejects!(rejects_duplicate_symbol_token, |v: &mut Value| v["symbols"]
        [1]["token"] =
        json!(0));
    rejects!(rejects_symbol_kind_missing, |v: &mut Value| v["symbols"]
        [0]["kind"] =
        Value::Null);
    rejects!(rejects_symbol_key_missing, |v: &mut Value| v["symbols"]
        [0]["key"] =
        Value::Null);
    rejects!(rejects_dangling_reference, |v: &mut Value| v["references"]
        [0]["target"] =
        json!(99));
    rejects!(
        rejects_reference_target_missing,
        |v: &mut Value| v["references"][0]["target"] = Value::Null
    );
    rejects!(
        rejects_dangling_parent_child,
        |v: &mut Value| v["parents"] = json!([{"child":99,"parent_token":0}])
    );
    rejects!(
        rejects_dangling_parent_token,
        |v: &mut Value| v["parents"] = json!([{"child":0,"parent_token":99}])
    );
    rejects!(
        rejects_parent_cycle,
        |v: &mut Value| v["parents"] =
            json!([{"child":0,"parent_token":1},{"child":1,"parent_token":0}])
    );
    rejects!(
        rejects_conflicting_parent,
        |v: &mut Value| v["parents"] =
            json!([{"child":0,"parent_token":1},{"child":0,"parent_token":0}])
    );
    rejects!(
        rejects_dangling_nominal_member,
        |v: &mut Value| v["nominal_members"] = json!([{"child":0,"owner":99}])
    );
    rejects!(rejects_range_file_absent, |v: &mut Value| v["symbols"][0]
        ["definition"]["file"] =
        json!(99));
    rejects!(
        rejects_range_reversed,
        |v: &mut Value| v["symbols"][0]["definition"]["end"] = json!(2)
    );
    rejects!(rejects_range_out_of_bounds, |v: &mut Value| v["symbols"]
        [0]["definition"]["end"] =
        json!(999));
    rejects!(rejects_range_incomplete, |v: &mut Value| v["symbols"][0]
        ["definition"]["end"] =
        Value::Null);
    rejects!(rejects_range_splits_utf8, |v: &mut Value| v["symbols"][0]
        ["definition"] =
        json!({"file":0,"start":1,"end":2}));
    rejects!(rejects_nested_argument_range, |v: &mut Value| v["calls"] =
        json!([{"arguments":[{"file":0,"start":0,"end":999}]}]));
    rejects!(rejects_non_string_file_unit, |v: &mut Value| v["files"]
        [0]["unit"] =
        json!(true));
    rejects!(
        rejects_non_boolean_symbol_flags,
        |v: &mut Value| v["symbols"][0]["external"] = json!("false")
    );
    rejects!(
        rejects_non_boolean_exported_flag,
        |v: &mut Value| v["symbols"][0]["exported"] = json!(1)
    );
    rejects!(
        rejects_unknown_reference_role,
        |v: &mut Value| v["references"][0]["kind"] = json!("runtime-certain")
    );
    rejects!(rejects_non_boolean_reference_definition, |v: &mut Value| {
        v["references"][0]["is_definition"] = json!("true")
    });
    rejects!(
        rejects_non_integer_parent_token,
        |v: &mut Value| v["parents"] = json!([{"child":0,"parent_token":"1"}])
    );
    rejects!(
        rejects_unidentified_unindexed_parent,
        |v: &mut Value| v["parents"] = json!([{"child":0}])
    );
    rejects!(rejects_dangling_call_index, |v: &mut Value| v["references"]
        [0]["call_index"] =
        json!(3));
}
