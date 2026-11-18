use crate::atomic::immutable_write;
use cq_core::{
    Error, Result,
    model::*,
    ports::{ContentReader, ContentRepository},
};
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct BlobStore {
    root: PathBuf,
    usage: Mutex<Usage>,
    evidence_usage: Mutex<Usage>,
}

#[derive(Default)]
struct Usage {
    bytes: u64,
    seen: HashSet<String>,
}
impl BlobStore {
    fn verify(&self, id: &BlobId) -> Result<()> {
        use sha2::{Digest, Sha256};
        let mut input = std::fs::File::open(self.path(id)?)?;
        let mut hash = Sha256::new();
        let mut size = 0;
        let mut block = [0; 64 * 1024];
        loop {
            let n = input.read(&mut block)?;
            if n == 0 {
                break;
            }
            hash.update(&block[..n]);
            size += n as u64;
            if size > id.len {
                return Err(Error::new("DigestMismatch", &id.digest));
            }
        }
        if size != id.len || format!("{:x}", hash.finalize()) != id.digest {
            return Err(Error::new("DigestMismatch", &id.digest));
        }
        Ok(())
    }
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            usage: Mutex::new(Usage::default()),
            evidence_usage: Mutex::new(Usage::default()),
        }
    }
    /// A separate accounting scope over the same immutable content address space.
    /// Derived graph output must not consume the compiler-input reservation.
    pub(crate) fn budget_scope(&self) -> Self {
        Self::new(self.root.clone())
    }
    pub fn path(&self, id: &BlobId) -> Result<PathBuf> {
        if id.algorithm != "sha256"
            || id.digest.len() != 64
            || !id.digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::new("InvalidBlob", "invalid identity"));
        }
        Ok(self.root.join(&id.digest))
    }
}
impl ContentReader for BlobStore {
    fn verify_blob(&self, id: &BlobId) -> Result<()> {
        self.verify(id)
    }
    fn read_blob(&self, id: &BlobId) -> Result<Vec<u8>> {
        let path = self.path(id)?;
        let mut bytes = Vec::new();
        let mut file = std::fs::File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::new("MissingContent", &id.digest)
            } else {
                e.into()
            }
        })?;
        file.by_ref()
            .take(id.len.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if BlobId::of(&bytes) != *id {
            return Err(Error::new("DigestMismatch", &id.digest));
        }
        Ok(bytes)
    }
}
impl ContentRepository for BlobStore {
    fn put(&self, bytes: &[u8], budget: u64) -> Result<BlobId> {
        let id = BlobId::of(bytes);
        let path = self.path(&id)?;
        let mut usage = self.usage.lock().unwrap();
        if usage.seen.contains(&id.digest) {
            self.verify(&id)?;
            return Ok(id);
        }
        if id.len > budget.saturating_sub(usage.bytes) {
            return Err(Error::new(
                "ContentLimit",
                format!(
                    "source content budget exceeded: used={} requested={} limit={} bytes; blob={}",
                    usage.bytes, id.len, budget, id.digest
                ),
            ));
        }
        if path.exists() {
            self.verify(&id)?;
        } else {
            immutable_write(&path, bytes)?;
        }
        usage.bytes += id.len;
        usage.seen.insert(id.digest.clone());
        Ok(id)
    }
    fn freeze(&self, path: &Path, origin: &str, budget: u64) -> Result<FileRevision> {
        self.freeze_into(path, origin, budget, &self.usage, "source")
    }
    fn freeze_evidence(&self, path: &Path, origin: &str, budget: u64) -> Result<FileRevision> {
        self.freeze_into(
            path,
            origin,
            budget,
            &self.evidence_usage,
            "native_evidence",
        )
    }
}
impl BlobStore {
    fn freeze_into(
        &self,
        path: &Path,
        origin: &str,
        budget: u64,
        account: &Mutex<Usage>,
        category: &str,
    ) -> Result<FileRevision> {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        std::fs::create_dir_all(&self.root)?;
        let mut input = std::fs::File::open(path)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.root)?;
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut block = [0; 64 * 1024];
        loop {
            let n = input.read(&mut block)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > budget {
                return Err(Error::new(
                    "ContentLimit",
                    format!(
                        "{category} file budget exceeded: path={} read={total} limit={budget} bytes",
                        path.display()
                    ),
                ));
            }
            hash.update(&block[..n]);
            tmp.write_all(&block[..n])?;
        }
        let blob = BlobId {
            algorithm: "sha256".into(),
            digest: format!("{:x}", hash.finalize()),
            len: total,
        };
        let target = self.path(&blob)?;
        let mut usage = account.lock().unwrap();
        if usage.seen.contains(&blob.digest) {
            self.verify(&blob)?;
        } else {
            if total > budget.saturating_sub(usage.bytes) {
                return Err(Error::new(
                    "ContentLimit",
                    format!(
                        "{category} content budget exceeded: used={} requested={total} limit={budget} bytes; path={}",
                        usage.bytes,
                        path.display()
                    ),
                ));
            }
            if target.exists() {
                self.verify(&blob)?;
            } else {
                tmp.as_file().sync_all()?;
                match tmp.persist_noclobber(&target) {
                    Ok(_) => {}
                    Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        self.verify(&blob)?
                    }
                    Err(e) => return Err(e.error.into()),
                };
                std::fs::File::open(&self.root)?.sync_all()?;
            }
            usage.bytes += total;
            usage.seen.insert(blob.digest.clone());
        }
        let logical_path = NativeValue::new(path);
        let id = digest(&serde_json::to_vec(&(&logical_path, &blob))?);
        Ok(FileRevision {
            id,
            logical_path,
            blob,
            origin: origin.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derived_output_has_separate_accounting_without_relaxing_input_limits() {
        let t = tempfile::tempdir().unwrap();
        let inputs = BlobStore::new(t.path().join("blobs"));
        let first = inputs.put(b"abc", 4).unwrap();
        let output = inputs.budget_scope();
        output.put(b"output", 8).unwrap();
        assert!(output.put(b"more", 8).is_err());
        assert!(inputs.put(b"def", 4).is_err());
        assert_eq!(inputs.read_blob(&first).unwrap(), b"abc");
        assert_eq!(output.read_blob(&first).unwrap(), b"abc");
    }
    #[test]
    fn native_export_larger_than_source_budget_does_not_poison_source_normalization() {
        let t = tempfile::tempdir().unwrap();
        let store = BlobStore::new(t.path().join("blobs"));
        store.put(b"abc", 4).unwrap();
        let export = t.path().join("hir.json");
        std::fs::write(&export, b"native-output").unwrap();
        let evidence = store
            .freeze_evidence(&export, "native_export_evidence", 16)
            .unwrap();
        store.put(b"d", 4).unwrap();
        assert_eq!(store.read_blob(&evidence.blob).unwrap(), b"native-output");
        let error = store.put(b"e", 4).unwrap_err();
        assert_eq!(error.code(), "ContentLimit");
        assert!(error.message().contains("used=4 requested=1 limit=4"));
        let other = t.path().join("other.json");
        std::fs::write(&other, b"more").unwrap();
        let error = store
            .freeze_evidence(&other, "native_export_evidence", 16)
            .unwrap_err();
        assert!(error.message().contains("native_evidence"));
        assert!(error.message().contains("used=13 requested=4 limit=16"));
    }
}
