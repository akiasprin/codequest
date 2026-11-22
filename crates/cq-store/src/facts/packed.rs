//! Immutable payload segments and a fixed-width, hash-sorted locator per bucket.
//! Segment data is durable before an atomic index replacement makes it visible.
//! ObjectRef remains content-addressed; legacy loose objects remain readable.
use super::objects::{MAX_OBJECT, valid_hash};
use cq_core::{Error, Result, model::digest, ports::facts::ObjectRef};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt},
    },
    path::{Path, PathBuf},
};
const MAGIC: &[u8; 8] = b"CQPACK01";
const ENTRY: usize = 64;
const HEADER: u64 = 16;
const BATCH: usize = 16 * 1024;
const SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

// Keep sparse fences and an open descriptor for each immutable index version.
// A lookup reads one small contiguous block instead of issuing a pread at every
// binary-search level. File damage remains an I/O error, with no unsafe mapping.
const INDEX_BLOCK: usize = 128;
const INDEX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
struct Index {
    version: (u64, u64, u64, i64, i64, i64, i64),
    file: File,
    fences: Vec<[u8; 32]>,
    rows: u64,
    block: Vec<u8>,
    block_start: Option<usize>,
}
/// A locator is resolved under the reader lock; payload I/O happens afterwards.
pub(super) struct ReadPlan {
    pub path: PathBuf,
    pub offset: u64,
    pub bytes: u64,
    pub whole_file: bool,
}
impl ReadPlan {
    pub fn range(&self, start: u64, length: u64) -> Result<Vec<u8>> {
        if start.checked_add(length).is_none_or(|end| end > self.bytes) {
            return Err(Error::new("InvalidFactRef", "record range outside object"));
        }
        let file = File::open(&self.path)?;
        let size = file.metadata()?.len();
        if self
            .offset
            .checked_add(self.bytes)
            .is_none_or(|end| end > size)
            || self.whole_file && size != self.bytes
        {
            return Err(Error::new("DigestMismatch", "object extent"));
        }
        let mut bytes = vec![0; length as usize];
        file.read_exact_at(&mut bytes, self.offset + start)?;
        Ok(bytes)
    }
    pub fn read(&self) -> Result<Vec<u8>> {
        let file = File::open(&self.path)?;
        if self.whole_file && file.metadata()?.len() != self.bytes {
            return Err(Error::new("DigestMismatch", "loose object length"));
        }
        let mut bytes = vec![0; self.bytes as usize];
        file.read_exact_at(&mut bytes, self.offset)?;
        Ok(bytes)
    }
}
#[derive(Default)]
pub(super) struct Reader {
    indexes: BTreeMap<String, Index>,
    cached: u64,
    pub index_opens: u64,
    pub index_blocks: u64,
}
impl Reader {
    pub fn clear(&mut self) {
        self.indexes.clear();
        self.cached = 0;
    }
    pub fn cached_bytes(&self) -> u64 {
        self.cached
    }
    pub fn read(&mut self, root: &Path, r: &ObjectRef) -> Result<Option<Vec<u8>>> {
        self.plan(root, r)?.map(|p| p.read()).transpose()
    }
    pub fn plan(&mut self, root: &Path, r: &ObjectRef) -> Result<Option<ReadPlan>> {
        let prefix = &r.hash[..2];
        let path = index_path(root, prefix);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let version = |m: &std::fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if self
            .indexes
            .get(prefix)
            .is_none_or(|i| i.version != version(&meta))
        {
            if let Some(old) = self.indexes.remove(prefix) {
                self.cached -= old.fences.len() as u64 * 32;
            }
            let file = File::open(&path)?;
            self.index_opens += 1;
            let rows = header(&file)?;
            let size = rows.div_ceil(INDEX_BLOCK as u64) * 32;
            if size > INDEX_CACHE_BYTES {
                return lookup(root, &r.hash)?.map(|v| plan(root, r, v)).transpose();
            }
            while self.cached + size > INDEX_CACHE_BYTES {
                let Some((_, old)) = self.indexes.pop_first() else {
                    break;
                };
                self.cached -= old.fences.len() as u64 * 32;
            }
            let metadata = file.metadata()?;
            let mut input = BufReader::with_capacity(1024 * 1024, file.try_clone()?);
            input.seek(SeekFrom::Start(HEADER))?;
            let mut fences = Vec::with_capacity(size as usize / 32);
            let mut block = [0u8; INDEX_BLOCK * ENTRY];
            let mut done = 0;
            while done < rows {
                let count = (rows - done).min(INDEX_BLOCK as u64) as usize;
                input.read_exact(&mut block[..count * ENTRY])?;
                let key = block[..32].try_into().unwrap();
                if fences.last().is_some_and(|last| last >= &key) {
                    return Err(Error::new("InvalidPackIndex", "unsorted fences"));
                }
                fences.push(key);
                done += count as u64;
            }
            self.cached += size;
            self.indexes.insert(
                prefix.into(),
                Index {
                    version: version(&metadata),
                    file,
                    fences,
                    rows,
                    block: Vec::new(),
                    block_start: None,
                },
            );
        }
        let index = self.indexes.get_mut(prefix).unwrap();
        let key = binary::<32>(&r.hash)?;
        let upper = index.fences.partition_point(|f| f <= &key);
        if upper == 0 {
            return Ok(None);
        }
        let start = (upper - 1) * INDEX_BLOCK;
        let count = (index.rows - start as u64).min(INDEX_BLOCK as u64) as usize;
        if index.block_start != Some(start) {
            index.block.resize(count * ENTRY, 0);
            index
                .file
                .read_exact_at(&mut index.block, HEADER + start as u64 * ENTRY as u64)?;
            index.block_start = Some(start);
            self.index_blocks += 1;
        }
        let block = &index.block;
        let (mut lo, mut hi) = (0, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let offset = mid * ENTRY;
            let found: &[u8; 32] = block[offset..offset + 32].try_into().unwrap();
            match found.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let v = Locator::decode(block[offset..offset + ENTRY].try_into().unwrap())?;
                    return plan(root, r, v).map(Some);
                }
            }
        }
        Ok(None)
    }
}
fn plan(root: &Path, r: &ObjectRef, v: Locator) -> Result<ReadPlan> {
    if v.bytes != r.bytes {
        return Err(Error::new("DigestMismatch", "packed object length"));
    }
    Ok(ReadPlan {
        path: data_path(root, &v.segment),
        offset: v.offset,
        bytes: v.bytes,
        whole_file: false,
    })
}

