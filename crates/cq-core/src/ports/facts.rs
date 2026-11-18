//! Fixed-revision reads. Storage engines and physical encodings stay outside core.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub hash: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactRef {
    pub object: ObjectRef,
    pub offset: u64,
    pub length: u64,
    pub digest: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReadStats {
    pub page_reads: u64,
    pub bytes_decoded: u64,
    pub provider_rows: u64,
    pub database_queries: u64,
    pub full_snapshot_decodes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactRow {
    pub key: String,
    pub reference: FactRef,
    pub value: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactBatch {
    pub rows: Vec<FactRow>,
    pub after: Option<String>,
    pub complete: bool,
}
/// A view cannot be modified after admission. Implementations bind every read
/// to these coordinates and retain an execution pin for the reader's lifetime.
pub trait FactReader: Send + Sync {
    /// A request budget includes queueing and native database acquisition.
    fn set_timeout(&self, _timeout_ms: u64) {}
    /// Derive a request pin from an already admitted session. Implementations
    /// keep that pin until the returned reader is dropped, even past lease expiry.
    fn request_view(&self) -> Result<Option<Box<dyn FactReader>>> {
        Ok(None)
    }
    fn revision(&self) -> &str;
    fn project(&self) -> &str;
    fn metadata(&self) -> &Value;
    fn get(&self, family: &str, id: &str) -> Result<Option<FactRow>>;
    fn scan(&self, group: &str, after: Option<&str>, limit: usize) -> Result<FactBatch>;
    fn count(&self, group: &str) -> Result<u64>;
    fn source_range(&self, file: &str, start: u64, end: u64) -> Result<Vec<u8>>;
    fn stats(&self) -> ReadStats;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitOutcome {
    Committed,
    NotCommitted,
    Unknown,
}

/// Failure after a durable operation may mean the result needs recovery.
/// Callers must preserve the revision and resolve Unknown before retrying.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitResult {
    pub revision: String,
    pub outcome: CommitOutcome,
    pub detail: Option<String>,
}

pub fn fact_key(family: &str, id: &str) -> String {
    format!("{family}:{id}")
}
pub fn group(kind: &str, subject: &str) -> String {
    format!("{kind}:{subject}")
}

/// Index keys describe objective symbols, owners, and references. No inferred
/// responsibility grouping is used to remove facts or decide reachability.
pub fn groups(family: &str, value: &Value) -> Vec<String> {
    let mut out = vec![format!("all:{family}")];
    let s = |k: &str| value[k].as_str().unwrap_or("");
    match family {
        "entity" => {
            if value["external"] == false {
                out.push("internal:entity".into());
            }
            out.push(group("kind_entity", &s("kind").to_ascii_lowercase()));
            if let Some(id) = value["source"]["file_revision"].as_str() {
                out.push(group("file_entity", id));
            }
            for k in ["id", "stable_handle", "name", "qualified_name"] {
                if !s(k).is_empty() {
                    out.push(group("lookup", s(k)));
                }
            }
            if !s("parent").is_empty() {
                out.push(group("member", s("parent")));
            }
            out.push(group("unit", s("unit")));
        }
        "relation" => {
            out.push(group("out", s("from")));
            out.push(group("in", s("to")));
            if !s("occurrence").is_empty() {
                out.push(group("occurrence_relation", s("occurrence")));
            }
            if matches!(s("kind"), "Contains" | "HasMember" | "LexicallyContains") {
                out.push(group("membership", s("from")));
                out.push(group(
                    "membership_pair",
                    &format!("{}\0{}", s("from"), s("to")),
                ));
            }
            if matches!(s("kind"), "MayCall" | "Dispatches") {
                out.push(group("candidate_out", s("from")));
                out.push(group("candidate_in", s("to")));
            }
        }
        "occurrence" => {
            if let Some(id) = value["source"]["file_revision"].as_str() {
                out.push(group("file_occurrence", id));
            }
            if !s("owner").is_empty() {
                out.push(group("occurrence_owner", s("owner")));
            }
            if value["declaration"]["state"] == "Resolved"
                && let Some(id) = value["declaration"]["value"].as_str()
            {
                out.push(group("reference", id));
            }
        }
        "body" => {
            out.push(group("body", s("owner")));
        }
        "build_dependencies" => {
            out.push(group("build_dependency", s("consumer")));
        }
        "document" if !s("owner").is_empty() => {
            out.push(group("document", s("owner")));
        }
        _ => {}
    }
    out.sort();
    out.dedup();
    out
}
