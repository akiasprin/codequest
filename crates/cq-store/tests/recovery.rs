use cq_capture_protocol::{self as p, Envelope, Event, NativeValue, Packet};
use cq_core::{model::*, ports::CaptureJournal};
use cq_store::{LocalStore, journal::LocalCapture};
use std::{
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
#[ignore = "subprocess fixture, invoked by a24_ack_without_seal_does_not_survive_as_receipt"]
fn collector_process_fixture() {
    let Some(path) = std::env::var_os("CQ_TEST_STORE") else {
        return;
    };
    let run = std::env::var("CQ_TEST_RUN").unwrap();
    let store = LocalStore::open(std::path::Path::new(&path)).unwrap();
    let capture = LocalCapture::open(
        store.run_path(&run).unwrap(),
        &run,
        CaptureBudget::default(),
    )
    .unwrap();
    std::fs::write(
        store.root.join("endpoint"),
        capture.endpoint().as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[test]
fn a24_ack_without_seal_does_not_survive_as_receipt() {
    let t = tempfile::tempdir().unwrap();
    let run = p::new_id().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "collector_process_fixture", "--ignored"])
        .env("CQ_TEST_STORE", t.path())
        .env("CQ_TEST_RUN", &run)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let endpoint = t.path().join("endpoint");
    let start = Instant::now();
    while !endpoint.exists() && start.elapsed() < Duration::from_secs(5) {
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    let path = std::fs::read_to_string(endpoint).unwrap();
    let mut stream = UnixStream::connect(path).unwrap();
    let event = Envelope {
        protocol_version: p::VERSION,
        run_id: run.clone(),
        invocation_id: p::new_id().unwrap(),
        seq: 0,
        time_ms: p::now_ms(),
        event: Event::InvocationStarted {
            recorder_pid: 1,
            command: NativeValue::from("rustc"),
            args: vec![],
            cwd: NativeValue::from("/"),
            environment: vec![],
            removed_environment: vec![],
        },
    };
    p::wire::write_packet(&mut stream, &Packet::Event(event), p::MAX_FRAME).unwrap();
    let ack = p::wire::decode(
        &p::wire::read_bytes(&mut stream, p::MAX_FRAME)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(ack, Packet::Ack(p::Ack { accepted: true, .. })));
    child.kill().unwrap();
    child.wait().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    assert!(matches!(
        store.recover_capture(&run).unwrap(),
        SealOutcome::NotSealed(_)
    ));
    assert!(store.run_path(&run).unwrap().join("events.log").exists());
    assert!(!store.run_path(&run).unwrap().join("seal.json").exists());
    // Test helper died abruptly; remove only its task-owned socket.
    let _ = std::fs::remove_file(
        stream
            .peer_addr()
            .ok()
            .and_then(|a| a.as_pathname().map(|p| p.to_path_buf()))
            .unwrap_or_default(),
    );
}
#[test]
fn a24_seal_confirmation_timeout_requires_recovery() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let run = p::new_id().unwrap();
    let budget = CaptureBudget {
        drain_timeout_ms: 1,
        ..CaptureBudget::default()
    };
    let c = LocalCapture::open(store.run_path(&run).unwrap(), &run, budget).unwrap();
    let _idle = UnixStream::connect(c.endpoint()).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let first = c.seal().unwrap();
    assert!(matches!(
        first,
        SealOutcome::Unknown(_) | SealOutcome::Sealed(_)
    ));
    let start = Instant::now();
    loop {
        match store.recover_capture(&run).unwrap() {
            SealOutcome::Sealed(r) => {
                assert_eq!(r.integrity, JournalIntegrity::WithGaps);
                break;
            }
            _ => {
                assert!(start.elapsed() < Duration::from_secs(2));
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
