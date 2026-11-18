//! Frozen evidence read by range; file paths are never opened on reads.
use super::{
    objects::Objects,
    tree::{self, Root},
};
use cq_core::{
    Error, Result,
    model::BlobId,
    ports::facts::{ObjectRef, ReadStats},
};
use serde::{Deserialize, Serialize};
use std::io::Read;
pub const BLOCK: usize = 64 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub blob: BlobId,
    pub blocks: Root,
}
impl Source {
    pub fn freeze(o: &Objects, blob: &BlobId, mut input: impl Read) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let mut total = 0;
        let mut blocks = None;
        let mut ordinal = 0;
        loop {
            let mut b = vec![0; BLOCK];
            let mut n = 0;
            while n < BLOCK {
                let read = input.read(&mut b[n..])?;
                if read == 0 {
                    break;
                }
                n += read;
            }
            if n == 0 {
                break;
            }
            b.truncate(n);
            hash.update(&b);
            total += n as u64;
            let r = o.put(&b)?;
            blocks = tree::set(
                o,
                &blocks,
                &ordinal.to_string(),
                Some(serde_json::to_value(r)?),
                &mut ReadStats::default(),
            )?;
            ordinal += 1;
        }
        if blob.algorithm != "sha256"
            || format!("{:x}", hash.finalize()) != blob.digest
            || total != blob.len
        {
            return Err(Error::new("DigestMismatch", "frozen source"));
        }
        Ok(Self {
            blob: blob.clone(),
            blocks,
        })
    }
    pub fn range(&self, o: &Objects, start: u64, end: u64, s: &mut ReadStats) -> Result<Vec<u8>> {
        if start > end || end > self.blob.len || end - start > 8 * 1024 * 1024 {
            return Err(Error::new(
                "SourceRange",
                "invalid or oversized evidence range",
            ));
        }
        let mut out = Vec::new();
        if start == end {
            return Ok(out);
        }
        for i in start / BLOCK as u64..=(end - 1) / BLOCK as u64 {
            let v = tree::get(o, &self.blocks, &i.to_string(), s)?
                .ok_or_else(|| Error::new("MissingSourceBlock", i.to_string()))?;
            let r: ObjectRef = serde_json::from_value(v)?;
            let b = o.read(&r, s)?;
            let lo = start.saturating_sub(i * BLOCK as u64) as usize;
            let hi = ((end - i * BLOCK as u64) as usize).min(b.len());
            if lo > hi {
                return Err(Error::new("SourceRange", "block directory bounds"));
            }
            out.extend_from_slice(&b[lo..hi]);
        }
        if out.len() as u64 != end - start {
            return Err(Error::new("MissingSourceBlock", "short range"));
        }
        Ok(out)
    }
    pub fn audit(&self, o: &Objects) -> Result<()> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let mut total = 0;
        while total < self.blob.len {
            let end = (total + BLOCK as u64).min(self.blob.len);
            h.update(self.range(o, total, end, &mut ReadStats::default())?);
            total = end;
        }
        if format!("{:x}", h.finalize()) != self.blob.digest {
            return Err(Error::new("DigestMismatch", "source root"));
        }
        Ok(())
    }
}
