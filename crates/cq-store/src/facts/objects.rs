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
        atomic::{AtomicU64, Ordering},
    },
};
pub const MAX_OBJECT: usize = 8 * 1024 * 1024;
pub const CACHE_BYTES: usize = 32 * 1024 * 1024;
#[derive(Default)]
struct Cache {
    bytes: usize,
    entries: HashMap<String, std::sync::Arc<Vec<u8>>>,
    order: VecDeque<String>,
}
pub struct Objects {
    pub root: PathBuf,
    cache: Arc<Mutex<Cache>>,
    packed: Option<Mutex<super::packed::Writer>>,
    syncs: AtomicU64,
    writes: AtomicU64,
    written_bytes: AtomicU64,
}
pub fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Objects {
    pub fn new(root: PathBuf) -> Result<Self> {
        crate::atomic::create_directory(&root)?;
        let root = root.canonicalize()?;
        static CACHES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<Cache>>>>> = OnceLock::new();
        let mut caches = CACHES.get_or_init(Default::default).lock().unwrap();
        caches.retain(|_, v| v.strong_count() > 0);
        let cache = if let Some(c) = caches.get(&root).and_then(Weak::upgrade) {
            c
        } else {
            let c = Arc::new(Mutex::new(Cache::default()));
            caches.insert(root.clone(), Arc::downgrade(&c));
            c
        };
        Ok(Self {
            root,
            cache,
            packed: None,
            syncs: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
        })
    }
    /// Publication preparation owns ObjectGate until all objects are durable
    /// and its staging root is installed. Ordinary callers stay synchronous.
    pub fn buffered(root: PathBuf) -> Result<Self> {
        let mut o = Self::new(root)?;
        o.packed = Some(Mutex::new(super::packed::Writer::new(&o.root)?));
        Ok(o)
    }
    pub fn sync_pending(&self) -> Result<()> {
        if let Some(packed) = &self.packed {
            let mut p = packed.lock().unwrap();
            p.sync()?;
            self.syncs.store(p.syncs, Ordering::Relaxed);
            return Ok(());
        }
        Ok(())
    }
    pub fn resource_stats(&self) -> serde_json::Value {
        let c = self.cache.lock().unwrap();
        serde_json::json!({"cache_bytes":c.bytes,"cache_limit":CACHE_BYTES,"sync_calls":self.syncs.load(Ordering::Relaxed),"encoding":"packed-segments-v1"})
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
                let mut p = packed.lock().unwrap();
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
        let p = self.path(r)?;
        {
            let c = self.cache.lock().unwrap();
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
        let mut c = self.cache.lock().unwrap();
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
    pub fn clear_cache(&self) {
        *self.cache.lock().unwrap() = Cache::default();
    }
    pub fn audit(&self, r: &ObjectRef) -> Result<()> {
        self.audit_bytes(r).map(|_| ())
    }
    pub(crate) fn audit_bytes(&self, r: &ObjectRef) -> Result<Vec<u8>> {
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
            && let Some(b) = p.lock().unwrap().read(r)?
        {
            return Ok(b);
        }
        super::packed::read(&self.root, r)?
            .ok_or_else(|| Error::new("MissingFactPage", r.hash.clone()))
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
