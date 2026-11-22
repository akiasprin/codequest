use cq_core::{
    model::digest,
    ports::facts::{FactRef, ReadStats},
};
use cq_store::facts::objects::Objects;
use std::io::{Seek, SeekFrom, Write};

#[test]
fn authenticated_records_read_only_the_requested_range_in_loose_and_packed_objects() {
    for packed in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let objects = if packed {
            Objects::buffered(tmp.path().join("objects")).unwrap()
        } else {
            Objects::new(tmp.path().join("objects")).unwrap()
        };
        let record = br#"{"id":"needle","name":"stamp_rule_tags"}"#;
        let mut bytes = vec![b' '; 4 * 1024 * 1024];
        let offset = 1000;
        bytes[offset..offset + record.len()].copy_from_slice(record);
        let object = objects.put(&bytes).unwrap();
        objects.sync_pending().unwrap();
        let reopened = Objects::new(objects.directory().to_owned()).unwrap();
        let r = FactRef {
            object: object.clone(),
            offset: offset as u64,
            length: record.len() as u64,
            digest: digest(record),
        };
        let mut stats = ReadStats::default();
        assert_eq!(reopened.read_record(&r, &mut stats).unwrap(), record);
        assert_eq!(stats.bytes_decoded, record.len() as u64);
        let mut invalid = r.clone();
        invalid.offset = u64::MAX;
        assert_eq!(
            reopened
                .read_record(&invalid, &mut stats)
                .unwrap_err()
                .code(),
            "InvalidFactRef"
        );
        let mut invalid = r.clone();
        invalid.digest = "0".repeat(64);
        assert_eq!(
            reopened
                .read_record(&invalid, &mut stats)
                .unwrap_err()
                .code(),
            "DigestMismatch"
        );
        // Damage in an unqueried sibling is caught by full audit, while a valid
        // authenticated record remains readable. No aggregate audit is skipped.
        let (path, start) = if packed {
            let p = std::fs::read_dir(objects.directory().join("packs"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            (p, 0u64)
        } else {
            (objects.path(&object).unwrap(), 0)
        };
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(start + 500)).unwrap();
        f.write_all(b"!").unwrap();
        f.sync_all().unwrap();
        assert_eq!(
            reopened.read_record(&r, &mut ReadStats::default()).unwrap(),
            record
        );
        assert_eq!(
            reopened.audit(&object).unwrap_err().code(),
            "DigestMismatch"
        );
        f.seek(SeekFrom::Start(start + offset as u64)).unwrap();
        f.write_all(b"!").unwrap();
        f.sync_all().unwrap();
        assert_eq!(
            reopened
                .read_record(&r, &mut ReadStats::default())
                .unwrap_err()
                .code(),
            "DigestMismatch"
        );
    }
}
