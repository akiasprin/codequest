//! Gather bounded requests into physical windows, then decode on the shared
//! pool. Logical result order is preserved without seeking once per fact.
use cq_core::{Error, Result};
use std::fs::File;

#[derive(Clone, Copy)]
pub(crate) struct Span {
    pub offset: u64,
    pub bytes: u64,
}
pub(crate) const BATCH_BYTES: u64 = 8 * 1024 * 1024;
const GAP: u64 = 4096;

pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buffer, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buffer.len() {
            let n = file.seek_read(&mut buffer[done..], offset + done as u64)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
}
pub(crate) fn decode<T: Send>(
    file: &File,
    spans: &[Span],
    decode: impl Fn(usize, &[u8]) -> Result<T> + Send + Sync,
) -> Result<Vec<T>> {
    let mut order = (0..spans.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|i| spans[*i].offset);
    let mut windows = Vec::new();
    let mut locations = vec![(0usize, 0usize, 0usize); spans.len()];
    let mut at = 0;
    while at < order.len() {
        let start = spans[order[at]].offset;
        let mut end = start;
        let first = at;
        while at < order.len() {
            let s = spans[order[at]];
            let next = s
                .offset
                .checked_add(s.bytes)
                .ok_or_else(|| Error::new("InvalidSpoolRange", "offset overflow"))?;
            if at > first && (s.offset.saturating_sub(end) > GAP || next - start > BATCH_BYTES) {
                break;
            }
            end = end.max(next);
            locations[order[at]] = (windows.len(), (s.offset - start) as usize, s.bytes as usize);
            at += 1;
        }
        let mut bytes =
            vec![
                0;
                usize::try_from(end - start)
                    .map_err(|_| Error::new("InvalidSpoolRange", "range exceeds address space"))?
            ];
        read_at(file, &mut bytes, start)?;
        windows.push(bytes);
    }
    cq_core::compute::map_records(
        locations.into_iter().enumerate().collect(),
        |(i, (window, offset, length))| decode(i, &windows[window][offset..offset + length]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn scattered_overlapping_and_repeated_ranges_preserve_request_order() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abcdefghij").unwrap();
        let spans = [
            Span {
                offset: 8,
                bytes: 2,
            },
            Span {
                offset: 0,
                bytes: 3,
            },
            Span {
                offset: 1,
                bytes: 4,
            },
            Span {
                offset: 8,
                bytes: 2,
            },
        ];
        assert_eq!(
            decode(&file, &spans, |_, b| Ok(b.to_vec())).unwrap(),
            [
                b"ij".to_vec(),
                b"abc".to_vec(),
                b"bcde".to_vec(),
                b"ij".to_vec()
            ]
        );
        assert!(
            decode(
                &file,
                &[Span {
                    offset: 9,
                    bytes: 2
                }],
                |_, b| Ok(b.len())
            )
            .is_err()
        );
        assert!(
            decode(
                &file,
                &[Span {
                    offset: u64::MAX,
                    bytes: 2
                }],
                |_, b| Ok(b.len())
            )
            .is_err()
        );
        assert_eq!(
            decode(&file, &spans, |i, _| -> Result<()> {
                if i == 2 {
                    Err(Error::new("Injected", "bad payload"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err()
            .code(),
            "Injected"
        );
    }
}
