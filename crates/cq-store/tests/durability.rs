use cq_capture_protocol::{self as p, Envelope, Event, ExitObservation, NativeValue, Packet};
use cq_core::{input::InputView, model::*, ports::*};
use cq_store::{content::BlobStore, journal::LocalCapture};
use std::{io::Write, os::unix::net::UnixStream};

fn event(run: &str, id: &str, seq: u64, kind: Event) -> Envelope {
    Envelope {
        protocol_version: p::VERSION,
        run_id: run.into(),
        invocation_id: id.into(),
        seq,
        time_ms: 1,
        event: kind,
    }
}
fn started() -> Event {
    Event::InvocationStarted {
        recorder_pid: 1,
        command: NativeValue::from("rustc"),
        args: vec![],
        cwd: NativeValue::from("/"),
        environment: vec![],
        removed_environment: vec![],
    }
}
#[test]
fn indexing_jobs_have_independent_input_reservations_over_shared_blobs() {
    let t = tempfile::tempdir().unwrap();
    let store = cq_store::LocalStore::open(t.path()).unwrap();
    let first = store.analysis_scope();
    let second = store.analysis_scope();
    let a = first.content.put(b"abc", 4).unwrap();
    second.content.put(b"def", 4).unwrap();
    assert!(first.content.put(b"def", 4).is_err());
    assert!(second.content.put(b"abc", 4).is_err());
    assert_eq!(second.content.read_blob(&a).unwrap(), b"abc");
}
fn send(capture: &LocalCapture, e: Envelope) -> bool {
    let mut stream = UnixStream::connect(capture.endpoint()).unwrap();
    p::wire::write_packet(&mut stream, &Packet::Event(e), p::MAX_FRAME).unwrap();
    match p::wire::read_bytes(&mut stream, p::MAX_FRAME) {
        Ok(Some(bytes)) => matches!(
            p::wire::decode(&bytes).unwrap(),
            Packet::Ack(p::Ack { accepted: true, .. })
        ),
        _ => false,
    }
}
fn receipt(c: &LocalCapture) -> SealReceipt {
    match c.seal().unwrap() {
        SealOutcome::Sealed(r) => r,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a04_a05_fixed_bytes_are_independent_of_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path().join("blobs"));
    let source = tmp.path().join("x.rs");
    std::fs::write(&source, b"old bytes").unwrap();
    let f = store.freeze(&source, "test", 1024).unwrap();
    let snap = SourceSnapshot::seal(vec![f.clone()], InputBinding::PrePostChecked, vec![]);
    let view = InputView::open(snap, &store).unwrap();
    std::fs::write(&source, b"new bytes").unwrap();
    std::fs::remove_file(&source).unwrap();
    let anchor = SourceAnchor {
        file_revision: f.id.clone(),
        start: 0,
        end: 9,
    };
    assert_eq!(view.read(&anchor, 9).unwrap(), b"old bytes");
    assert!(view.resolve_file(&NativeValue::from("missing")).is_err());
    assert!(
        view.read(
            &SourceAnchor {
                end: 10,
                ..anchor.clone()
            },
            100
        )
        .is_err()
    );
    assert!(view.read(&anchor, 8).is_err());
    std::fs::write(store.path(&f.blob).unwrap(), b"bad bytes").unwrap();
    assert!(view.read(&anchor, 9).is_err());
}
#[test]
fn a21_a23_duplicates_conflicts_and_unfinished_do_not_become_sound() {
    for conflict in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let run = p::new_id().unwrap();
        let id = p::new_id().unwrap();
        let c = LocalCapture::open(tmp.path().join("run"), &run, CaptureBudget::default()).unwrap();
        let first = event(&run, &id, 0, started());
        assert!(send(&c, first.clone()));
        assert!(send(&c, first.clone()));
        if conflict {
            let mut changed = first;
            changed.time_ms += 1;
            assert!(!send(&c, changed));
        }
        let r = receipt(&c);
        assert_eq!(r.event_count, 1);
        assert_eq!(c.health().duplicates, 1);
        assert_eq!(
            r.integrity,
            if conflict {
                JournalIntegrity::Invalid
            } else {
                JournalIntegrity::WithGaps
            }
        );
        assert!(r.gaps.iter().any(|g| g.code == "MissingEvent"));
    }
}
#[test]
fn a21_wrong_run_version_and_orphan_are_visible() {
    for mode in 0..3 {
        let tmp = tempfile::tempdir().unwrap();
        let run = p::new_id().unwrap();
        let id = p::new_id().unwrap();
        let c = LocalCapture::open(tmp.path().join("run"), &run, CaptureBudget::default()).unwrap();
        let mut e = event(
            &run,
            &id,
            0,
            Event::InvocationFinished {
                exit: ExitObservation {
                    code: Some(0),
                    signal: None,
                    spawn_error: None,
                },
                reporting_errors: vec![],
            },
        );
        if mode == 0 {
            e.run_id = p::new_id().unwrap();
        }
        if mode == 1 {
            e.protocol_version += 1;
        }
        assert_eq!(send(&c, e), mode == 2);
        assert_ne!(receipt(&c).integrity, JournalIntegrity::Sound);
    }
}
#[test]
fn a24_seal_recovery_and_tamper_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let run = p::new_id().unwrap();
    let c = LocalCapture::open(tmp.path().join("run"), &run, CaptureBudget::default()).unwrap();
    assert!(matches!(c.recover().unwrap(), SealOutcome::NotSealed(_)));
    c.write_output(OutputStream::Stdout, b"raw output\0")
        .unwrap();
    let r = receipt(&c);
    assert_eq!(r.integrity, JournalIntegrity::Sound);
    assert!(matches!(c.recover().unwrap(), SealOutcome::Sealed(_)));
    assert_eq!(c.output(&r, OutputStream::Stdout).unwrap(), b"raw output\0");
    assert!(c.write_output(OutputStream::Stderr, b"late").is_err());
    std::fs::OpenOptions::new()
        .append(true)
        .open(c.root().join("events.log"))
        .unwrap()
        .write_all(b"corrupt")
        .unwrap();
    assert!(c.recover().is_err());
    assert!(c.read(&r, 0, 10).is_err());
}
#[test]
fn a22_disk_content_frame_and_queue_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path().join("blobs"));
    assert!(store.put(b"12345", 4).is_err());
    store.put(b"abc", 10).unwrap();
    store.put(b"def", 10).unwrap();
    let reused = BlobStore::new(tmp.path().join("blobs"));
    reused.put(b"abc", 4).unwrap();
    assert!(reused.put(b"def", 4).is_err());
    for mode in 0..3 {
        let mut budget = CaptureBudget::default();
        if mode == 0 {
            budget.journal_bytes = 1;
        }
        if mode == 1 {
            budget.frame_bytes = 32;
        }
        if mode == 2 {
            budget.queue_bytes = 1;
        }
        let run = p::new_id().unwrap();
        let id = p::new_id().unwrap();
        let c = LocalCapture::open(tmp.path().join(format!("run{mode}")), &run, budget).unwrap();
        assert!(!send(&c, event(&run, &id, 0, started())));
        assert_ne!(receipt(&c).integrity, JournalIntegrity::Sound);
    }
}
#[test]
fn a21_partial_frame_timeout_does_not_resynchronize_as_a_new_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let run = p::new_id().unwrap();
    let c = LocalCapture::open(tmp.path().join("run"), &run, CaptureBudget::default()).unwrap();
    let mut stream = UnixStream::connect(c.endpoint()).unwrap();
    stream.write_all(&[0, 0]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    drop(stream);
    assert_ne!(receipt(&c).integrity, JournalIntegrity::Sound);
}
