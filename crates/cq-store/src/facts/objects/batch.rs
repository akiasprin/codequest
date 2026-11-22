//! Bounded audit reads: resolve hash-ordered locators, then coalesce payloads by
//! physical file/offset. No payload cache and no reader lock spans payload I/O.
use super::*;
use crate::facts::packed::ReadPlan;
use std::os::unix::fs::FileExt;

pub(crate) const MAX_BATCH_BYTES: u64 = 8 * 1024 * 1024;
const GAP: u64 = 4096;
#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct BatchReadStats {
    pub objects: u64,
    pub segments: u64,
    pub read_calls: u64,
    pub read_bytes: u64,
    pub requested_bytes: u64,
}
impl BatchReadStats {
    pub fn add(&mut self, b: Self) {
        self.objects += b.objects;
        self.segments += b.segments;
        self.read_calls += b.read_calls;
        self.read_bytes += b.read_bytes;
        self.requested_bytes += b.requested_bytes;
    }
}
impl Objects {
    pub(crate) fn audit_batch<T: Send>(
        &self,
        refs: &[ObjectRef],
        decode: impl Fn(usize, &[u8]) -> Result<T> + Send + Sync,
    ) -> Result<(Vec<T>, BatchReadStats)> {
        if refs
            .iter()
            .try_fold(0u64, |n, r| n.checked_add(r.bytes))
            .is_none_or(|n| n > MAX_BATCH_BYTES)
        {
            return Err(Error::new("AuditReadBudget", "audit batch exceeds 8 MiB"));
        }
        let mut shards = (0..16).map(|_| Vec::new()).collect::<Vec<_>>();
        for (i, r) in refs.iter().enumerate() {
            self.trace_ref(r);
            self.path(r)?;
            shards[shard(&r.hash)].push((i, r));
        }
        let located =
            cq_core::compute::map(shards.into_iter().enumerate().collect(), |(s, mut rows)| {
                rows.sort_unstable_by(|a, b| a.1.hash.cmp(&b.1.hash));
                let mut reader = self.shared.readers[s].lock().unwrap();
                rows.into_iter()
                    .map(|(i, r)| {
                        let loose = self.path(r)?;
                        let plan = if loose.is_file() {
                            ReadPlan {
                                path: loose,
                                offset: 0,
                                bytes: r.bytes,
                                whole_file: true,
                            }
                        } else {
                            reader
                                .plan(&self.root, r)?
                                .ok_or_else(|| Error::new("MissingFactPage", &r.hash))?
                        };
                        Ok((i, plan))
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
        let mut files = BTreeMap::<PathBuf, Vec<(usize, ReadPlan)>>::new();
        for (i, p) in located.into_iter().flatten() {
            files.entry(p.path.clone()).or_default().push((i, p));
        }
        let results = cq_core::compute::map(files.into_iter().collect(), |(path, mut rows)| {
            rows.sort_unstable_by_key(|(_, p)| p.offset);
            let file = File::open(path)?;
            if rows.iter().any(|(_, p)| {
                p.whole_file && file.metadata().map(|m| m.len() != p.bytes).unwrap_or(true)
            }) {
                return Err(Error::new("DigestMismatch", "loose object length"));
            }
            let mut output = Vec::with_capacity(rows.len());
            let mut stats = BatchReadStats {
                segments: 1,
                ..Default::default()
            };
            let mut at = 0;
            while at < rows.len() {
                let first = at;
                let start = rows[at].1.offset;
                let mut end = start;
                while at < rows.len() {
                    let p = &rows[at].1;
                    let next = p
                        .offset
                        .checked_add(p.bytes)
                        .ok_or_else(|| Error::new("InvalidPackIndex", "offset overflow"))?;
                    if at > first
                        && (p.offset.saturating_sub(end) > GAP || next - start > MAX_BATCH_BYTES)
                    {
                        break;
                    }
                    end = end.max(next);
                    at += 1;
                }
                let mut bytes = vec![0; (end - start) as usize];
                file.read_exact_at(&mut bytes, start)?;
                stats.read_calls += 1;
                stats.read_bytes += bytes.len() as u64;
                let values =
                    cq_core::compute::map_records(rows[first..at].iter().collect(), |(i, p)| {
                        let slice = &bytes
                            [(p.offset - start) as usize..(p.offset - start + p.bytes) as usize];
                        if digest(slice) != refs[*i].hash {
                            return Err(Error::new("DigestMismatch", &refs[*i].hash));
                        }
                        Ok((*i, decode(*i, slice)?))
                    })?;
                stats.objects += values.len() as u64;
                stats.requested_bytes += rows[first..at].iter().map(|(_, p)| p.bytes).sum::<u64>();
                output.extend(values);
            }
            Ok((output, stats))
        })?;
        let mut ordered = std::iter::repeat_with(|| None)
            .take(refs.len())
            .collect::<Vec<_>>();
        let mut stats = BatchReadStats::default();
        for (values, s) in results {
            stats.add(s);
            for (i, v) in values {
                ordered[i] = Some(v);
            }
        }
        Ok((ordered.into_iter().map(Option::unwrap).collect(), stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_batches_preserve_request_order_and_bypass_cached_damage() {
        let temp = tempfile::tempdir().unwrap();
        let packed = Objects::buffered(temp.path().join("objects")).unwrap();
        let mut refs = (0..2048)
            .map(|i| {
                packed
                    .put(format!("frozen-evidence-{i:05}").as_bytes())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        packed.sync_pending().unwrap();
        let objects = Objects::new(packed.root.clone()).unwrap();
        refs.reverse();
        let (values, stats) = objects.audit_batch(&refs, |_, b| Ok(b.to_vec())).unwrap();
        for (i, value) in values.iter().enumerate() {
            assert_eq!(
                value,
                &format!("frozen-evidence-{:05}", 2047 - i).into_bytes()
            );
        }
        assert!(
            stats.read_calls * 4 < stats.objects,
            "coalesce adjacent objects, not one read/open per hash"
        );
        assert_eq!(stats.objects, 2048);
        let loose = objects.put(b"loose mixed evidence").unwrap();
        let mixed = [refs[3].clone(), loose.clone(), refs[3].clone()];
        let (values, _) = objects.audit_batch(&mixed, |_, b| Ok(b.to_vec())).unwrap();
        assert_eq!(values[0], values[2]);
        assert_eq!(values[1], b"loose mixed evidence");
        objects.read(&loose, &mut ReadStats::default()).unwrap();
        std::fs::write(objects.path(&loose).unwrap(), b"loose mixed evidence EXTRA").unwrap();
        assert_eq!(
            objects
                .audit_batch(&[loose], |_, b| Ok(b.len()))
                .unwrap_err()
                .code(),
            "DigestMismatch"
        );
        let mut wrong = refs[0].clone();
        wrong.bytes += 1;
        assert_eq!(
            objects
                .audit_batch(&[wrong], |_, b| Ok(b.len()))
                .unwrap_err()
                .code(),
            "DigestMismatch"
        );
    }
    #[test]
    fn oversized_batch_and_truncated_locator_are_not_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let packed = Objects::buffered(temp.path().join("objects")).unwrap();
        let r = packed.put(b"payload").unwrap();
        packed.sync_pending().unwrap();
        let objects = Objects::new(packed.root.clone()).unwrap();
        let mut large = r.clone();
        large.bytes = MAX_BATCH_BYTES;
        assert_eq!(
            objects
                .audit_batch(&[large.clone(), large], |_, _| Ok(()))
                .unwrap_err()
                .code(),
            "AuditReadBudget"
        );
        objects
            .audit_batch(std::slice::from_ref(&r), |_, _| Ok(()))
            .unwrap();
        // The locator's cached block must be invalidated after replacement.
        let idx = objects
            .root
            .join("pack-index")
            .join(format!("{}.idx", &r.hash[..2]));
        std::fs::write(idx, b"truncated").unwrap();
        assert!(objects.audit_batch(&[r], |_, _| Ok(())).is_err());
    }
}
