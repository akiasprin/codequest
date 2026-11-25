//! Stable pages over sampled working-tree changes and revision comparisons.
use super::*;
use cq_core::query::{ContinuationState, QueryContinuation, request_digest};

impl Service {
    pub(super) fn collection_page(
        &self,
        project: &str,
        operation: &str,
        args: &Value,
        revision: &str,
        response: Value,
        resume: Option<&QueryContinuation>,
    ) -> Result<Value> {
        let mut input = args.clone();
        if input.get("limit").is_none() {
            input["limit"] = json!(1000);
        }
        let q = query::request(operation, input)?;
        if q.max_bytes < 128 || q.limit == 0 || q.limit > 1000 {
            return Err(Error::new("InvalidBudget", "bytes >=128; limit 1..1000"));
        }
        let (saved, mut field, mut offset) = if let Some(c) = resume {
            if c.revision != revision || c.query_digest != request_digest(&q) {
                return Err(Error::new(
                    "StaleCursor",
                    "collection scope or revision changed",
                ));
            }
            match &c.state {
                ContinuationState::Store {
                    project: owner,
                    state,
                } if owner == project && state["page_kind"] == "collections_v1" => (
                    state["response"].clone(),
                    state["field"].as_u64().unwrap_or(0) as usize,
                    state["offset"].as_u64().unwrap_or(0) as usize,
                ),
                _ => {
                    return Err(Error::new(
                        "InvalidCursor",
                        "expected sampled collection page",
                    ));
                }
            }
        } else {
            (response, 0, 0)
        };
        let keys = if operation == "compare" {
            ["modified_files", "added_files", "removed_files"]
        } else {
            ["modified", "added", "removed"]
        };
        let mut out = saved.clone();
        let mut counts = serde_json::Map::new();
        for key in keys {
            counts.insert(key.into(), json!(saved[key].as_array().map_or(0, Vec::len)));
            out[key] = json!([]);
        }
        out["revision"] = json!(revision);
        out["counts"] = Value::Object(counts);
        if out.get("state").is_none() {
            out["state"] = json!("results");
        }
        let mut delivered = 0;
        while field < keys.len() && delivered < q.limit {
            let values = saved[keys[field]]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if offset == values.len() {
                field += 1;
                offset = 0;
                continue;
            }
            if offset > values.len() {
                return Err(Error::new("InvalidCursor", "collection offset"));
            }
            out[keys[field]]
                .as_array_mut()
                .unwrap()
                .push(values[offset].clone());
            // Reserve the actual outer continuation envelope, not just its rows.
            if serde_json::to_vec(&out)?.len() + 256 > q.max_bytes {
                out[keys[field]].as_array_mut().unwrap().pop();
                if delivered == 0 {
                    return Err(Error::new(
                        "OutputBudgetTooSmall",
                        "one change record and its summary exceed max_bytes",
                    ));
                }
                break;
            }
            delivered += 1;
            offset += 1;
        }
        while field < keys.len() && offset == saved[keys[field]].as_array().map_or(0, Vec::len) {
            field += 1;
            offset = 0;
        }
        if field < keys.len() {
            let digest = request_digest(&q);
            let c = QueryContinuation {
                request: q,
                revision: revision.into(),
                query_digest: digest,
                state: ContinuationState::Store {
                    project: project.into(),
                    state: json!({
                        "page_kind":"collections_v1", "response":saved, "field":field, "offset":offset
                    }),
                },
            };
            if serde_json::to_vec(&c)?.len() > 16 * 1024 * 1024 {
                return Err(Error::new(
                    "CursorBudgetExceeded",
                    "sampled change set exceeds 16 MiB",
                ));
            }
            out["truncated"] = json!(true);
            out["next_cursor"] = json!(self.store.save_cursor(&c)?);
        }
        if serde_json::to_vec(&out)?.len() > args["max_bytes"].as_u64().unwrap_or(32768) as usize {
            return Err(Error::new(
                "OutputBudgetTooSmall",
                "collection summary exceeds max_bytes",
            ));
        }
        Ok(out)
    }
}
