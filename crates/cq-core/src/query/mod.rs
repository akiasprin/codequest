pub mod architecture;
pub mod embeddings;
mod engine;
pub mod paged;
pub mod search;
use crate::model::*;
pub use engine::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) type AnchorLocation = (String, Option<(usize, usize, String)>);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryRequest {
    pub authority: Option<Authority>,
    pub mode: String,
    pub document_kind: Option<String>,
    pub include_external: bool,
    pub operation: String,
    pub subject: Option<String>,
    pub target: Option<String>,
    pub text: String,
    pub context: Option<String>,
    pub file: Option<String>,
    pub kind: Option<String>,
    pub relation: Option<String>,
    pub direction: String,
    pub depth: usize,
    pub limit: usize,
    pub max_bytes: usize,
    pub max_work: usize,
    pub timeout_ms: u64,
    pub offset: usize,
    pub regex: bool,
    pub include_candidates: bool,
    /// Scan persisted vectors exactly instead of approximate LSH candidates.
    pub exact_semantic: bool,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub max_lines: usize,
    pub cursor: Option<String>,
    pub compact: bool,
}
impl Default for QueryRequest {
    fn default() -> Self {
        Self {
            authority: None,
            mode: "calls".into(),
            document_kind: None,
            include_external: false,
            operation: "overview".into(),
            subject: None,
            target: None,
            text: String::new(),
            context: None,
            file: None,
            kind: None,
            relation: None,
            direction: "outbound".into(),
            depth: 3,
            limit: 7,
            max_bytes: 32768,
            max_work: 1000000,
            timeout_ms: 5000,
            offset: 0,
            regex: false,
            include_candidates: false,
            exact_semantic: false,
            line: None,
            column: None,
            max_lines: 80,
            cursor: None,
            compact: false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking: Option<Value>,
    pub provenance_records: std::collections::BTreeMap<ProvenanceId, Provenance>,
    pub revision: String,
    pub context: Option<String>,
    pub items: Vec<Value>,
    pub total: Option<usize>,
    pub total_kind: String,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub state: String,
    pub coverage: Vec<CapabilityCoverage>,
    pub gaps: Vec<Gap>,
    pub hidden: Vec<HiddenGroup>,
    pub work: usize,
    pub elapsed_ms: u64,
}
impl QueryPage {
    pub fn to_compact_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("revision".into(), serde_json::json!(self.revision));
        if let Some(context) = &self.context {
            map.insert("context".into(), serde_json::json!(context));
        }
        map.insert("items".into(), serde_json::json!(self.items));
        if let Some(total) = self.total {
            map.insert("total".into(), serde_json::json!(total));
        }
        if self.truncated {
            map.insert("truncated".into(), serde_json::json!(true));
        }
        if let Some(cursor) = &self.next_cursor {
            map.insert("next_cursor".into(), serde_json::json!(cursor));
        }
        if let Some(ranking) = &self.ranking {
            map.insert("ranking".into(), ranking.clone());
        }
        map.insert("state".into(), serde_json::json!(self.state));
        if !self.coverage.is_empty() {
            map.insert("coverage".into(), serde_json::json!(self.coverage));
        }
        if !self.gaps.is_empty() {
            map.insert("gaps".into(), serde_json::json!(self.gaps));
        }
        if !self.hidden.is_empty() {
            map.insert("hidden".into(), serde_json::json!(self.hidden));
        }
        if !self.provenance_records.is_empty() {
            map.insert(
                "provenance_records".into(),
                serde_json::json!(self.provenance_records),
            );
        }
        Value::Object(map)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HiddenGroup {
    pub category: String,
    pub count: Option<usize>,
    pub reason: String,
    pub continue_with: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryContinuation {
    pub request: QueryRequest,
    pub revision: String,
    pub query_digest: String,
    pub state: ContinuationState,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContinuationState {
    Store {
        project: String,
        state: Value,
    },
    Scan {
        position: usize,
        rows: Vec<Value>,
    },
    Rows {
        rows: Vec<Value>,
        offset: usize,
        total: Option<usize>,
        complete: bool,
        after: Option<Box<ContinuationState>>,
    },
    Walk {
        queue: Vec<WalkNode>,
        visited: Vec<String>,
        target: Option<String>,
        path_mode: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalkNode {
    /// Resume within a high-fanout node without charging/returning it twice.
    #[serde(default)]
    pub edge_offset: Option<usize>,
    pub id: String,
    pub hop: usize,
    pub path: Vec<String>,
    pub via: Option<String>,
}
pub fn request_digest(request: &QueryRequest) -> String {
    let mut r = request.clone();
    r.cursor = None;
    r.limit = 0;
    r.max_bytes = 0;
    r.max_work = 0;
    r.timeout_ms = 0;
    r.offset = 0;
    digest(&serde_json::to_vec(&r).expect("query identity"))
}
