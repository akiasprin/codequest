//! Native page locators bind the index to canonical immutable payloads. Large
//! distinct string bodies can exhaust checkpoint memory despite bounded import
//! transactions. Earlier inline/segmented projections remain readable.
use super::err;
use crate::facts::objects::Objects;
use cq_core::{
    Error, Result,
    model::digest,
    ports::facts::{ObjectRef, ReadStats},
};
use lbug::{Connection, Value};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Instant};

pub(super) const SEGMENT_BYTES: usize = 32 * 1024;
const CODEC: &str = "codequest-native-page-segments-v1";
const LOCATOR_CODEC: &str = "codequest-canonical-page-locator-v1";
const MAX_PARTS: usize = 512;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    codec: String,
    bytes: u64,
    parts: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Locator {
    codec: String,
    hash: String,
    bytes: u64,
}
pub(super) fn is_locator(text: &str, r: &ObjectRef) -> Result<bool> {
    // A legacy canonical payload may itself be a JSON codec marker.
    if text.len() as u64 == r.bytes && digest(text.as_bytes()) == r.hash {
        return Ok(false);
    }
    if let Ok(locator) = serde_json::from_str::<Locator>(text) {
        if locator.codec != LOCATOR_CODEC || locator.hash != r.hash || locator.bytes != r.bytes {
            return Err(invalid());
        }
        return Ok(true);
    }
    Ok(false)
}

/// Native storage is an index; immutable canonical files own the payload.
/// Keeping only the bound locator also avoids native checkpoint dictionaries
/// growing with the aggregate size of distinct page bodies.
pub(super) fn store_locator(c: &Connection<'_>, r: &ObjectRef) -> Result<()> {
    let text = serde_json::to_string(&Locator {
        codec: LOCATOR_CODEC.into(),
        hash: r.hash.clone(),
        bytes: r.bytes,
    })?;
    let mut statement = c
        .prepare("CREATE (:Page {id:$id, bytes:$bytes})")
        .map_err(err)?;
    c.execute(
        &mut statement,
        vec![
            ("id", Value::String(r.hash.clone())),
            ("bytes", Value::String(text)),
        ],
    )
    .map_err(err)?;
    Ok(())
}
fn invalid() -> Error {
    Error::new("IndexIncomplete", "native page segments or checksum")
}

