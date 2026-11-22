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
    fn put(&self, header: &SemanticSnapshot, record: Record, _: bool) -> Result<()> {
        if self.fail {
            return Err(Error::new("InjectedSinkFailure", "test"));
        }
        if let Record::Entity(entity) = &record {
            assert!(header.provenance_records.contains_key(&entity.provenance));
            if let Some(source) = &entity.source {
                assert!(
                    header
                        .sources
                        .files
                        .iter()
                        .any(|f| f.id == source.file_revision)
                );
            }
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
fn build(events_bytes: usize) -> BuildObservation {
    BuildObservation {
        id: "observed-build".into(),
        language: Language::Rust,
        command: NativeValue::from("cargo"),
        arguments: vec![],
        cwd: NativeValue::from("/repo"),
        outcome: BuildOutcome::Succeeded,
        exit: ExitObservation {
            code: Some(0),
            signal: None,
            spawn_error: None,
        },
        events: vec![Envelope {
            protocol_version: 1,
            run_id: "observed-build".into(),
            invocation_id: "rustc".into(),
            seq: 0,
            time_ms: 0,
            event: Event::InvocationStarted {
                recorder_pid: 1,
                command: NativeValue::from("rustc"),
                args: vec![NativeValue::from("x".repeat(events_bytes).as_str())],
                cwd: NativeValue::from("/repo"),
                environment: vec![],
                removed_environment: vec![],
            },
        }],
        rejected_fields: vec![],
        captured_report: Some("retained-build-report".into()),
        gaps: vec![],
    }
}
#[test]
fn parallel_frames_preserve_serial_facts_sources_and_metadata() {
    let archive = archive();
    let mut seed = SemanticSnapshot::empty("parallel".into(), NativeValue::from("/repo"));
    seed.builds.push(build(16));
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
                checkpoints: true,
                workers,
                inflight_bytes: 64 * 1024 * 1024,
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&serial).unwrap()
        );
        let canonical = |sink: &Sink| {
            let mut rows = sink
                .records
                .lock()
                .unwrap()
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>();
            rows.sort();
            rows
        };
        assert_eq!(canonical(&sink), canonical(&serial_sink));
        if workers > 1 {
            assert!(content.peak.load(Ordering::SeqCst) > 1);
        }
        let samples = sink.progress.lock().unwrap();
        for (done, total, p) in samples.iter() {
            assert_eq!(p.active, p.planning + p.preparing);
            assert!(p.active <= p.workers);
            assert!(p.waiting_memory + p.waiting_priority <= p.waiting_admission);
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
fn large_build_events_stay_project_owned_and_never_enter_file_results() {
    let mut raw = fixture();
    for field in ["files", "symbols", "references"] {
        raw[field].as_array_mut().unwrap().truncate(1);
    }
    raw["references"][0]["target"] = json!(0);
    let mut project = SemanticSnapshot::empty("large-build".into(), NativeValue::from("/repo"));
    project.builds.push(build(14_100_000));
    let original = digest(&serde_json::to_vec(&project.builds).unwrap());
    let sources = FrameSources::new(&project, &[]);
    let content = Content::default();
    let mut output = native_batch::prepare_frame(
        &project,
        &raw,
        &[],
        &content,
        "test",
        Language::Rust,
        "1",
        Some(0),
        &sources,
    )
    .unwrap();
    assert!(output.fragment.builds.is_empty());
    assert!(output.fragment.providers.is_empty());
    assert!(
        output
            .fragment
            .provenance_records
            .values()
            .all(|p| p.build_run.as_deref() == Some("observed-build"))
    );

    // Independent public entry point still obtains provenance from the full
    // project observation. Compare all payload families and provenance IDs.
    let mut expected = SemanticSnapshot::empty(project.project.clone(), project.root.clone());
    expected.builds = std::mem::take(&mut project.builds);
    crate::semantic::normalize::native_graph(
        &mut expected,
        &raw,
        &[],
        &content,
        "test",
        Language::Rust,
        "1",
    )
    .unwrap();
    project.builds = std::mem::take(&mut expected.builds);
    let actual_json = serde_json::to_value(&output.fragment).unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();
    for family in [
        "entities",
        "bodies",
        "occurrences",
        "relations",
        "documents",
        "provenance_records",
    ] {
        assert_eq!(actual_json[family], expected_json[family], "{family}");
    }
    crate::semantic::records::drain(&mut output.fragment, &Sink::default()).unwrap();
    let stored = StoredFrame::write(output).unwrap();
    assert!(stored.file.as_file().metadata().unwrap().len() < 32 * 1024);
    let metadata = stored.read().unwrap();
    assert!(
        metadata
            .provenance_records
            .values()
            .all(|p| p.build_run.as_deref() == Some("observed-build"))
    );
    assert_eq!(
        digest(&serde_json::to_vec(&project.builds).unwrap()),
        original
    );
}
#[test]
fn drained_payload_releases_budget_while_metadata_keeps_its_task_slot() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 10,
        tasks: 4,
    };
    let mut first = budget.claim(2).unwrap();
    let mut next = budget.claim(2).unwrap();
    assert!(first.admit(8));
    first.ready(1);
    {
        let state = budget.state.lock().unwrap();
        assert_eq!(state.bytes, 1);
        assert_eq!(state.tasks, 2);
        assert_eq!(state.completed, 0);
        assert_eq!(state.stages[Stage::Ready as usize], 1);
    }
    assert!(next.admit(8));
    assert_eq!(budget.state.lock().unwrap().bytes, 9);
    first.complete();
    next.complete();
    let state = budget.state.lock().unwrap();
    assert_eq!((state.bytes, state.tasks, state.completed), (0, 0, 2));
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
            checkpoints: true,
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
fn later_spooled_metadata_cannot_strand_the_earliest_unadmitted_file() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 8,
        tasks: 4,
    };
    let mut first = budget.claim(2).unwrap();
    let mut later = budget.claim(2).unwrap();
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        scope.spawn(move || {
            assert!(later.admit(8));
            // In production StoredFrame has persisted and dropped the entire
            // output before this transition. Only the task slot is retained.
            later.ready(0);
            tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            later.complete();
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            budget.state.lock().unwrap().stages[Stage::Ready as usize],
            1
        );
        // The oldest frame needs the entire budget, despite its successor
        // already waiting for ordered merge. This used to require serial
        // admission to avoid a circular wait.
        assert!(first.admit(8));
        first.complete();
        finish_tx.send(()).unwrap();
    });
    let s = budget.state.lock().unwrap();
    assert_eq!((s.completed, s.bytes, s.tasks), (2, 0, 0));
    assert_eq!(s.stages, [0; 5]);
}

