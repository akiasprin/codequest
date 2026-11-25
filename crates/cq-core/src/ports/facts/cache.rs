//! Bounded derived data, keyed by the immutable project and revision.
use super::*;
use crate::{Error, model::FileRevision};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

type PageKey = (String, Option<String>, usize);
type Pages = BTreeMap<PageKey, FactBatch>;
pub struct ReadCache {
    project: String,
    revision: String,
    facts: Mutex<(usize, BTreeMap<String, Option<FactRow>>)>,
    counts: Mutex<BTreeMap<String, u64>>,
    pages: Mutex<(usize, Pages)>,
    files: Mutex<Option<BTreeMap<String, (FileRevision, FactRow)>>>,
    locations: Mutex<locations::SourceLocations>,
}
impl ReadCache {
    pub fn new(project: &str, revision: &str) -> Self {
        Self {
            project: project.into(),
            revision: revision.into(),
            facts: Mutex::new(Default::default()),
            counts: Mutex::new(Default::default()),
            pages: Mutex::new(Default::default()),
            files: Mutex::new(None),
            locations: Mutex::new(Default::default()),
        }
    }
}
pub struct CachedReader<'a> {
    inner: &'a dyn FactReader,
    cache: Arc<ReadCache>,
}
impl<'a> CachedReader<'a> {
    pub fn new(inner: &'a dyn FactReader, cache: Arc<ReadCache>) -> Result<Self> {
        if inner.project() != cache.project || inner.revision() != cache.revision {
            return Err(Error::new(
                "ReadCacheScope",
                "cache belongs to another immutable view",
            ));
        }
        Ok(Self { inner, cache })
    }
}
impl FactReader for CachedReader<'_> {
    fn project(&self) -> &str {
        self.inner.project()
    }
    fn revision(&self) -> &str {
        self.inner.revision()
    }
    fn metadata(&self) -> &Value {
        self.inner.metadata()
    }
    fn query_cache(&self) -> Option<Arc<ReadCache>> {
        Some(self.cache.clone())
    }
    fn set_timeout(&self, timeout: u64) {
        self.inner.set_timeout(timeout)
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<FactRow>> {
        if family == "file"
            && let Some(row) = self
                .cache
                .files
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|files| files.get(id))
                .map(|(_, r)| r.clone())
        {
            return Ok(Some(row));
        }
        let key = fact_key(family, id);
        if let Some(v) = self.cache.facts.lock().unwrap().1.get(&key).cloned() {
            return Ok(v);
        }
        let v = self.inner.get(family, id)?;
        let n = key.len() + serde_json::to_vec(&v)?.len();
        if n <= 128 * 1024 {
            let mut c = self.cache.facts.lock().unwrap();
            if c.0 + n > 4 * 1024 * 1024 || c.1.len() >= 8192 {
                *c = Default::default();
            }
            if !c.1.contains_key(&key) {
                c.0 += n;
                c.1.insert(key, v.clone());
            }
        }
        Ok(v)
    }
    fn scan(&self, group: &str, after: Option<&str>, n: usize) -> Result<FactBatch> {
        let key = (group.into(), after.map(str::to_owned), n);
        if let Some(page) = self.cache.pages.lock().unwrap().1.get(&key).cloned() {
            return Ok(page);
        }
        let page = self.inner.scan(group, after, n)?;
        let bytes = serde_json::to_vec(&page)?.len();
        if bytes <= 1024 * 1024 {
            let mut pages = self.cache.pages.lock().unwrap();
            if pages.0 + bytes > 8 * 1024 * 1024 || pages.1.len() >= 512 {
                *pages = Default::default();
            }
            if !pages.1.contains_key(&key) {
                pages.0 += bytes;
                pages.1.insert(key, page.clone());
            }
        }
        Ok(page)
    }
    fn count(&self, group: &str) -> Result<u64> {
        if let Some(n) = self.cache.counts.lock().unwrap().get(group).copied() {
            return Ok(n);
        }
        let n = self.inner.count(group)?;
        let mut c = self.cache.counts.lock().unwrap();
        if c.len() >= 4096 {
            c.clear();
        }
        c.insert(group.into(), n);
        Ok(n)
    }
    fn named_files(&self, path: &str, directory: bool) -> Result<Vec<FileRevision>> {
        let mut saved = self.cache.files.lock().unwrap();
        if let Some(files) = &*saved {
            return Ok(files
                .values()
                .filter(|(f, _)| path_matches(&f.logical_path.display(), path, directory))
                .map(|(f, _)| f.clone())
                .collect());
        }
        let mut files = BTreeMap::new();
        let mut matches = vec![];
        let mut bytes = 0;
        let mut cacheable = true;
        let mut after = None;
        loop {
            let b = self.inner.scan("all:file", after.as_deref(), 1000)?;
            for row in b.rows {
                let f: FileRevision = serde_json::from_value(row.value.clone())?;
                if path_matches(&f.logical_path.display(), path, directory) {
                    matches.push(f.clone());
                }
                if cacheable {
                    bytes += serde_json::to_vec(&row)?.len();
                    if bytes > 32 * 1024 * 1024 || files.len() >= 65536 {
                        files.clear();
                        cacheable = false;
                    } else {
                        files.insert(f.id.clone(), (f, row));
                    }
                }
            }
            if b.complete {
                break;
            }
            after = b.after;
        }
        if cacheable {
            *saved = Some(files);
        }
        Ok(matches)
    }
    fn source_range(&self, f: &str, a: u64, b: u64) -> Result<Vec<u8>> {
        self.inner.source_range(f, a, b)
    }
    fn source_position(&self, f: &str, offset: u64, len: u64) -> Result<Option<SourcePosition>> {
        self.cache
            .locations
            .lock()
            .unwrap()
            .lookup(f, offset, len, |a, b| self.inner.source_range(f, a, b))
    }
    fn stats(&self) -> ReadStats {
        self.inner.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlobId, NativeValue};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Reader {
        revision: String,
        scans: AtomicUsize,
        fail_tail: AtomicBool,
    }
    impl FactReader for Reader {
        fn project(&self) -> &str {
            "cache-contract"
        }
        fn revision(&self) -> &str {
            &self.revision
        }
        fn metadata(&self) -> &Value {
            static V: Value = Value::Null;
            &V
        }
        fn get(&self, _: &str, _: &str) -> Result<Option<FactRow>> {
            Ok(None)
        }
        fn count(&self, _: &str) -> Result<u64> {
            Ok(2000)
        }
        fn source_range(&self, _: &str, _: u64, _: u64) -> Result<Vec<u8>> {
            unreachable!()
        }
        fn stats(&self) -> ReadStats {
            ReadStats::default()
        }
        fn scan(&self, group: &str, after: Option<&str>, limit: usize) -> Result<FactBatch> {
            assert_eq!(group, "all:file");
            self.scans.fetch_add(1, Ordering::SeqCst);
            if after.is_some() && self.fail_tail.swap(false, Ordering::SeqCst) {
                return Err(Error::new("ReadFailed", "controlled tail failure"));
            }
            let from = after.map_or(0, |s| s.parse::<usize>().unwrap());
            let end = (from + limit).min(2000);
            let rows = (from..end)
                .map(|i| {
                    let f = FileRevision {
                        id: format!("f{i}"),
                        logical_path: NativeValue::from(format!("/repo/src/file{i}.rs").as_str()),
                        blob: BlobId::of(b""),
                        origin: "fixture".into(),
                    };
                    FactRow {
                        key: format!("file:f{i}"),
                        reference: FactRef {
                            object: ObjectRef {
                                hash: "fixture".into(),
                                bytes: 0,
                            },
                            offset: 0,
                            length: 0,
                            digest: "fixture".into(),
                        },
                        value: serde_json::to_value(f).unwrap(),
                    }
                })
                .collect();
            Ok(FactBatch {
                rows,
                after: (end < 2000).then(|| end.to_string()),
                complete: end == 2000,
            })
        }
    }
    #[test]
    fn file_catalogue_is_shared_across_reads_and_fenced_by_revision() {
        let r = Reader {
            revision: "one".into(),
            scans: AtomicUsize::new(0),
            fail_tail: AtomicBool::new(false),
        };
        let cache = Arc::new(ReadCache::new(r.project(), r.revision()));
        assert_eq!(
            CachedReader::new(&r, cache.clone())
                .unwrap()
                .named_files("src/file900.rs", false)
                .unwrap()
                .len(),
            1
        );
        let scans = r.scans.load(Ordering::SeqCst);
        assert_eq!(
            CachedReader::new(&r, cache.clone())
                .unwrap()
                .named_files("src", true)
                .unwrap()
                .len(),
            2000
        );
        assert_eq!(
            r.scans.load(Ordering::SeqCst),
            scans,
            "later queries must not rescan every frozen file"
        );
        let row = CachedReader::new(&r, cache.clone())
            .unwrap()
            .get("file", "f900")
            .unwrap()
            .expect("catalogue must serve verified file facts without a separate point read");
        assert_eq!(row.key, "file:f900");
        assert_eq!(row.reference.digest, "fixture");
        let other = Reader {
            revision: "two".into(),
            scans: AtomicUsize::new(0),
            fail_tail: AtomicBool::new(false),
        };
        assert!(CachedReader::new(&other, cache).is_err());
    }
    #[test]
    fn failed_catalogue_read_never_becomes_a_cached_absence() {
        let r = Reader {
            revision: "one".into(),
            scans: AtomicUsize::new(0),
            fail_tail: AtomicBool::new(true),
        };
        let cache = Arc::new(ReadCache::new(r.project(), r.revision()));
        let view = CachedReader::new(&r, cache).unwrap();
        assert!(view.named_files("src/file900.rs", false).is_err());
        assert_eq!(view.named_files("src/file900.rs", false).unwrap().len(), 1);
    }
    #[test]
    fn verified_pages_are_reused_but_failed_pages_are_retried() {
        let r = Reader {
            revision: "one".into(),
            scans: AtomicUsize::new(0),
            fail_tail: AtomicBool::new(true),
        };
        let cache = Arc::new(ReadCache::new(r.project(), r.revision()));
        let view = CachedReader::new(&r, cache.clone()).unwrap();
        let first = view.scan("all:file", None, 128).unwrap();
        let again = CachedReader::new(&r, cache)
            .unwrap()
            .scan("all:file", None, 128)
            .unwrap();
        assert_eq!(first.rows[0].reference, again.rows[0].reference);
        assert_eq!(r.scans.load(Ordering::SeqCst), 1);
        assert!(view.scan("all:file", first.after.as_deref(), 128).is_err());
        assert_eq!(
            view.scan("all:file", first.after.as_deref(), 128)
                .unwrap()
                .rows
                .len(),
            128
        );
        view.scan("all:file", first.after.as_deref(), 128).unwrap();
        assert_eq!(r.scans.load(Ordering::SeqCst), 3);
    }
}
