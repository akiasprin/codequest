use crate::{
    atomic::{immutable_write, valid_id},
    content::BlobStore,
};
use cq_core::{Error, Result, model::*, ports::CaptureReportRepository};
use std::path::{Path, PathBuf};

pub struct LocalStore {
    pub root: PathBuf,
    pub content: BlobStore,
    pub(crate) raw: Option<std::sync::Arc<crate::raw::Journal>>,
}
impl LocalStore {
    /// A new indexing job owns a fresh compiler-input reservation. Immutable
    /// blobs, journals, leases and HEAD publication locks remain shared on disk.
    pub fn analysis_scope(&self) -> Self {
        Self {
            root: self.root.clone(),
            content: self.content.budget_scope(),
            raw: None,
        }
    }
    pub fn streaming_scope(&self) -> Result<Self> {
        let mut s = self.analysis_scope();
        s.raw = Some(std::sync::Arc::new(crate::raw::Journal::new(
            self.root.clone(),
        )?));
        Ok(s)
    }
    pub fn analysis_records(&self) -> Option<&crate::raw::Journal> {
        self.raw.as_deref()
    }
    /// After canonical preparation, the sealed spool owns the publication
    /// payloads. Release the temporary raw journal before building indexes.
    pub fn finish_streaming(mut self) -> Self {
        self.raw = None;
        self
    }
    pub fn open(path: &Path) -> Result<Self> {
        crate::atomic::create_directory(path)?;
        crate::atomic::create_directory(&path.join("blobs"))?;
        crate::atomic::sync_directory_chain(&path.join("blobs"))?;
        let root = path.canonicalize()?;
        Ok(Self {
            content: BlobStore::new(root.join("blobs")),
            root,
            raw: None,
        })
    }
    pub fn run_path(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err(Error::new("InvalidRunId", id));
        }
        Ok(self.root.join("runs").join(id))
    }
    pub fn recover_capture(&self, id: &str) -> Result<SealOutcome> {
        let dir = self.run_path(id)?;
        let path = dir.join("seal.json");
        if !path.exists() {
            return Ok(SealOutcome::NotSealed(
                if dir.join("events.log").exists() {
                    "journal exists without committed seal"
                } else {
                    "capture run does not exist"
                }
                .into(),
            ));
        }
        let receipt: SealReceipt = serde_json::from_slice(&std::fs::read(path)?)?;
        if receipt.run_id != id {
            return Err(Error::new("CorruptJournal", "recovery run mismatch"));
        }
        super::journal::verify_receipt(&dir, &receipt)?;
        Ok(SealOutcome::Sealed(receipt))
    }
}
impl CaptureReportRepository for LocalStore {
    fn save_report(&self, report: &CaptureReport) -> Result<String> {
        cq_core::validation::validate_report(report, &self.content)?;
        super::journal::verify_report_evidence(&self.run_path(&report.run.id)?, report)?;
        let bytes = serde_json::to_vec_pretty(report)?;
        let hash = digest(&bytes);
        immutable_write(&self.run_path(&report.run.id)?.join("report.json"), &bytes)?;
        immutable_write(
            &self.run_path(&report.run.id)?.join("report.sha256"),
            hash.as_bytes(),
        )?;
        Ok(hash)
    }
    fn load_report(&self, run: &str) -> Result<(String, CaptureReport)> {
        let dir = self.run_path(run)?;
        let bytes = std::fs::read(dir.join("report.json"))?;
        let hash = std::fs::read_to_string(dir.join("report.sha256"))?;
        if digest(&bytes) != hash {
            return Err(Error::new("DigestMismatch", "capture report"));
        }
        let report: CaptureReport = serde_json::from_slice(&bytes)?;
        if report.run.id != run || report.plan.run_id != run {
            return Err(Error::new("CorruptReport", "run identity mismatch"));
        }
        if let SealOutcome::Sealed(r) = &report.seal {
            super::journal::verify_receipt(&dir, r)?;
        }
        cq_core::validation::validate_report(&report, &self.content)?;
        super::journal::verify_report_evidence(&dir, &report)?;
        Ok((hash, report))
    }
}