#[derive(Clone)]
struct Locator {
    hash: [u8; 32],
    segment: [u8; 16],
    offset: u64,
    bytes: u64,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn binary<const N: usize>(s: &str) -> Result<[u8; N]> {
    if s.len() != N * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new("InvalidPackIndex", "hex width"));
    }
    let mut out = [0; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::new("InvalidPackIndex", "hex encoding"))?;
    }
    Ok(out)
}
impl Locator {
    fn encode(&self) -> [u8; ENTRY] {
        let mut b = [0; ENTRY];
        b[..32].copy_from_slice(&self.hash);
        b[32..48].copy_from_slice(&self.segment);
        b[48..56].copy_from_slice(&self.offset.to_le_bytes());
        b[56..].copy_from_slice(&self.bytes.to_le_bytes());
        b
    }
    fn decode(b: &[u8; ENTRY]) -> Result<Self> {
        let v = Self {
            hash: b[..32].try_into().unwrap(),
            segment: b[32..48].try_into().unwrap(),
            offset: u64::from_le_bytes(b[48..56].try_into().unwrap()),
            bytes: u64::from_le_bytes(b[56..].try_into().unwrap()),
        };
        if v.bytes > MAX_OBJECT as u64 || v.offset.checked_add(v.bytes).is_none() {
            return Err(Error::new("InvalidPackIndex", "payload bounds"));
        }
        Ok(v)
    }
    fn reference(&self) -> ObjectRef {
        ObjectRef {
            hash: hex(&self.hash),
            bytes: self.bytes,
        }
    }
}
fn index_path(root: &Path, prefix: &str) -> PathBuf {
    root.join("pack-index").join(format!("{prefix}.idx"))
}
fn data_path(root: &Path, segment: &[u8; 16]) -> PathBuf {
    root.join("packs").join(format!("{}.pack", hex(segment)))
}
fn header(file: &File) -> Result<u64> {
    let mut h = [0; 16];
    file.read_exact_at(&mut h, 0)?;
    let n = u64::from_le_bytes(h[8..].try_into().unwrap());
    if &h[..8] != MAGIC
        || n.checked_mul(ENTRY as u64)
            .and_then(|n| n.checked_add(HEADER))
            != Some(file.metadata()?.len())
    {
        return Err(Error::new("InvalidPackIndex", "header or length"));
    }
    Ok(n)
}
fn lookup(root: &Path, hash: &str) -> Result<Option<Locator>> {
    let file = match File::open(index_path(root, &hash[..2])) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let key = binary::<32>(hash)?;
    let (mut lo, mut hi) = (0, header(&file)?);
    let mut b = [0; ENTRY];
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        file.read_exact_at(&mut b, HEADER + mid * ENTRY as u64)?;
        let v = Locator::decode(&b)?;
        match v.hash.cmp(&key) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(Some(v)),
        }
    }
    Ok(None)
}
fn read_locator(root: &Path, v: &Locator) -> Result<Vec<u8>> {
    let file = File::open(data_path(root, &v.segment))?;
    let end = v
        .offset
        .checked_add(v.bytes)
        .ok_or_else(|| Error::new("InvalidPackIndex", "offset overflow"))?;
    if end > file.metadata()?.len() {
        return Err(Error::new("DigestMismatch", "truncated packed payload"));
    }
    let mut b = vec![0; v.bytes as usize];
    file.read_exact_at(&mut b, v.offset)?;
    if digest(&b) != hex(&v.hash) {
        return Err(Error::new("DigestMismatch", "packed payload"));
    }
    Ok(b)
}
#[cfg(test)]
pub(super) fn read(root: &Path, r: &ObjectRef) -> Result<Option<Vec<u8>>> {
    let Some(v) = lookup(root, &r.hash)? else {
        return Ok(None);
    };
    if v.bytes != r.bytes {
        return Err(Error::new("DigestMismatch", "packed object length"));
    }
    read_locator(root, &v).map(Some)
}
struct Lock(File);
impl Lock {
    fn take(root: &Path) -> Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("pack-index/publish.lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
fn entries(root: &Path, prefix: &str) -> Result<BTreeMap<[u8; 32], Locator>> {
    let path = index_path(root, prefix);
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let n = header(&f)?;
    let mut reader = BufReader::new(f);
    reader.seek(SeekFrom::Start(HEADER))?;
    let mut result = BTreeMap::new();
    let mut b = [0; ENTRY];
    let mut last = None;
    for _ in 0..n {
        reader.read_exact(&mut b)?;
        let v = Locator::decode(&b)?;
        if last.is_some_and(|h| h >= v.hash) {
            return Err(Error::new("InvalidPackIndex", "unsorted or duplicate key"));
        }
        last = Some(v.hash);
        result.insert(v.hash, v);
    }
    Ok(result)
}
fn save_index(root: &Path, prefix: &str, rows: &BTreeMap<[u8; 32], Locator>) -> Result<()> {
    let path = index_path(root, prefix);
    let tmp = path.with_extension(cq_capture_protocol::new_id()?);
    let result = (|| {
        let mut f = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&tmp)?);
        f.write_all(MAGIC)?;
        f.write_all(&(rows.len() as u64).to_le_bytes())?;
        for v in rows.values() {
            f.write_all(&v.encode())?;
        }
        f.flush()?;
        f.get_ref().sync_all()?;
        std::fs::rename(&tmp, &path)?;
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}
struct Pending {
    file: BufWriter<File>,
    segment: [u8; 16],
    bytes: u64,
    rows: BTreeMap<[u8; 32], Locator>,
}
pub(super) struct Writer {
    root: PathBuf,
    pending: BTreeMap<String, Pending>,
    pub syncs: u64,
}
impl Writer {
    pub fn new(root: &Path) -> Result<Self> {
        crate::atomic::create_directory(&root.join("packs"))?;
        crate::atomic::create_directory(&root.join("pack-index"))?;
        Ok(Self {
            root: root.into(),
            pending: BTreeMap::new(),
            syncs: 0,
        })
    }
    pub fn put(&mut self, r: &ObjectRef, bytes: &[u8]) -> Result<bool> {
        let prefix = &r.hash[..2];
        let hash = binary::<32>(&r.hash)?;
        if let Some(p) = self.pending.get_mut(prefix)
            && let Some(v) = p.rows.get(&hash)
        {
            p.file.flush()?;
            if read_locator(&self.root, v)? != bytes {
                return Err(Error::new("DigestMismatch", "duplicate pending object"));
            }
            return Ok(false);
        }
        if self
            .pending
            .get(prefix)
            .is_some_and(|p| p.rows.len() >= BATCH || p.bytes + r.bytes > SEGMENT_BYTES)
        {
            self.seal(prefix)?;
        }
        if !self.pending.contains_key(prefix) {
            let segment = binary::<16>(&cq_capture_protocol::new_id()?)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(data_path(&self.root, &segment))?;
            self.pending.insert(
                prefix.into(),
                Pending {
                    file: BufWriter::with_capacity(64 * 1024, file),
                    segment,
                    bytes: 0,
                    rows: BTreeMap::new(),
                },
            );
        }
        let p = self.pending.get_mut(prefix).unwrap();
        p.file.write_all(bytes)?;
        let loc = Locator {
            hash,
            segment: p.segment,
            offset: p.bytes,
            bytes: r.bytes,
        };
        p.bytes += r.bytes;
        p.rows.insert(hash, loc);
        Ok(true)
    }
    pub fn read(&mut self, r: &ObjectRef) -> Result<Option<Vec<u8>>> {
        if let Some(p) = self.pending.get_mut(&r.hash[..2])
            && let Some(v) = p.rows.get(&binary::<32>(&r.hash)?)
        {
            if v.bytes != r.bytes {
                return Err(Error::new("DigestMismatch", "pending length"));
            }
            p.file.flush()?;
            return read_locator(&self.root, v).map(Some);
        }
        Ok(None)
    }
    fn seal(&mut self, prefix: &str) -> Result<()> {
        let Some(mut p) = self.pending.remove(prefix) else {
            return Ok(());
        };
        p.file.flush()?;
        p.file.get_ref().sync_all()?;
        File::open(self.root.join("packs"))?.sync_all()?;
        self.syncs += 2;
        let _lock = Lock::take(&self.root)?;
        let mut rows = entries(&self.root, prefix)?;
        for (hash, v) in p.rows {
            if let Some(old) = rows.get(&hash) {
                if old.bytes != v.bytes
                    || read_locator(&self.root, old)? != read_locator(&self.root, &v)?
                {
                    return Err(Error::new("DigestMismatch", "existing packed object"));
                }
            } else {
                rows.insert(hash, v);
            }
        }
        save_index(&self.root, prefix, &rows)?;
        self.syncs += 2;
        Ok(())
    }
    pub fn sync(&mut self) -> Result<()> {
        for prefix in self.pending.keys().cloned().collect::<Vec<_>>() {
            self.seal(&prefix)?;
        }
        File::open(&self.root)?.sync_all()?;
        self.syncs += 1;
        Ok(())
    }
}
pub(super) fn visit(root: &Path, mut f: impl FnMut(ObjectRef) -> Result<()>) -> Result<()> {
    let p = root.join("pack-index");
    if !p.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(p)? {
        let path = entry?.path();
        let Some(prefix) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().is_none_or(|e| e != "idx") || prefix.len() != 2 {
            continue;
        }
        for row in entries(root, prefix)?.into_values() {
            f(row.reference())?;
        }
    }
    Ok(())
}
pub(super) fn remove(root: &Path, hashes: &[String]) -> Result<()> {
    if !root.join("pack-index").is_dir() {
        return Ok(());
    }
    let _lock = Lock::take(root)?;
    let mut groups = BTreeMap::<String, Vec<[u8; 32]>>::new();
    for hash in hashes {
        if !valid_hash(hash) {
            return Err(Error::new("InvalidObjectRef", "retirement hash"));
        }
        groups
            .entry(hash[..2].into())
            .or_default()
            .push(binary::<32>(hash)?);
    }
    for (prefix, keys) in groups {
        let mut rows = entries(root, &prefix)?;
        for key in keys {
            rows.remove(&key);
        }
        save_index(root, &prefix, &rows)?;
    }
    // Mixed live/dead segments stay immutable. Fully unreferenced segments,
    // including interrupted preparation orphans, can be reclaimed safely.
    let mut live = BTreeSet::new();
    for i in 0..256 {
        for row in entries(root, &format!("{i:02x}"))?.into_values() {
            live.insert(hex(&row.segment));
        }
    }
    for e in std::fs::read_dir(root.join("packs"))? {
        let p = e?.path();
        if p.extension().is_some_and(|e| e == "pack")
            && p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| !live.contains(s))
        {
            std::fs::remove_file(p)?;
        }
    }
    File::open(root.join("packs"))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::objects::Objects;
    use cq_core::ports::facts::ReadStats;
    fn one_bucket(n: usize) -> Vec<(ObjectRef, Vec<u8>)> {
        use sha2::{Digest, Sha256};
        let mut rows = Vec::new();
        for i in 0u64.. {
            let bytes = i.to_le_bytes().to_vec();
            let hash = Sha256::digest(&bytes);
            if hash[0] == 0 {
                rows.push((
                    ObjectRef {
                        hash: hex(&hash),
                        bytes: bytes.len() as u64,
                    },
                    bytes,
                ));
                if rows.len() == n {
                    break;
                }
            }
        }
        rows
    }
    #[test]
    fn performance_pack_index_is_not_republished_for_each_thousand_objects() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("objects");
        let rows = one_bucket(2049);
        let mut writer = Writer::new(&root).unwrap();
        for (r, bytes) in &rows {
            writer.put(r, bytes).unwrap();
        }
        // A preparation owns ObjectGate; publication only needs the durable
        // catalogue at its barrier, rather than repeated whole-bucket rewrites.
        writer.sync().unwrap();
        assert!(
            writer.syncs <= 5,
            "one populated bucket needs one durability barrier, got {} fsyncs",
            writer.syncs
        );
        drop(writer);
        for (r, bytes) in rows {
            assert_eq!(read(&root, &r).unwrap().unwrap(), bytes);
        }
    }
    #[test]
    fn performance_immutable_bucket_index_is_opened_once_for_many_distinct_reads() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("objects");
        let rows = one_bucket(64);
        {
            let o = Objects::buffered(root.clone()).unwrap();
            for (_, bytes) in &rows {
                o.put(bytes).unwrap();
            }
            o.sync_pending().unwrap();
        }
        let o = Objects::new(root).unwrap();
        for (r, bytes) in rows {
            assert_eq!(
                &**o.read(&r, &mut ReadStats::default()).unwrap(),
                bytes.as_slice()
            );
        }
        assert_eq!(o.resource_stats()["pack_index_opens"].as_u64(), Some(1));
        assert_eq!(
            o.resource_stats()["pack_index_read_blocks"].as_u64(),
            Some(1)
        );
        assert!(
            o.resource_stats()["pack_index_cache_bytes"]
                .as_u64()
                .unwrap()
                < 256
        );
    }
    #[test]
    fn cached_locator_snapshots_follow_replacement_and_do_not_hide_truncation() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("objects");
        let rows = one_bucket(2);
        let a = Objects::buffered(root.clone()).unwrap();
        a.put(&rows[0].1).unwrap();
        a.sync_pending().unwrap();
        let reader = Objects::new(root.clone()).unwrap();
        reader.read(&rows[0].0, &mut ReadStats::default()).unwrap();
        let b = Objects::buffered(root.clone()).unwrap();
        b.put(&rows[1].1).unwrap();
        b.sync_pending().unwrap();
        reader.read(&rows[1].0, &mut ReadStats::default()).unwrap();
        assert_eq!(
            reader.resource_stats()["pack_index_opens"].as_u64(),
            Some(2)
        );
        std::fs::write(index_path(&root, "00"), b"broken").unwrap();
        // A physical read bypasses the payload cache and checks the changed
        // locator snapshot; this must remain an error after cache optimization.
        assert!(reader.audit_bytes(&rows[0].0).is_err());
    }
    fn files(p: &Path) -> usize {
        std::fs::read_dir(p)
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                if p.is_dir() { files(&p) } else { 1 }
            })
            .sum()
    }
    #[test]
    fn packed_objects_reopen_mix_with_loose_and_keep_content_addresses() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("objects");
        let loose = Objects::new(root.clone()).unwrap();
        let a = loose.put(b"legacy loose").unwrap();
        let packed = Objects::buffered(root.clone()).unwrap();
        let mut refs = vec![];
        for i in 0..3000 {
            let bytes = format!("object-{i}");
            let r = packed.put(bytes.as_bytes()).unwrap();
            assert_eq!(
                &**packed.read(&r, &mut ReadStats::default()).unwrap(),
                bytes.as_bytes()
            );
            refs.push(r);
        }
        packed.sync_pending().unwrap();
        drop(packed);
        let reopened = Objects::new(root.clone()).unwrap();
        assert_eq!(
            &**reopened.read(&a, &mut ReadStats::default()).unwrap(),
            b"legacy loose"
        );
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(
                &**reopened.read(r, &mut ReadStats::default()).unwrap(),
                format!("object-{i}").as_bytes()
            );
        }
        assert!(
            files(&root) < 800,
            "payloads must be packed, not one file per object"
        );
    }
    #[test]
    fn corrupt_index_payload_and_missing_segment_fail_closed() {
        for damage in ["index", "payload", "missing"] {
            let t = tempfile::tempdir().unwrap();
            let o = Objects::buffered(t.path().join("objects")).unwrap();
            let r = o.put(b"frozen evidence").unwrap();
            o.sync_pending().unwrap();
            let v = lookup(&o.root, &r.hash).unwrap().unwrap();
            let data = data_path(&o.root, &v.segment);
            match damage {
                "index" => std::fs::write(index_path(&o.root, &r.hash[..2]), b"bad index").unwrap(),
                "payload" => {
                    let f = OpenOptions::new().write(true).open(data).unwrap();
                    f.write_all_at(b"changed", v.offset).unwrap();
                }
                _ => std::fs::remove_file(data).unwrap(),
            }
            assert!(read(&o.root, &r).is_err(), "{damage}");
        }
    }
    #[test]
    fn interrupted_writer_has_no_visible_locator_and_retirement_keeps_shared_segment() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("objects");
        let a = {
            let o = Objects::buffered(root.clone()).unwrap();
            o.put(b"interrupted").unwrap()
        };
        assert!(read(&root, &a).unwrap().is_none());
        let mut w = Writer::new(&root).unwrap();
        let mut refs = vec![];
        // Force two members into one bucket and therefore one segment.
        for i in 0..10000 {
            let bytes = i.to_string();
            let r = ObjectRef {
                hash: digest(bytes.as_bytes()),
                bytes: bytes.len() as u64,
            };
            if r.hash.starts_with("00") {
                w.put(&r, bytes.as_bytes()).unwrap();
                refs.push(r);
                if refs.len() == 2 {
                    break;
                }
            }
        }
        w.sync().unwrap();
        let live = lookup(&root, &refs[1].hash).unwrap().unwrap();
        remove(&root, &[refs[0].hash.clone()]).unwrap();
        assert!(read(&root, &refs[0]).unwrap().is_none());
        assert!(read(&root, &refs[1]).unwrap().is_some());
        assert!(data_path(&root, &live.segment).is_file());
        remove(&root, &[refs[1].hash.clone()]).unwrap();
        assert!(!data_path(&root, &live.segment).exists());
    }
    #[test]
    fn concurrent_pack_writers_merge_indexes_without_lost_objects() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("objects");
        let handles = (0..3)
            .map(|worker| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let o = Objects::buffered(root).unwrap();
                    let refs = (0..40)
                        .map(|i| o.put(format!("worker-{worker}-{i}").as_bytes()).unwrap())
                        .collect::<Vec<_>>();
                    o.sync_pending().unwrap();
                    refs
                })
            })
            .collect::<Vec<_>>();
        for h in handles {
            for r in h.join().unwrap() {
                assert!(read(&root, &r).unwrap().is_some());
            }
        }
    }
}
