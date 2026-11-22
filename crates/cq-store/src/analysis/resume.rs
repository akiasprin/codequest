//! Sealed canonical spool retention. Loading restores locators, never a second
//! in-memory project payload. This is an execution artifact, not a publication.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, BufWriter},
    path::Path,
};
impl Prepared {
    pub fn retain(&self, directory: &Path) -> Result<()> {
        crate::atomic::create_directory(directory)?;
        let mut source = self.spool.file.lock().unwrap();
        source.file.sync_all()?;
        let payload = directory.join("payload");
        if payload.exists() && !directory.join("seal.json").exists() {
            std::fs::remove_file(&payload)?;
        }
        if !payload.exists() {
            if let Some(t) = &source.temporary {
                match std::fs::hard_link(t.path(), &payload) {
                    Ok(()) => (),
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                        source.file.seek(SeekFrom::Start(0))?;
                        let mut target = File::create(&payload)?;
                        std::io::copy(&mut source.file, &mut target)?;
                        target.sync_all()?;
                    }
                    Err(e) => return Err(e.into()),
                }
            } else {
                return Err(Error::new(
                    "InvalidSpool",
                    "retained spool needs its original artifact",
                ));
            }
        }
        drop(source);
        // Streaming sequences avoid a project-sized serialization buffer.
        let file = File::create(directory.join("entries"))?;
        let mut out = BufWriter::new(file);
        for (key, entry) in &self.spool.entries {
            serde_json::to_writer(&mut out, &(key, entry))?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        let file = File::create(directory.join("metadata"))?;
        let mut out = BufWriter::new(file);
        let metadata = json!({"header":self.header,"generation":self.generation,"units":self.spool.units,"universe":self.spool.universe,"sources":self.spool.sources,"shards":self.spool.by_shard.iter().collect::<Vec<_>>(),"scopes":self.spool.by_scope.iter().collect::<Vec<_>>()});
        serde_json::to_writer(&mut out, &metadata)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        let mut files = BTreeMap::new();
        for name in ["payload", "entries", "metadata"] {
            let mut f = File::open(directory.join(name))?;
            let mut hash = Sha256::new();
            let n = std::io::copy(&mut f, &mut hash)?;
            files.insert(
                name,
                json!({"bytes":n,"sha256":format!("{:x}",hash.finalize())}),
            );
        }
        crate::atomic::immutable_write(&directory.join("seal.json"), &serde_json::to_vec(&files)?)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }
    pub fn restore(store: &LocalStore, directory: &Path) -> Result<Self> {
        use std::io::BufRead;
        let seals: Value = serde_json::from_reader(File::open(directory.join("seal.json"))?)?;
        for name in ["payload", "entries", "metadata"] {
            let mut f = File::open(directory.join(name))?;
            let mut hash = Sha256::new();
            let n = std::io::copy(&mut f, &mut hash)?;
            if seals[name]["bytes"] != n
                || seals[name]["sha256"] != format!("{:x}", hash.finalize())
            {
                return Err(Error::new("DigestMismatch", "canonical spool checkpoint"));
            }
        }
        let m: Value =
            serde_json::from_reader(BufReader::new(File::open(directory.join("metadata"))?))?;
        let header: SemanticSnapshot = serde_json::from_value(m["header"].clone())?;
        let mut spool = Spool::new(&header)?;
        spool.file = Mutex::new(SpoolFile {
            file: File::open(directory.join("payload"))?,
            temporary: None,
        });
        let bytes = spool.file.lock().unwrap().file.metadata()?.len();
        for line in BufReader::new(File::open(directory.join("entries"))?).lines() {
            let (key, entry): (Arc<str>, Entry) = serde_json::from_str(&line?)?;
            if entry
                .offset
                .checked_add(entry.length)
                .is_none_or(|end| end > bytes)
                || spool.entries.insert(key, entry).is_some()
            {
                return Err(Error::new(
                    "InvalidSpool",
                    "duplicate/out of range checkpoint locator",
                ));
            }
        }
        spool.units = serde_json::from_value(m["units"].clone())?;
        spool.universe = serde_json::from_value(m["universe"].clone())?;
        spool.sources = serde_json::from_value(m["sources"].clone())?;
        spool.by_shard = serde_json::from_value::<Vec<((Arc<str>, String), Vec<Arc<str>>)>>(
            m["shards"].clone(),
        )?
        .into_iter()
        .collect();
        spool.by_scope =
            serde_json::from_value::<Vec<(ScopeKey, Vec<Arc<str>>)>>(m["scopes"].clone())?
                .into_iter()
                .collect();
        spool.diagnostic_root = Some(store.root.clone());
        let base = ScopeBase::open(store, &header.project, header.previous.as_deref())?;
        Ok(Self {
            header,
            spool,
            base,
            generation: m["generation"].clone(),
        })
    }
}
