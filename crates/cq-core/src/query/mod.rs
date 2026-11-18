pub mod architecture;
pub mod embeddings;
mod engine;
pub mod paged;
pub mod search;
use crate::model::*;
pub use engine::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub max_lines: usize,
    pub cursor: Option<String>,
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
            line: None,
            column: None,
            max_lines: 80,
            cursor: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPage {
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
