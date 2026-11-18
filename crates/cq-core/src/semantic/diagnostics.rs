//! Diagnostics are part of scope completion, not merely displayable prose.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: String,
    pub message: String,
}
impl Diagnostic {
    pub fn is_error(&self) -> bool {
        matches!(self.severity.as_str(), "error" | "fatal")
    }
}
pub fn read(raw: &Value) -> Result<Vec<Diagnostic>> {
    if raw.get("diagnostics").is_some_and(|v| !v.is_array()) {
        return Err(Error::new(
            "InvalidDiagnostic",
            "diagnostics must be an array",
        ));
    }
    raw["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            if let Some(message) = v.as_str() {
                return Ok(Diagnostic {
                    severity: "error".into(),
                    message: message.into(),
                });
            }
            let d: Diagnostic = if v.get("severity").is_none() {
                Diagnostic {
                    severity: if matches!(
                        v["code"].as_str(),
                        Some("MultipleNativeContexts" | "DocumentationSourceUnavailable")
                    ) {
                        "warning"
                    } else {
                        "error"
                    }
                    .into(),
                    message: v["message"]
                        .as_str()
                        .ok_or_else(|| Error::new("InvalidDiagnostic", "missing message"))?
                        .into(),
                }
            } else {
                serde_json::from_value(v.clone())?
            };
            if !["ignored", "note", "warning", "error", "fatal"].contains(&d.severity.as_str()) {
                return Err(Error::new(
                    "InvalidDiagnostic",
                    "unknown diagnostic severity",
                ));
            }
            Ok(d)
        })
        .collect()
}