#[test]
fn memory_wait_and_oldest_ready_priority_are_distinct() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 8,
        tasks: 4,
    };
    let mut state = budget.state.lock().unwrap();
    state.bytes = 4;
    state.waiting.insert(0, 8);
    state.waiting.insert(1, 2);
    assert!(matches!(
        budget.waiting_reason(&state, 0, 8),
        Some(WaitReason::Memory)
    ));
    assert!(budget.waiting_reason(&state, 1, 2).is_none());
    state.bytes = 0;
    assert!(budget.waiting_reason(&state, 0, 8).is_none());
    assert!(matches!(
        budget.waiting_reason(&state, 1, 2),
        Some(WaitReason::Priority)
    ));
    // An oversized successor must wait for all preceding metadata to merge,
    // even if no payload is currently using memory.
    state.waiting.clear();
    state.waiting.insert(1, 9);
    assert!(matches!(
        budget.waiting_reason(&state, 1, 9),
        Some(WaitReason::Memory)
    ));
}

#[test]
fn spooled_frames_preserve_metadata_and_reject_truncated_storage() {
    let mut snapshot = SemanticSnapshot::empty("spool".into(), NativeValue::from("/repo"));
    snapshot.production = Some(Default::default());
    let expected = serde_json::to_value(
        FrameOutput {
            fragment: snapshot.clone(),
            peak: 37,
        }
        .into_metadata()
        .unwrap(),
    )
    .unwrap();
    let stored = StoredFrame::write(FrameOutput {
        fragment: snapshot.clone(),
        peak: 37,
    })
    .unwrap();
    let path = stored.file.path().to_owned();
    let output = stored.read().unwrap();
    assert_eq!(output.peak, 37);
    assert_eq!(serde_json::to_value(output).unwrap(), expected);
    assert!(!path.exists());

    let stored = StoredFrame::write(FrameOutput {
        fragment: snapshot,
        peak: 0,
    })
    .unwrap();
    let path = stored.file.path().to_owned();
    stored.file.as_file().set_len(1).unwrap();
    assert!(stored.read().is_err());
    assert!(!path.exists());
}

#[test]
fn out_of_order_ready_frames_remain_bounded_by_the_task_window() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 8,
        tasks: 2,
    };
    let first = budget.claim(3).unwrap();
    let mut later = budget.claim(3).unwrap();
    assert!(later.admit(2));
    later.ready(0);
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let budget = &budget;
        scope.spawn(move || {
            let permit = budget.claim(3).unwrap();
            tx.send(permit.index).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        first.complete();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
    });
    later.complete();
    assert_eq!(budget.state.lock().unwrap().tasks, 0);
}

#[test]
fn cancellation_releases_unadmitted_plans_and_wakes_ordered_waiters() {
    let budget = Budget {
        state: Mutex::new(Usage::default()),
        changed: Condvar::new(),
        bytes: 8,
        tasks: 4,
    };
    let mut first = budget.claim(2).unwrap();
    assert!(first.admit(8));
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
                checkpoints: true,
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
