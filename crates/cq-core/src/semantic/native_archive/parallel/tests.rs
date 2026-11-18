use super::*;
use crate::{
    ports::ContentReader,
    semantic::records::{Record, Source},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Content {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    panic: bool,
}
impl ContentReader for Content {
    fn read_blob(&self, id: &BlobId) -> Result<Vec<u8>> {
        self.blobs
            .lock()
            .unwrap()
            .get(&id.digest)
            .cloned()
            .ok_or_else(|| Error::new("MissingContent", "test"))
    }
}
impl ContentRepository for Content {
    fn freeze(&self, _: &Path, _: &str, _: u64) -> Result<FileRevision> {
        Err(Error::new("UnexpectedFreeze", "test"))
    }
    fn put(&self, bytes: &[u8], _: u64) -> Result<BlobId> {
        assert!(!self.panic, "injected worker panic");
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        // Ensure out-of-order execution and overlapping worker work.
        std::thread::sleep(std::time::Duration::from_millis(
            if bytes.starts_with(b" ") { 9 } else { 1 },
        ));
        let id = BlobId::of(bytes);
        self.blobs
            .lock()
            .unwrap()
            .insert(id.digest.clone(), bytes.to_vec());
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(id)
    }
}
#[derive(Default)]
struct Sink {
    records: Mutex<Vec<Record>>,
    fail: bool,
}
impl Source for Sink {
    fn get(&self, family: &str, id: &str) -> Result<Option<Record>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.family() == family && r.id() == id)
            .cloned())
    }
    fn visit(&self, visitor: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
        for r in self.records.lock().unwrap().clone() {
            visitor(r)?;
        }
        Ok(())
    }
}
impl Store for Sink {
    fn put(&self, _: &SemanticSnapshot, record: Record, _: bool) -> Result<()> {
        if self.fail {
            return Err(Error::new("InjectedSinkFailure", "test"));
        }
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}
fn fixture() -> Value {
    const FILES: u64 = 64;
    let files = (0..FILES)
        .map(|i| {
            json!({"id":i,"path":format!("/repo/f{i}.rs"),
        "text":if i == 0 {" fn a(){}"} else {"fn a(){}"}})
        })
        .collect::<Vec<_>>();
    let symbols = (0..FILES)
        .map(|i| {
            json!({"token":i,"key":format!("s{i}"),
        "name":format!("a{i}"),"kind":"Function","documentation":format!("Author docs for {i}"),
        "definition":{"file":i,"start":if i == 0 {4} else {3},"end":if i == 0 {5} else {4}}})
        })
        .collect::<Vec<_>>();
    let references = (0..FILES)
        .map(|i| {
            json!({"target":(i+1)%FILES,"kind":"Reference",
        "source":{"file":i,"start":if i == 0 {4} else {3},"end":if i == 0 {5} else {4}}})
        })
        .collect::<Vec<_>>();
    json!({"protocol":"codequest-hir-export","schema_version":1,"files":files,
        "symbols":symbols,"references":references,"parents":[],"relations":[],
        "bodies":[],"calls":[],"doc_links":[]})
}
fn archive() -> Archive {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    serde_json::to_writer(&mut file, &fixture()).unwrap();
    Archive::open(file.path()).unwrap()
}
#[test]
fn parallel_frames_preserve_serial_record_order_sources_and_metadata() {
    let archive = archive();
    let mut seed = SemanticSnapshot::empty("parallel".into(), NativeValue::from("/repo"));
    seed.production = Some(Default::default());
    let mut serial = seed.clone();
    let serial_sink = Sink::default();
    native_batch::normalize(
        &mut serial,
        &fixture(),
        &[],
        &Content::default(),
        "test",
        Language::Rust,
        "1",
        &serial_sink,
    )
    .unwrap();
    for workers in [1, 4, 48] {
        let mut s = seed.clone();
        let sink = Sink::default();
        let content = Content::default();
        normalize(
            &archive,
            &mut s,
            &[],
            &content,
            "test",
            Language::Rust,
            "1",
            &sink,
            Config {
                workers,
                inflight_bytes: 64 * 1024 * 1024,
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&serial).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&*sink.records.lock().unwrap()).unwrap(),
            serde_json::to_value(&*serial_sink.records.lock().unwrap()).unwrap()
        );
        if workers > 1 {
            assert!(content.peak.load(Ordering::SeqCst) > 1);
        }
    }
}
#[test]
fn oversized_estimated_frame_runs_alone_without_deadlock() {
    let content = Content::default();
    normalize(
        &archive(),
        &mut SemanticSnapshot::empty("budget".into(), NativeValue::from("/repo")),
        &[],
        &content,
        "test",
        Language::Rust,
        "1",
        &Sink::default(),
        Config {
            workers: 4,
            inflight_bytes: 1,
        },
    )
    .unwrap();
    assert_eq!(content.peak.load(Ordering::SeqCst), 1);
}
#[test]
fn sink_failure_and_worker_panic_cancel_backpressured_workers() {
    for panic in [false, true] {
        let error = normalize(
            &archive(),
            &mut SemanticSnapshot::empty("failure".into(), NativeValue::from("/repo")),
            &[],
            &Content {
                panic,
                ..Default::default()
            },
            "test",
            Language::Rust,
            "1",
            &Sink {
                fail: !panic,
                ..Default::default()
            },
            Config {
                workers: 4,
                inflight_bytes: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            if panic {
                "NormalizationWorkerPanic"
            } else {
                "InjectedSinkFailure"
            }
        );
    }
}
