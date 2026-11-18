use crate::{Result, model::*};
use cq_capture_protocol::Envelope;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub enum OutputStream {
    Stdout,
    Stderr,
    Probes,
}
impl OutputStream {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "cargo.stdout",
            Self::Stderr => "cargo.stderr",
            Self::Probes => "probes.json",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventPage {
    pub items: Vec<Envelope>,
    pub next: Option<usize>,
}
use serde::{Deserialize, Serialize};
pub trait CaptureJournal: Send + Sync {
    fn endpoint(&self) -> &Path;
    fn write_output(&self, stream: OutputStream, bytes: &[u8]) -> Result<()>;
    fn seal(&self) -> Result<SealOutcome>;
    fn recover(&self) -> Result<SealOutcome>;
    fn read(&self, receipt: &SealReceipt, offset: usize, limit: usize) -> Result<EventPage>;
    fn output(&self, receipt: &SealReceipt, stream: OutputStream) -> Result<Vec<u8>>;
    fn health(&self) -> CaptureHealth;
}
pub trait ContentReader: Send + Sync {
    fn read_blob(&self, id: &BlobId) -> Result<Vec<u8>>;
    fn verify_blob(&self, id: &BlobId) -> Result<()> {
        let bytes = self.read_blob(id)?;
        if BlobId::of(&bytes) != *id {
            return Err(crate::Error::new("DigestMismatch", &id.digest));
        }
        Ok(())
    }
}
pub trait ContentRepository: ContentReader {
    fn freeze(&self, path: &Path, origin: &str, budget: u64) -> Result<FileRevision>;
    fn put(&self, bytes: &[u8], budget: u64) -> Result<BlobId>;
}
pub trait CaptureReportRepository {
    fn save_report(&self, report: &CaptureReport) -> Result<String>;
    fn load_report(&self, run: &str) -> Result<(String, CaptureReport)>;
}
/// Storage owns capture journals and source materialization. Language analysts
/// receive these capabilities without depending on a database implementation.
pub trait AnalysisWorkspace {
    fn semantic_records(&self) -> Option<&dyn crate::semantic::records::Store> {
        None
    }
    fn load_native_cache(&self, _key: &str) -> Result<Option<NativeCache>> {
        Ok(None)
    }
    fn save_native_cache(&self, _cache: &NativeCache) -> Result<()> {
        Ok(())
    }
    fn content(&self) -> &dyn ContentRepository;
    fn run_directory(&self, run: &str) -> Result<std::path::PathBuf>;
    fn create_capture(&self, run: &str) -> Result<std::sync::Arc<dyn CaptureJournal>>;
    fn materialize_input(
        &self,
        file: &FileRevision,
        run_directory: &Path,
    ) -> Result<std::path::PathBuf>;
}

/// Storage-owned exclusive lease for a mutable, revalidated analysis mirror.
pub trait AnalysisLease: Send {}
pub mod facts;
