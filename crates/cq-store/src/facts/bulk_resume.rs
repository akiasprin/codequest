//! Frozen sort runs are shared by hard link; merge work always uses disposable
//! links, so restarting a directory build cannot mutate its encoded input.
use super::*;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
struct Part {
    count: u64,
    prefix: String,
    project: Option<String>,
    runs: Vec<(usize, String, u64, String)>,
}
#[derive(Serialize, Deserialize)]
struct Image {
    version: u32,
    count: u64,
    project: Option<String>,
    parts: Vec<Part>,
}
impl Builder {
    pub(crate) fn retain(&mut self, directory: &Path) -> Result<()> {
        use sha2::{Digest, Sha256};
        crate::atomic::create_directory(directory)?;
        let mut parts = vec![];
        for (n, p) in self.parts.iter_mut().enumerate() {
            p.flush()?;
            p.join_pending()?;
            let mut runs = vec![];
            for (i, (level, path)) in p.runs.iter().enumerate() {
                let name = format!("{n}-{i}.lz4");
                let retained = directory.join(&name);
                std::fs::File::open(path)?.sync_all()?;
                if !retained.exists() {
                    std::fs::hard_link(path, &retained)?;
                }
                let mut hash = Sha256::new();
                let bytes = std::io::copy(&mut std::fs::File::open(&retained)?, &mut hash)?;
                runs.push((*level, name, bytes, format!("{:x}", hash.finalize())));
            }
            parts.push(Part {
                count: p.count,
                prefix: p.prefix.clone(),
                project: p.project.clone(),
                runs,
            });
        }
        crate::atomic::immutable_write(
            &directory.join("runs.json"),
            &serde_json::to_vec(&Image {
                version: 1,
                count: self.count,
                project: self.project.clone(),
                parts,
            })?,
        )?;
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    }
    pub(crate) fn restore(root: &Path, directory: &Path) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let image: Image =
            serde_json::from_reader(std::fs::File::open(directory.join("runs.json"))?)?;
        if image.version != 1 || image.parts.len() != 16 {
            return Err(Error::new(
                "InvalidIndexCheckpoint",
                "builder version/partitions",
            ));
        }
        let mut builder = Self::new(root)?;
        builder.count = image.count;
        builder.project = image.project;
        for (p, saved) in builder.parts.iter_mut().zip(image.parts) {
            p.count = saved.count;
            p.prefix = saved.prefix;
            p.project = saved.project;
            for (level, name, bytes, digest) in saved.runs {
                if name.contains('/') || !name.ends_with(".lz4") {
                    return Err(Error::new("InvalidIndexCheckpoint", "run path"));
                }
                let source = directory.join(name);
                let mut hash = Sha256::new();
                let n = std::io::copy(&mut std::fs::File::open(&source)?, &mut hash)?;
                if n != bytes || format!("{:x}", hash.finalize()) != digest {
                    return Err(Error::new("DigestMismatch", "retained sort run"));
                }
                p.shared_disk.charge(bytes).map_err(scratch_error)?;
                let path = p.path();
                std::fs::hard_link(source, &path)?;
                p.runs.push((level, path));
                p.live_bytes += bytes;
            }
        }
        Ok(builder)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_directory_attempt_keeps_frozen_input_for_an_exact_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = Objects::buffered(tmp.path().join("objects")).unwrap();
        let mut original = Builder::new(&objects.root).unwrap();
        for n in 0..8000 {
            original
                .push("all:entity", &format!("entity:e{n}"), serde_json::json!(n))
                .unwrap();
        }
        let input = tmp.path().join("encoded");
        original.retain(&input).unwrap();
        let expected = original.build(&objects).unwrap();
        objects.sync_pending().unwrap();
        let control = cq_core::control::CaptureControl::default();
        control.cancel();
        let mut cancelled = Builder::restore(&objects.root, &input).unwrap();
        cancelled.cancel_with(control);
        cancelled.resume_in(tmp.path().join("ranges")).unwrap();
        assert_eq!(cancelled.build(&objects).unwrap_err().code(), "Cancelled");
        let mut resumed = Builder::restore(&objects.root, &input).unwrap();
        resumed.cancel_with(Default::default());
        resumed.resume_in(tmp.path().join("ranges")).unwrap();
        assert_eq!(resumed.build(&objects).unwrap(), expected);
    }
}
