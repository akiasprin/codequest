use cq_capture_protocol::{self as p, Event, Packet};
use std::io::Write;
use std::{
    os::unix::net::UnixListener,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn a20_lost_ack_retries_exact_event_and_collector_exit_keeps_compilation_result() {
    for die in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("collector.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let run = p::new_id().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let bytes = p::wire::read_bytes(&mut first, p::MAX_FRAME)
                .unwrap()
                .unwrap();
            let Packet::Event(start) = p::wire::decode(&bytes).unwrap() else {
                panic!()
            };
            assert!(matches!(start.event, Event::InvocationStarted { .. }));
            if die {
                let ack = Packet::Ack(p::Ack {
                    run_id: start.run_id,
                    invocation_id: start.invocation_id,
                    seq: start.seq,
                    accepted: true,
                    reason: None,
                });
                p::wire::write_packet(&mut first, &ack, p::MAX_FRAME).unwrap();
                let _ = p::wire::read_bytes(&mut first, p::MAX_FRAME);
                return;
            }
            drop(first); // First event accepted but ACK lost.
            let (mut retry, _) = listener.accept().unwrap();
            let retried = p::wire::read_bytes(&mut retry, p::MAX_FRAME)
                .unwrap()
                .unwrap();
            assert_eq!(retried, bytes);
            let mut next = Some(retried);
            loop {
                let bytes = next.take().unwrap_or_else(|| {
                    p::wire::read_bytes(&mut retry, p::MAX_FRAME)
                        .unwrap()
                        .unwrap()
                });
                let Packet::Event(e) = p::wire::decode(&bytes).unwrap() else {
                    panic!()
                };
                let finished = matches!(e.event, Event::InvocationFinished { .. });
                p::wire::write_packet(
                    &mut retry,
                    &Packet::Ack(p::Ack {
                        run_id: e.run_id,
                        invocation_id: e.invocation_id,
                        seq: e.seq,
                        accepted: true,
                        reason: None,
                    }),
                    p::MAX_FRAME,
                )
                .unwrap();
                if finished {
                    break;
                }
            }
        });
        let started = Instant::now();
        let out = Command::new(env!("CARGO_BIN_EXE_cq-rustc-recorder"))
            .args(["python3", "-c", "print('kept'); raise SystemExit(19)"])
            .env(p::ENDPOINT_ENV, &path)
            .env(p::RUN_ENV, &run)
            .env("CQ_CAPTURE_REPORT_MS", "50")
            .env("CQ_CAPTURE_REPORT_TOTAL_MS", "200")
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(out.status.code(), Some(19));
        assert_eq!(out.stdout, b"kept\n");
        assert!(out.stderr.is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn a22_trickling_ack_cannot_extend_frame_or_invocation_budget() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("slow.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let bytes = p::wire::read_bytes(&mut stream, p::MAX_FRAME)
            .unwrap()
            .unwrap();
        let Packet::Event(e) = p::wire::decode(&bytes).unwrap() else {
            panic!()
        };
        let mut ack = vec![];
        p::wire::write_packet(
            &mut ack,
            &Packet::Ack(p::Ack {
                run_id: e.run_id,
                invocation_id: e.invocation_id,
                seq: e.seq,
                accepted: true,
                reason: None,
            }),
            p::MAX_FRAME,
        )
        .unwrap();
        for b in ack.iter().take(100) {
            if stream.write_all(&[*b]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_cq-rustc-recorder"))
        .args(["python3", "-c", "print('bounded'); raise SystemExit(19)"])
        .env(p::ENDPOINT_ENV, path)
        .env(p::RUN_ENV, p::new_id().unwrap())
        .env("CQ_CAPTURE_REPORT_MS", "20")
        .env("CQ_CAPTURE_REPORT_TOTAL_MS", "40")
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    server.join().unwrap();
    assert_eq!(out.status.code(), Some(19));
    assert_eq!(out.stdout, b"bounded\n");
    assert!(elapsed < Duration::from_millis(300), "{elapsed:?}");
}
