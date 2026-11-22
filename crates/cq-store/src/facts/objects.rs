use crate::atomic::immutable_write;
use cq_core::{
    Error, Result,
    model::digest,
    ports::facts::{ObjectRef, ReadStats},
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
pub const MAX_OBJECT: usize = 8 * 1024 * 1024;
pub const CACHE_BYTES: usize = 32 * 1024 * 1024;
mod batch;
pub(crate) use batch::BatchReadStats;
#[derive(Default)]
struct Cache {
    bytes: usize,
    entries: HashMap<String, std::sync::Arc<Vec<u8>>>,
    order: VecDeque<String>,
}
struct Shared {
    cache: Mutex<Cache>,
    readers: Vec<Mutex<super::packed::Reader>>,
}
impl Default for Shared {
    fn default() -> Self {
        Self {
            cache: Mutex::new(Cache::default()),
            readers: (0..16).map(|_| Mutex::new(Default::default())).collect(),
        }
    }
}
#[derive(Clone)]
pub struct Objects {
    pub root: PathBuf,
    shared: Arc<Shared>,
    packed: Option<Arc<Vec<Mutex<super::packed::Writer>>>>,
    syncs: Arc<AtomicU64>,
    writes: Arc<AtomicU64>,
    written_bytes: Arc<AtomicU64>,
    trace: Arc<Mutex<Option<BTreeMap<String, ObjectRef>>>>,
    tracing: Arc<AtomicBool>,
}
pub fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Objects {
    pub(crate) fn trace_begin(&self) {
        *self.trace.lock().unwrap() = Some(BTreeMap::new());
        self.tracing.store(true, Ordering::Release);
    }
    pub(crate) fn trace_end(&self) -> Vec<ObjectRef> {
        self.tracing.store(false, Ordering::Release);
        self.trace
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default()
            .into_values()
            .collect()
    }
    fn trace_ref(&self, r: &ObjectRef) {
        if !self.tracing.load(Ordering::Acquire) {
            return;
        }
        if let Some(ref mut trace) = *self.trace.lock().unwrap() {
            trace.insert(r.hash.clone(), r.clone());
        }
    }

    pub fn new(root: PathBuf) -> Result<Self> {
        crate::atomic::create_directory(&root)?;
        let root = root.canonicalize()?;
        static CACHES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Shared>>>> = OnceLock::new();
        let mut caches = CACHES.get_or_init(Default::default).lock().unwrap();
        caches.retain(|_, v| v.strong_count() > 0);
        let cache = if let Some(c) = caches.get(&root).and_then(Weak::upgrade) {
            c
        } else {
            let c = Arc::new(Shared::default());
            caches.insert(root.clone(), Arc::downgrade(&c));
            c
        };
        Ok(Self {
            root,
            shared: cache,
            packed: None,
            syncs: Arc::new(AtomicU64::new(0)),
            writes: Arc::new(AtomicU64::new(0)),
            written_bytes: Arc::new(AtomicU64::new(0)),
            trace: Arc::new(Mutex::new(None)),
            tracing: Arc::new(AtomicBool::new(false)),
        })
    }
    /// Publication preparation owns ObjectGate until all objects are durable
    /// and its staging root is installed. Ordinary callers stay synchronous.
    pub fn buffered(root: PathBuf) -> Result<Self> {
        let mut o = Self::new(root)?;
        o.packed = Some(Arc::new(
            (0..16)
                .map(|_| super::packed::Writer::new(&o.root).map(Mutex::new))
                .collect::<Result<_>>()?,
        ));
        Ok(o)
    }
    pub fn sync_pending(&self) -> Result<()> {
        if let Some(packed) = &self.packed {
            cq_core::compute::map(packed.iter().collect(), |p| p.lock().unwrap().sync())?;
            self.syncs.store(
                packed.iter().map(|p| p.lock().unwrap().syncs).sum(),
                Ordering::Relaxed,
            );
            return Ok(());
        }
        Ok(())
    }
    pub fn resource_stats(&self) -> serde_json::Value {
        let c = self.shared.cache.lock().unwrap();
        let (opens, cached, blocks) =
            self.shared
                .readers
                .iter()
                .fold((0, 0, 0), |(opens, cached, blocks), r| {
                    let r = r.lock().unwrap();
                    (
                        opens + r.index_opens,
                        cached + r.cached_bytes(),
                        blocks + r.index_blocks,
                    )
                });
        serde_json::json!({"cache_bytes":c.bytes,"cache_limit":CACHE_BYTES,"sync_calls":self.syncs.load(Ordering::Relaxed),"pack_index_opens":opens,"pack_index_cache_bytes":cached,"pack_index_cache_limit":16u64*16*1024*1024,"pack_index_read_blocks":blocks,"encoding":"packed-segments-v1","descriptors":super::descriptor_budget::diagnostics()})
    }
    pub fn path(&self, r: &ObjectRef) -> Result<PathBuf> {
        if !valid_hash(&r.hash) || r.bytes > MAX_OBJECT as u64 {
            return Err(Error::new("InvalidObjectRef", "digest or byte limit"));
        }
        Ok(self.root.join(&r.hash[..2]).join(&r.hash))
    }
    pub fn put(&self, bytes: &[u8]) -> Result<ObjectRef> {
        if bytes.len() > MAX_OBJECT {
            return Err(Error::new("FactPageTooLarge", "8 MiB hard limit"));
        }
        let r = ObjectRef {
            hash: digest(bytes),
            bytes: bytes.len() as u64,
        };
        let path = self.path(&r)?;
        if path.exists() {
            if std::fs::read(&path)? != bytes {
                return Err(Error::new("DigestMismatch", &r.hash));
            }
        } else {
            if let Some(packed) = &self.packed {
                if let Some(existing) = self.shared.readers[shard(&r.hash)]
                    .lock()
                    .unwrap()
                    .read(&self.root, &r)?
                {
                    if existing != bytes {
                        return Err(Error::new("DigestMismatch", "existing packed object"));
                    }
                    return Ok(r);
                }
                let mut p = packed[shard(&r.hash)].lock().unwrap();
                if p.put(&r, bytes)? {
                    self.writes.fetch_add(1, Ordering::Relaxed);
                    self.written_bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
                self.syncs.store(p.syncs, Ordering::Relaxed);
                return Ok(r);
            }
            immutable_write(&path, bytes)?;
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.written_bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(r)
    }
    pub fn json<T: serde::Serialize>(&self, value: &T) -> Result<ObjectRef> {
        self.put(&serde_json::to_vec(value)?)
    }
    /// Validates bytes before cache insertion. Cache retention is byte bounded.
    pub fn read(&self, r: &ObjectRef, stats: &mut ReadStats) -> Result<std::sync::Arc<Vec<u8>>> {
        self.trace_ref(r);
        let p = self.path(r)?;
        {
            let c = self.shared.cache.lock().unwrap();
            if let Some(b) = c.entries.get(&r.hash) {
                if b.len() as u64 != r.bytes {
                    return Err(Error::new("DigestMismatch", "object length"));
                }
                return Ok(b.clone());
            }
        }
        let bytes = self.load(r)?;
        if bytes.len() as u64 != r.bytes || digest(&bytes) != r.hash {
            return Err(Error::new("DigestMismatch", p.display().to_string()));
        }
        stats.page_reads += 1;
        stats.bytes_decoded += bytes.len() as u64;
        let bytes = std::sync::Arc::new(bytes);
        let mut c = self.shared.cache.lock().unwrap();
        // Another reader may have populated this shared cache during I/O.
        if let Some(existing) = c.entries.get(&r.hash) {
            return Ok(existing.clone());
        }
        while c.bytes + bytes.len() > CACHE_BYTES {
            if let Some(k) = c.order.pop_front() {
                if let Some(b) = c.entries.remove(&k) {
                    c.bytes -= b.len();
                }
            } else {
                break;
            }
        }
        c.bytes += bytes.len();
        c.order.push_back(r.hash.clone());
        c.entries.insert(r.hash.clone(), bytes.clone());
        Ok(bytes)
    }
    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        r: &ObjectRef,
        stats: &mut ReadStats,
    ) -> Result<T> {
        Ok(serde_json::from_slice(&self.read(r, stats)?)?)
    }
    /// The caller authenticates FactRef membership in a fixed revision. Its
    /// record digest authenticates this range independently of sibling bytes;
    /// full-page audits continue to verify the aggregate object digest.
    pub fn read_record(
        &self,
        r: &cq_core::ports::facts::FactRef,
        stats: &mut ReadStats,
    ) -> Result<Vec<u8>> {
        let path = self.path(&r.object)?;
        if r.offset
            .checked_add(r.length)
            .is_none_or(|end| end > r.object.bytes)
        {
            return Err(Error::new("InvalidFactRef", "record bounds"));
        }
        let plan = if path.is_file() {
            super::packed::ReadPlan {
                path,
                offset: 0,
                bytes: r.object.bytes,
                whole_file: true,
            }
        } else {
            self.shared.readers[shard(&r.object.hash)]
                .lock()
                .unwrap()
                .plan(&self.root, &r.object)?
                .ok_or_else(|| Error::new("MissingFactPage", &r.object.hash))?
        };
        let bytes = plan.range(r.offset, r.length)?;
        if digest(&bytes) != r.digest {
            return Err(Error::new("DigestMismatch", "fact record"));
        }
        stats.page_reads += 1;
        stats.bytes_decoded += bytes.len() as u64;
        self.trace_ref(&r.object);
        Ok(bytes)
    }
    pub fn clear_cache(&self) {
        *self.shared.cache.lock().unwrap() = Cache::default();
    }
    pub(crate) fn clear_index_cache(&self) {
        for reader in &self.shared.readers {
            reader.lock().unwrap().clear();
        }
    }
    pub fn audit(&self, r: &ObjectRef) -> Result<()> {
        self.audit_bytes(r).map(|_| ())
    }
    pub(crate) fn audit_bytes(&self, r: &ObjectRef) -> Result<Vec<u8>> {
        self.trace_ref(r);
        let bytes = self.load(r)?;
        if bytes.len() as u64 != r.bytes || digest(&bytes) != r.hash {
            return Err(Error::new("DigestMismatch", &r.hash));
        }
        Ok(bytes)
    }
    fn load(&self, r: &ObjectRef) -> Result<Vec<u8>> {
        let path = self.path(r)?;
        if path.is_file() {
            let mut b = Vec::new();
            File::open(path)?.take(r.bytes + 1).read_to_end(&mut b)?;
            return Ok(b);
        }
        if let Some(p) = &self.packed
            && let Some(b) = p[shard(&r.hash)].lock().unwrap().read(r)?
        {
            return Ok(b);
        }
        let plan = self.shared.readers[shard(&r.hash)]
            .lock()
            .unwrap()
            .plan(&self.root, r)?
            .ok_or_else(|| Error::new("MissingFactPage", r.hash.clone()))?;
        // Do not keep a shared locator lock while waiting for payload pages.
        plan.read()
    }
    pub(crate) fn visit_packed(&self, f: impl FnMut(ObjectRef) -> Result<()>) -> Result<()> {
        super::packed::visit(&self.root, f)
    }
    pub(crate) fn remove_packed(&self, hashes: &[String]) -> Result<()> {
        super::packed::remove(&self.root, hashes)
    }
    pub fn directory(&self) -> &Path {
        &self.root
    }
    pub fn write_counts(&self) -> (u64, u64) {
        (
            self.writes.load(Ordering::Relaxed),
            self.written_bytes.load(Ordering::Relaxed),
        )
    }
}
fn shard(hash: &str) -> usize {
    match hash.as_bytes()[0] {
        b'0'..=b'9' => (hash.as_bytes()[0] - b'0') as usize,
        c => (c - b'a' + 10) as usize,
    }
}
