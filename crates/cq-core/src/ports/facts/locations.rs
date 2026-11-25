//! Request-local, bounded line indices and coalesced frozen-source reads.
use super::SourcePosition;
use crate::Result;
use std::collections::HashMap;
const CHUNK: u64 = 64 * 1024;
const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct Lines {
    bytes: Vec<u8>,
    starts: Vec<u32>,
    used: u64,
}
impl Lines {
    fn size(&self) -> usize {
        self.bytes.capacity() + self.starts.capacity() * 4
    }
    fn append(&mut self, data: &[u8]) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        let begin = self.bytes.len();
        self.starts.extend(
            data.iter()
                .enumerate()
                .filter_map(|(i, &b)| (b == b'\n').then_some((begin + i + 1) as u32)),
        );
        self.bytes.extend_from_slice(data);
    }
    fn at(&self, offset: usize) -> SourcePosition {
        let index = self
            .starts
            .partition_point(|&s| s as usize <= offset)
            .saturating_sub(1);
        let start = self.starts[index] as usize;
        let end = self
            .starts
            .get(index + 1)
            .map(|&n| n as usize)
            .unwrap_or(self.bytes.len());
        (
            index + 1,
            offset - start + 1,
            String::from_utf8_lossy(&self.bytes[start..end])
                .trim()
                .chars()
                .take(160)
                .collect(),
        )
    }
}
#[derive(Default)]
pub(crate) struct SourceLocations {
    entries: HashMap<String, Lines>,
    bytes: usize,
    clock: u64,
}
impl SourceLocations {
    pub(crate) fn lookup(
        &mut self,
        file: &str,
        offset: u64,
        len: u64,
        mut read: impl FnMut(u64, u64) -> Result<Vec<u8>>,
    ) -> Result<Option<SourcePosition>> {
        if offset > len || offset >= LIMIT {
            return Ok(None);
        }
        let required = offset.saturating_add(512).min(len).min(LIMIT);
        let end = required
            .div_ceil(CHUNK)
            .saturating_mul(CHUNK)
            .min(len)
            .min(LIMIT);
        self.clock += 1;
        let mut entry = self.entries.remove(file).unwrap_or_default();
        self.bytes -= entry.size();
        let begin = entry.bytes.len() as u64;
        if begin < end {
            // Extend only the unread suffix. Multiple positions in one source
            // chunk share both the authenticated read and newline scan.
            let bytes = read(begin, end)?;
            if bytes.len() as u64 != end - begin {
                return Err(crate::Error::new(
                    "MissingSourceBlock",
                    "short source prefix",
                ));
            }
            entry.append(&bytes);
        } else if entry.starts.is_empty() {
            entry.append(&[]);
        }
        let position = entry.at(offset as usize);
        entry.used = self.clock;
        let size = entry.size();
        if size <= LIMIT as usize {
            while self.bytes + size > LIMIT as usize {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.used)
                    .map(|(k, _)| k.clone());
                let Some(oldest) = oldest else {
                    break;
                };
                self.bytes -= self.entries.remove(&oldest).unwrap().size();
            }
            self.bytes += size;
            self.entries.insert(file.into(), entry);
        }
        Ok(Some(position))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn evicts_old_prefixes_and_does_not_cache_failed_reads() {
        let mut cache = SourceLocations::default();
        for i in 0..9 {
            cache
                .lookup(&i.to_string(), 1_000_000, 1_048_576, |a, b| {
                    Ok(vec![b'x'; (b - a) as usize])
                })
                .unwrap();
            assert!(cache.bytes <= LIMIT as usize);
        }
        assert!(!cache.entries.contains_key("0"));
        let mut reloaded = false;
        cache
            .lookup("0", 1_000_000, 1_048_576, |a, b| {
                reloaded = true;
                Ok(vec![b'x'; (b - a) as usize])
            })
            .unwrap();
        assert!(reloaded);
        assert!(
            cache
                .lookup("bad", 10, 100, |_, _| Err(crate::Error::new(
                    "DigestMismatch",
                    "test evidence"
                )))
                .is_err()
        );
        assert!(!cache.entries.contains_key("bad"));
        assert!(cache.lookup("short", 10, 100, |_, _| Ok(vec![])).is_err());
        assert!(!cache.entries.contains_key("short"));
    }
    #[test]
    fn coalesces_ranges_and_handles_unicode_crlf_eof_and_file_versions() {
        let data = "第一行\r\nsecond\n".repeat(10000).into_bytes();
        let mut cache = SourceLocations::default();
        let mut reads = vec![];
        for offset in [0, 11, 12, 70000, 2, data.len()] {
            let got = cache
                .lookup("rev-a", offset as u64, data.len() as u64, |a, b| {
                    reads.push((a, b));
                    Ok(data[a as usize..b as usize].to_vec())
                })
                .unwrap()
                .unwrap();
            let line = data[..offset].iter().filter(|&&b| b == b'\n').count() + 1;
            let start = data[..offset]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |n| n + 1);
            assert_eq!((got.0, got.1), (line, offset - start + 1));
        }
        assert!(reads.windows(2).all(|r| r[0].1 == r[1].0), "{reads:?}");
        let got = cache
            .lookup("rev-b", 1, 3, |_, _| Ok(b"x\ny".to_vec()))
            .unwrap()
            .unwrap();
        assert_eq!((got.0, got.1), (1, 2));
        assert!(
            cache
                .lookup("a", LIMIT, LIMIT + 1, |_, _| panic!())
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .lookup("empty", 0, 0, |_, _| panic!())
                .unwrap()
                .is_some()
        );
    }
}
