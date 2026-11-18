use cq_core::{Error, Result, ports::CaptureReportRepository};
use cq_store::LocalStore;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
struct Cursor {
    digest: String,
    section: String,
    offset: usize,
}
pub fn inspect(
    store: &LocalStore,
    run: &str,
    section: &str,
    limit: usize,
    cursor: Option<&str>,
) -> Result<serde_json::Value> {
    let (digest, report) = store.load_report(run)?;
    let values = match section {
        "units" => serde_json::to_value(&report.units)?,
        "invocations" => serde_json::to_value(&report.invocations)?,
        "artifacts" => serde_json::to_value(&report.artifacts)?,
        "gaps" => serde_json::to_value(
            report
                .gaps
                .iter()
                .chain(&report.snapshot.gaps)
                .collect::<Vec<_>>(),
        )?,
        "inputs" => serde_json::to_value(&report.snapshot.files)?,
        _ => return Err(Error::new("InvalidSection", section)),
    };
    let values = values.as_array().unwrap();
    let offset = if let Some(token) = cursor {
        if token.len() > 4096 {
            return Err(Error::new("InvalidCursor", "cursor too long"));
        }
        let bytes = token
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                std::str::from_utf8(p)
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    .ok_or_else(|| Error::new("InvalidCursor", "not hex"))
            })
            .collect::<Result<Vec<_>>>()?;
        if token.len() % 2 != 0 {
            return Err(Error::new("InvalidCursor", "odd length"));
        }
        let c: Cursor = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("InvalidCursor", "invalid fields"))?;
        if c.digest != digest || c.section != section || c.offset == 0 || c.offset >= values.len() {
            return Err(Error::new(
                "StaleCursor",
                "cursor does not match report/section/range",
            ));
        }
        c.offset
    } else {
        0
    };
    let end = offset.saturating_add(limit).min(values.len());
    let next = if end < values.len() {
        Some(
            serde_json::to_vec(&Cursor {
                digest: digest.clone(),
                section: section.into(),
                offset: end,
            })?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        )
    } else {
        None
    };
    Ok(
        serde_json::json!({"run_id":run,"report_digest":digest,"section":section,"items":&values[offset..end],"total":values.len(),"next_cursor":next}),
    )
}