/// Called inside the page transaction; the page marker is inserted last.
#[cfg(test)]
fn store(c: &Connection<'_>, r: &ObjectRef, text: &str) -> Result<u64> {
    let mut statement = c
        .prepare("CREATE (:Page {id:$id, bytes:$bytes})")
        .map_err(err)?;
    let mut insert = |id: String, text: String| -> Result<()> {
        c.execute(
            &mut statement,
            vec![("id", Value::String(id)), ("bytes", Value::String(text))],
        )
        .map_err(err)?;
        Ok(())
    };
    if text.len() <= SEGMENT_BYTES {
        insert(r.hash.clone(), text.into())?;
        return Ok(0);
    }
    let mut position = 0;
    let mut part = 0;
    while position < text.len() {
        let mut end = (position + SEGMENT_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        insert(format!("{}:{part}", r.hash), text[position..end].into())?;
        position = end;
        part += 1;
    }
    insert(
        r.hash.clone(),
        serde_json::to_string(&Manifest {
            codec: CODEC.into(),
            bytes: r.bytes,
            parts: part,
        })?,
    )?;
    Ok(part as u64)
}

pub(super) fn load(
    c: &Connection<'_>,
    objects: &Objects,
    r: &ObjectRef,
    text: &str,
    started: Instant,
    timeout: u64,
    io: &mut ReadStats,
) -> Result<(Vec<u8>, u64)> {
    // Digest first: an old canonical page can itself look like a codec marker.
    if text.len() as u64 == r.bytes && digest(text.as_bytes()) == r.hash {
        return Ok((text.as_bytes().to_vec(), 0));
    }
    if let Ok(locator) = serde_json::from_str::<Locator>(text) {
        if locator.codec != LOCATOR_CODEC || locator.hash != r.hash || locator.bytes != r.bytes {
            return Err(invalid());
        }
        return Ok((objects.read(r, io)?.as_ref().clone(), 0));
    }
    let manifest: Manifest = serde_json::from_str(text).map_err(|_| invalid())?;
    if manifest.codec != CODEC
        || manifest.bytes != r.bytes
        || manifest.parts == 0
        || manifest.parts > MAX_PARTS
        || r.bytes > crate::facts::objects::MAX_OBJECT as u64
    {
        return Err(invalid());
    }
    let remaining = timeout.saturating_sub(started.elapsed().as_millis() as u64);
    if remaining == 0 {
        return Err(Error::new("QueryTimeout", "native page request budget"));
    }
    c.set_query_timeout(remaining);
    let prefix = format!("{}:", r.hash);
    let ids = (0..manifest.parts)
        .map(|i| format!("{prefix}{i}"))
        .collect::<Vec<_>>();
    let query = format!(
        "UNWIND {} AS key MATCH (p:Page {{id:key}}) RETURN p.id,p.bytes",
        serde_json::to_string(&ids)?
    );
    let rows = c.query(&query).map_err(err)?;
    let mut pieces = BTreeMap::new();
    let mut bytes = 0;
    for row in rows {
        let [Value::String(id), Value::String(piece)] = row.as_slice() else {
            return Err(invalid());
        };
        let index: usize = id
            .strip_prefix(&prefix)
            .and_then(|s| s.parse().ok())
            .ok_or_else(invalid)?;
        bytes += piece.len();
        io.bytes_decoded += piece.len() as u64;
        if index >= manifest.parts
            || piece.is_empty()
            || piece.len() > SEGMENT_BYTES
            || bytes as u64 > r.bytes
            || pieces.insert(index, piece.clone()).is_some()
        {
            return Err(invalid());
        }
    }
    if pieces.len() != manifest.parts || bytes as u64 != r.bytes {
        return Err(invalid());
    }
    let mut result = Vec::with_capacity(bytes);
    for piece in pieces.into_values() {
        result.extend_from_slice(piece.as_bytes());
    }
    if digest(&result) != r.hash {
        return Err(invalid());
    }
    Ok((result, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladybug::{Projection, quote};
    #[test]
    fn native_page_segments_roundtrip_unicode_reject_damage_and_read_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let p = Projection::open(&dir.path().join("graph.lbdb"), true).unwrap();
        let c = p.connection().unwrap();
        let text = "设计意图abc\n".repeat(18000);
        let r = ObjectRef {
            hash: digest(text.as_bytes()),
            bytes: text.len() as u64,
        };
        c.query("BEGIN TRANSACTION").unwrap();
        let parts = store(&c, &r, &text).unwrap();
        assert!(parts > 1);
        c.query("COMMIT").unwrap();
        for row in p.rows("MATCH (p:Page) RETURN p.bytes").unwrap() {
            let Value::String(value) = &row[0] else {
                panic!("page must be text");
            };
            assert!(value.len() <= SEGMENT_BYTES);
        }
        let objects = Objects::new(dir.path().join("objects")).unwrap();
        let read = |r: &ObjectRef| p.page(&objects, r, 10000, &mut ReadStats::default());
        let (bytes, queries) = read(&r).unwrap();
        assert_eq!(bytes.as_slice(), text.as_bytes());
        assert_eq!(queries, 2);
        assert_eq!(read(&r).unwrap().1, 0);
        let original = p
            .rows(&format!(
                "MATCH (p:Page {{id:{}}}) RETURN p.bytes",
                quote(&format!("{}:0", r.hash))
            ))
            .unwrap();
        let Value::String(original) = &original[0][0] else {
            panic!("segment must be text");
        };
        p.cache.lock().unwrap().clear();
        c.query(&format!(
            "MATCH (p:Page {{id:{}}}) SET p.bytes='damaged'",
            quote(&format!("{}:0", r.hash))
        ))
        .unwrap();
        assert_eq!(read(&r).unwrap_err().code(), "IndexIncomplete");
        c.query(&format!(
            "MATCH (p:Page {{id:{}}}) SET p.bytes={}",
            quote(&format!("{}:0", r.hash)),
            quote(original)
        ))
        .unwrap();
        assert_eq!(read(&r).unwrap().0.as_slice(), text.as_bytes());
        p.cache.lock().unwrap().clear();
        c.query(&format!(
            "MATCH (p:Page {{id:{}}}) DELETE p",
            quote(&format!("{}:1", r.hash))
        ))
        .unwrap();
        assert!(read(&r).is_err());

        let legacy = r#"{"codec":"codequest-native-page-segments-v1","bytes":524288,"parts":16}"#;
        let r = ObjectRef {
            hash: digest(legacy.as_bytes()),
            bytes: legacy.len() as u64,
        };
        c.query(&format!(
            "CREATE (:Page {{id:{},bytes:{}}})",
            quote(&r.hash),
            quote(legacy)
        ))
        .unwrap();
        assert_eq!(read(&r).unwrap().0.as_slice(), legacy.as_bytes());
    }

    #[test]
    fn native_locator_binds_canonical_payload_and_reports_actual_file_reads() {
        let dir = tempfile::tempdir().unwrap();
        let p = Projection::open(&dir.path().join("graph.lbdb"), true).unwrap();
        let objects = Objects::new(dir.path().join("objects")).unwrap();
        let bytes = "不依赖原始工作区的冻结事实\n".repeat(16000).into_bytes();
        let r = objects.put(&bytes).unwrap();
        let c = p.connection().unwrap();
        store_locator(&c, &r).unwrap();
        let rows = p.rows("MATCH (p:Page) RETURN p.bytes").unwrap();
        let Value::String(locator) = &rows[0][0] else {
            panic!("locator must be text");
        };
        assert!(locator.len() < 256);
        let mut io = ReadStats::default();
        assert_eq!(
            p.page(&objects, &r, 10000, &mut io).unwrap().0.as_slice(),
            bytes
        );
        assert!(io.page_reads > 0);
        p.cache.lock().unwrap().clear();
        objects.clear_cache();
        std::fs::write(objects.path(&r).unwrap(), b"damaged canonical payload").unwrap();
        assert_eq!(
            p.page(&objects, &r, 10000, &mut ReadStats::default())
                .unwrap_err()
                .code(),
            "DigestMismatch"
        );
        std::fs::remove_file(objects.path(&r).unwrap()).unwrap();
        assert_eq!(
            p.page(&objects, &r, 10000, &mut ReadStats::default())
                .unwrap_err()
                .code(),
            "MissingFactPage"
        );
    }
}
