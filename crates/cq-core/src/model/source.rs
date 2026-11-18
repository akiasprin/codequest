use cq_capture_protocol::NativeValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobId {
    pub algorithm: String,
    pub digest: String,
    pub len: u64,
}
impl BlobId {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            algorithm: "sha256".into(),
            digest: digest(bytes),
            len: bytes.len() as u64,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRevision {
    pub id: String,
    pub logical_path: NativeValue,
    pub blob: BlobId,
    pub origin: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputBinding {
    ImmutableScopeVerified,
    PrePostChecked,
    Unknown,
    Changed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub id: String,
    pub files: Vec<FileRevision>,
    pub binding: InputBinding,
    pub scope: String,
    pub gaps: Vec<super::Gap>,
}
impl SourceSnapshot {
    pub fn seal(
        mut files: Vec<FileRevision>,
        binding: InputBinding,
        gaps: Vec<super::Gap>,
    ) -> Self {
        files.sort_by(|a, b| a.logical_path.cmp(&b.logical_path));
        let id = digest(&serde_json::to_vec(&files).expect("file revisions serialize"));
        Self{id,files,binding,scope:"discovered workspace sources and compiler input hints; external reads are not exhaustively observed".into(),gaps}
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAnchor {
    pub file_revision: String,
    pub start: u64,
    pub end: u64,
}
