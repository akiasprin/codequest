//! Import deduplication scans the native ID column once, not once per batch.
//! Only SHA-256 identities are retained. Larger inventories spill sorted runs.
use super::err;
use cq_core::Result;
use lbug::{Connection, Value};
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::File,
    io::{Read, Write},
};
const ROWS: usize = 1024 * 1024;
pub(super) fn key(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(err("invalid fact identity"));
    }
    let mut k = [0; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(err)?;
    }
    Ok(k)
}
pub(super) enum Membership {
    Memory(Vec<[u8; 32]>),
    Disk {
        _dir: tempfile::TempDir,
        _file: File,
        map: memmap2::Mmap,
        count: u64,
    },
}
impl Membership {
    pub fn load(c: &Connection<'_>) -> Result<Self> {
        Self::from_keys(
            c.query("MATCH (f:Fact) RETURN f.id")
                .map_err(err)?
                .map(|row| match row.into_iter().next() {
                    Some(Value::String(v)) => key(&v),
                    _ => Err(err("invalid native identity column")),
                }),
            ROWS,
        )
    }
    fn from_keys(values: impl Iterator<Item = Result<[u8; 32]>>, limit: usize) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let mut rows = Vec::new();
        let mut paths = vec![];
        for value in values {
            rows.push(value?);
            if rows.len() == limit {
                cq_core::compute::sort_by(&mut rows, Ord::cmp)?;
                rows.dedup();
                let path = dir.path().join(paths.len().to_string());
                let mut f = std::io::BufWriter::new(File::create(&path)?);
                for k in &rows {
                    f.write_all(k)?;
                }
                f.flush()?;
                paths.push(path);
                rows.clear();
            }
        }
        if paths.is_empty() {
            cq_core::compute::sort_by(&mut rows, Ord::cmp)?;
            rows.dedup();
            return Ok(Self::Memory(rows));
        }
        if !rows.is_empty() {
            cq_core::compute::sort_by(&mut rows, Ord::cmp)?;
            rows.dedup();
            let p = dir.path().join(paths.len().to_string());
            let mut f = std::io::BufWriter::new(File::create(&p)?);
            for k in rows {
                f.write_all(&k)?;
            }
            f.flush()?;
            paths.push(p);
        }
        let mut inputs = paths
            .iter()
            .map(|p| File::open(p).map(std::io::BufReader::new))
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut heap = BinaryHeap::new();
        for (i, f) in inputs.iter_mut().enumerate() {
            let mut k = [0; 32];
            f.read_exact(&mut k)?;
            heap.push(Reverse((k, i)));
        }
        let path = dir.path().join("sorted");
        let mut output = std::io::BufWriter::new(File::create(&path)?);
        let mut previous = None;
        let mut count = 0;
        while let Some(Reverse((k, i))) = heap.pop() {
            if previous != Some(k) {
                output.write_all(&k)?;
                count += 1;
                previous = Some(k);
            }
            let mut next = [0; 32];
            match inputs[i].read_exact(&mut next) {
                Ok(()) => heap.push(Reverse((next, i))),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                Err(e) => return Err(e.into()),
            }
        }
        output.flush()?;
        drop(output);
        let file = File::open(path)?;
        // SAFETY: this private temporary inventory is immutable after the only
        // writer is closed above. No writable handle/path escapes Membership;
        // the mapping and owning file live until this inventory is dropped.
        let map = unsafe { memmap2::MmapOptions::new().map(&file)? };
        Ok(Self::Disk {
            _dir: dir,
            _file: file,
            map,
            count,
        })
    }
    pub fn contains(&self, k: &[u8; 32]) -> Result<bool> {
        match self {
            Self::Memory(v) => Ok(v.binary_search(k).is_ok()),
            Self::Disk { map, count, .. } => {
                let (mut lo, mut hi) = (0, *count);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let b: &[u8; 32] = map
                        .get((mid * 32) as usize..(mid * 32 + 32) as usize)
                        .and_then(|b| b.try_into().ok())
                        .ok_or_else(|| err("invalid membership range"))?;
                    match b.cmp(k) {
                        std::cmp::Ordering::Less => lo = mid + 1,
                        std::cmp::Ordering::Greater => hi = mid,
                        std::cmp::Ordering::Equal => return Ok(true),
                    }
                }
                Ok(false)
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_and_spilled_membership_have_the_same_positive_and_negative_answers() {
        let rows = (0..37).rev().map(|i| {
            let mut k = [0; 32];
            k[0] = i;
            Ok(k)
        });
        for limit in [3, 100] {
            let m = Membership::from_keys(rows.clone(), limit).unwrap();
            for i in 0..50 {
                let mut k = [0; 32];
                k[0] = i;
                assert_eq!(m.contains(&k).unwrap(), i < 37);
            }
        }
    }
}
