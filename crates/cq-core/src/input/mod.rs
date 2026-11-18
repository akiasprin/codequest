use crate::{Error, Result, model::*, ports::ContentReader};

pub struct InputView<'a> {
    snapshot: SourceSnapshot,
    reader: &'a dyn ContentReader,
}
impl<'a> InputView<'a> {
    pub fn open(snapshot: SourceSnapshot, reader: &'a dyn ContentReader) -> Result<Self> {
        let view = Self { snapshot, reader };
        view.verify()?;
        Ok(view)
    }
    pub fn snapshot(&self) -> &SourceSnapshot {
        &self.snapshot
    }
    pub fn resolve_file(&self, path: &NativeValue) -> Result<&FileRevision> {
        self.snapshot
            .files
            .iter()
            .find(|f| &f.logical_path == path)
            .ok_or_else(|| Error::new("MissingFile", path.display()))
    }
    pub fn read(&self, anchor: &SourceAnchor, max_bytes: usize) -> Result<Vec<u8>> {
        let f = self
            .snapshot
            .files
            .iter()
            .find(|f| f.id == anchor.file_revision)
            .ok_or_else(|| Error::new("MissingFile", "revision not in this snapshot"))?;
        if anchor.start > anchor.end
            || anchor.end > f.blob.len
            || anchor.end - anchor.start > max_bytes as u64
        {
            return Err(Error::new(
                "InvalidRange",
                "anchor exceeds content or budget",
            ));
        }
        let bytes = self.reader.read_blob(&f.blob)?;
        Ok(bytes[anchor.start as usize..anchor.end as usize].to_vec())
    }
    pub fn verify(&self) -> Result<()> {
        let expected = SourceSnapshot::seal(
            self.snapshot.files.clone(),
            self.snapshot.binding.clone(),
            self.snapshot.gaps.clone(),
        );
        if expected.id != self.snapshot.id {
            return Err(Error::new("DigestMismatch", "snapshot manifest"));
        }
        let mut paths = std::collections::HashSet::new();
        for f in &self.snapshot.files {
            if !paths.insert(&f.logical_path) {
                return Err(Error::new("InvalidSnapshot", "duplicate logical file"));
            }
            if digest(&serde_json::to_vec(&(&f.logical_path, &f.blob))?) != f.id {
                return Err(Error::new("DigestMismatch", "file revision identity"));
            }
            self.reader.verify_blob(&f.blob)?;
        }
        Ok(())
    }
}
