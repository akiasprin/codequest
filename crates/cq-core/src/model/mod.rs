mod build;
mod semantic;
mod source;
pub use build::*;
pub use cq_capture_protocol::{Envelope, Event, ExitObservation, NativeValue};
pub use semantic::*;
use serde::{Deserialize, Serialize};
pub use source::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Observed<T> {
    Value(T),
    Unknown(String),
    Ambiguous(Vec<T>),
}
impl<T> Observed<T> {
    pub fn value(&self) -> Option<&T> {
        if let Self::Value(v) = self {
            Some(v)
        } else {
            None
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub run_id: String,
    pub source: String,
    pub index: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub code: String,
    pub scope: String,
    pub message: String,
    pub evidence: Vec<EvidenceRef>,
}
impl Gap {
    pub fn new(code: &str, scope: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            scope: scope.into(),
            message: message.into(),
            evidence: vec![],
        }
    }
}
