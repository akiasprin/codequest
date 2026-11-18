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
    progress: Mutex<Vec<(u64, u64, crate::progress::Pipeline)>>,
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
    fn progress_pipeline(
        &self,
        _: &str,
        completed: u64,
        total: u64,
        pipeline: &crate::progress::Pipeline,
    ) {
        self.progress
            .lock()
            .unwrap()
            .push((completed, total, pipeline.clone()));
    }
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
        let samples = sink.progress.lock().unwrap();
        for (done, total, p) in samples.iter() {
            assert_eq!(p.active, p.planning + p.preparing);
            assert!(p.active <= p.workers);
            assert_eq!(
                *done
                    + (p.queued
                        + p.planning
                        + p.preparing
                        + p.waiting_admission
                        + p.ready
                        + p.merging) as u64,
                *total
            );
        }
        let (done, total, p) = samples.last().unwrap();
        assert_eq!(done, total);
        assert_eq!(
            p.active + p.waiting_admission + p.ready + p.queued + p.merging,
            0
        );
        assert_eq!(p.inflight_bytes, 0);
    }
}
#[test]
fn oversized_estimated_frame_runs_alone_without_deadlock() {
    let content = Content::default();
    let sink = Sink::default();
    normalize(
        &archive(),
        &mut SemanticSnapshot::empty("budget".into(), NativeValue::from("/repo")),
        &[],
        &content,
        "test",
        Language::Rust,
        "1",
        &sink,
        Config {
            workers: 4,
            inflight_bytes: 1,
        },
    )
    .unwrap();
    assert_eq!(content.peak.load(Ordering::SeqCst), 1);
    for (_, _, p) in sink.progress.lock().unwrap().iter() {
        assert!(p.preparing + p.ready + p.merging <= 1);
    }
}

#[test]
fn later_plans_cannot_reserve_ahead_of_the_earliest_unadmitted_file() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 8,
        tasks: 4,
    };
    let mut first = budget.claim(2).unwrap();
    let mut later = budget.claim(2).unwrap();
    std::thread::scope(|scope| {
        let (started_tx, started_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            started_tx.send(()).unwrap();
            assert!(later.admit(8));
            tx.send(()).unwrap();
            later.complete();
        });
        started_rx.recv().unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(first.admit(8));
        assert!(rx.try_recv().is_err());
        first.complete();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    let s = budget.state.lock().unwrap();
    assert_eq!((s.completed, s.bytes, s.tasks), (2, 0, 0));
    assert_eq!(s.stages, [0; 5]);
}

#[test]
fn cancellation_releases_unadmitted_plans_and_wakes_ordered_waiters() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 8,
        tasks: 4,
    };
    let first = budget.claim(2).unwrap();
    let mut later = budget.claim(2).unwrap();
    std::thread::scope(|scope| {
        let budget = &budget;
        scope.spawn(move || {
            assert!(!later.admit(8));
        });
        budget.cancel();
    });
    drop(first);
    let s = budget.state.lock().unwrap();
    assert_eq!((s.bytes, s.tasks), (0, 0));
    assert_eq!(s.stages, [0; 5]);
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
