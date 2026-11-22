//! One page identity per inventory bucket, binary record digests. Keeping an
//! ObjectRef and two heap-allocated digest strings per fact costs gigabytes.
use cq_core::{
    Error, Result,
    ports::facts::{FactRef, ObjectRef},
};
use std::collections::BTreeMap;
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Row<T> {
    key: String,
    offset: u64,
    length: u64,
    digest: [u8; 32],
    extra: T,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Page<T> {
    pub object: ObjectRef,
    pub rows: Vec<Row<T>>,
}
pub(crate) fn push<T>(
    pages: &mut BTreeMap<String, Page<T>>,
    key: &str,
    reference: FactRef,
    extra: T,
) -> Result<()> {
    if !super::objects::valid_hash(&reference.digest) {
        return Err(Error::new("DigestMismatch", "invalid record digest"));
    }
    let mut digest = [0; 32];
    for (i, b) in digest.iter_mut().enumerate() {
        *b = u8::from_str_radix(&reference.digest[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::new("DigestMismatch", "record digest"))?;
    }
    let page = pages
        .entry(reference.object.hash.clone())
        .or_insert_with(|| Page {
            object: reference.object.clone(),
            rows: Vec::new(),
        });
    if page.object.bytes != reference.object.bytes {
        return Err(Error::new("DigestMismatch", "conflicting page lengths"));
    }
    page.rows.push(Row {
        key: key.into(),
        offset: reference.offset,
        length: reference.length,
        digest,
        extra,
    });
    Ok(())
}
impl<T> Row<T> {
    pub fn expand(self, object: &ObjectRef) -> (String, FactRef, T) {
        let mut digest = String::with_capacity(64);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for b in self.digest {
            digest.push(HEX[(b >> 4) as usize] as char);
            digest.push(HEX[(b & 15) as usize] as char);
        }
        (
            self.key,
            FactRef {
                object: object.clone(),
                offset: self.offset,
                length: self.length,
                digest,
            },
            self.extra,
        )
    }
}
