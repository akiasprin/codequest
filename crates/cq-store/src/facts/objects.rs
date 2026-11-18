use crate::atomic::immutable_write;
use cq_core::{
    Error, Result,
    model::digest,
    ports::facts::{ObjectRef, ReadStats},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{Read, Write},
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
    pending: Option<Mutex<BTreeSet<PathBuf>>>,
    syncs: AtomicU64,
    writebacks: AtomicU64,
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
            pending: None,
            syncs: AtomicU64::new(0),
            writebacks: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
        })
    }
    /// Publication preparation owns ObjectGate until all objects are durable
    /// and its staging root is installed. Ordinary callers stay synchronous.
    pub fn buffered(root: PathBuf) -> Result<Self> {
        let mut o = Self::new(root)?;
        o.pending = Some(Mutex::new(BTreeSet::new()));
        Ok(o)
    }
    pub fn sync_pending(&self) -> Result<()> {
        let Some(pending) = &self.pending else {
            return Ok(());
        };
        let mut pending = pending.lock().unwrap();
        let mut directories = BTreeSet::new();
        // Start data writeback for the whole batch before the durability pass.
        // Every file still gets fsync below. This hint is never the commit
        // barrier and unsupported filesystems retain the synchronous path.
        #[cfg(target_os = "linux")]
        for path in pending.iter() {
            use std::os::fd::AsRawFd;
            let file = File::open(path)?;
            if unsafe { libc::sync_file_range(file.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE) }
                != 0
            {
                let e = std::io::Error::last_os_error();
                if !matches!(
                    e.raw_os_error(),
                    Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EINVAL | libc::EBADF)
                ) {
                    return Err(e.into());
                }
            } else {
                self.writebacks.fetch_add(1, Ordering::Relaxed);
            }
        }
        for path in pending.iter() {
            File::open(path)?.sync_all()?;
            self.syncs.fetch_add(1, Ordering::Relaxed);
            directories.insert(path.parent().unwrap().to_owned());
        }
        for dir in directories {
            File::open(dir)?.sync_all()?;
            self.syncs.fetch_add(1, Ordering::Relaxed);
        }
        if !pending.is_empty() {
            File::open(&self.root)?.sync_all()?;
            File::open(self.root.parent().unwrap())?.sync_all()?;
            self.syncs.fetch_add(2, Ordering::Relaxed);
        }
        pending.clear();
        Ok(())
    }
    pub fn resource_stats(&self) -> serde_json::Value {
        let c = self.cache.lock().unwrap();
        serde_json::json!({"cache_bytes":c.bytes,"cache_limit":CACHE_BYTES,"sync_calls":self.syncs.load(Ordering::Relaxed),"writeback_calls":self.writebacks.load(Ordering::Relaxed)})
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
            if self.pending.is_some() {
                let parent = path.parent().unwrap();
                std::fs::create_dir_all(parent)?;
                let tmp = parent.join(format!(".tmp-{}", cq_capture_protocol::new_id()?));
                let result = (|| {
                    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                    f.write_all(bytes)?;
                    match std::fs::hard_link(&tmp, &path) {
                        Ok(()) => Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            if std::fs::read(&path)? != bytes {
                                Err(Error::new("ConflictingContent", path.display().to_string()))
                            } else {
                                Ok(())
                            }
                        }
                        Err(e) => Err(e.into()),
                    }
                })();
                let _ = std::fs::remove_file(tmp);
                result?;
            } else {
                immutable_write(&path, bytes)?;
            }
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.written_bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        if let Some(p) = &self.pending {
            p.lock().unwrap().insert(path);
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
        let mut bytes = Vec::new();
        File::open(&p)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::new("MissingFactPage", p.display().to_string())
                } else {
                    e.into()
                }
            })?
            .take(r.bytes + 1)
            .read_to_end(&mut bytes)?;
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
        let mut bytes = Vec::new();
        File::open(self.path(r)?)?
            .take(r.bytes + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != r.bytes || digest(&bytes) != r.hash {
            return Err(Error::new("DigestMismatch", &r.hash));
        }
        Ok(bytes)
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
