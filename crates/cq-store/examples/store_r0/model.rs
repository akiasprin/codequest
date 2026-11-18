//! Reduced, synthetic fact vocabulary for the isolated R0 experiment only.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const INFINITY: i64 = i64::MAX;

pub fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}
pub fn digest(bytes: &[u8]) -> String {
    cq_core::model::digest(bytes)
}

pub fn durable(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
    let temp = path.with_extension(format!("{}.tmp", cq_capture_protocol::new_id()?));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(path.parent().ok_or("missing parent")?)?.sync_all()?;
    Ok(())
}
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    durable(path, &serde_json::to_vec_pretty(value)?)
}
pub fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub doc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub occurrence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Payload {
    pub shard: String,
    pub context: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub source: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PayloadRef {
    pub hash: String,
    pub source: String,
    pub context: String,
    pub nodes: usize,
    pub edges: usize,
    pub fact_bytes: usize,
    pub source_bytes: usize,
    pub adjacency_roots: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageProof {
    pub hash: String,
    pub bytes: usize,
    pub rows: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdjacencyDirectory {
    pub payload: String,
    pub context: String,
    pub subject: String,
    pub edge_count: usize,
    pub pages: Vec<PageProof>,
}
pub struct AdjacencyBundle {
    pub directory: AdjacencyDirectory,
    pub items: Vec<String>,
}
pub fn edge_id(payload: &str, edge: &Edge) -> Result<String> {
    Ok(format!("{payload}:{}", digest(&serde_json::to_vec(edge)?)))
}
pub fn adjacency_bundles(
    hash: &str,
    payload: &Payload,
) -> Result<BTreeMap<String, AdjacencyBundle>> {
    let mut subjects: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for node in &payload.nodes {
        subjects.entry(node.id.clone()).or_default();
    }
    let mut identities = std::collections::BTreeSet::new();
    for edge in &payload.edges {
        require(
            edge.from.starts_with(&format!("{}::", payload.context))
                && edge.to.starts_with(&format!("{}::", payload.context)),
            "EdgeContextMismatch",
        )?;
        let id = edge_id(hash, edge)?;
        require(identities.insert(id.clone()), "DuplicateRelation")?;
        subjects
            .entry(edge.from.clone())
            .or_default()
            .push((id, edge.to.clone()));
    }
    let mut bundles = BTreeMap::new();
    for (subject, mut edges) in subjects {
        edges.sort();
        let mut items = vec![];
        let mut pages = vec![];
        for chunk in edges.chunks(7) {
            let text = serde_json::to_string(chunk)?;
            require(
                text.len() <= 16 * 1024,
                "adjacency page byte budget exceeded",
            )?;
            pages.push(PageProof {
                hash: digest(text.as_bytes()),
                bytes: text.len(),
                rows: chunk.len(),
            });
            items.push(text);
        }
        let directory = AdjacencyDirectory {
            payload: hash.into(),
            context: payload.context.clone(),
            subject: subject.clone(),
            edge_count: edges.len(),
            pages,
        };
        require(
            serde_json::to_vec(&directory)?.len() <= 64 * 1024,
            "R0DirectoryBudget: production needs a paged directory",
        )?;
        bundles.insert(subject, AdjacencyBundle { directory, items });
    }
    Ok(bundles)
}
pub fn adjacency_roots(hash: &str, payload: &Payload) -> Result<BTreeMap<String, String>> {
    adjacency_bundles(hash, payload)?
        .into_iter()
        .map(|(s, b)| Ok((s, digest(&serde_json::to_vec(&b.directory)?))))
        .collect()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub sequence: i64,
    pub parent: Option<String>,
    pub shards: BTreeMap<String, PayloadRef>,
}
impl Manifest {
    pub fn revision(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub job: String,
    pub revision: String,
    pub manifest: Manifest,
    pub seal: String,
}
impl Candidate {
    pub fn seal(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(&(
            &self.job,
            &self.revision,
            &self.manifest,
        ))?))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitState {
    Committed,
    NotCommitted,
    Unknown,
}
#[derive(Debug, Serialize)]
pub struct PublicationFailure {
    pub outcome: CommitState,
    pub revision: String,
    pub message: String,
}
impl std::fmt::Display for PublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: {} ({})",
            self.outcome, self.message, self.revision
        )
    }
}
impl std::error::Error for PublicationFailure {}
#[derive(Debug, Serialize)]
pub struct CommitResolution {
    pub outcome: CommitState,
    pub revision: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub epoch: String,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub shard: String,
    pub old_use: Option<String>,
    pub new_payload: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub candidate: Candidate,
    pub changes: Vec<Change>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImportStats {
    pub payloads: usize,
    pub nodes: usize,
    pub edges: usize,
    pub adjacency_pages: usize,
    pub uses_started: usize,
    pub uses_closed: usize,
}

pub fn fixture(target: &str, context: &str) -> Vec<Payload> {
    let id = |name: &str| format!("{context}::{name}");
    vec![
        Payload {
            shard: format!("{context}/targets"),
            context: context.into(),
            nodes: ["A", "B", "C"]
                .into_iter()
                .map(|name| Node {
                    id: id(name),
                    name: name.into(),
                    doc: "shared target".into(),
                })
                .collect(),
            edges: vec![],
            source: digest(b"fn A() {} fn B() {} fn C() {}"),
        },
        Payload {
            shard: format!("{context}/entry"),
            context: context.into(),
            nodes: vec![Node {
                id: id("entry"),
                name: "entry".into(),
                doc: format!("author document for {target}"),
            }],
            edges: vec![Edge {
                from: id("entry"),
                to: id(target),
                occurrence: "call@1".into(),
            }],
            source: digest(format!("fn entry() {{ {target}(); }}").as_bytes()),
        },
    ]
}
pub fn fixture_sources(target: &str) -> Vec<Vec<u8>> {
    vec![
        b"fn A() {} fn B() {} fn C() {}".to_vec(),
        format!("fn entry() {{ {target}(); }}").into_bytes(),
    ]
}
