use cq_capture_protocol::*;
use std::{collections::HashSet, io::Cursor, os::unix::ffi::OsStringExt};

#[test]
fn a03_native_bytes_boundaries_and_ids() {
    let values = [
        b"".to_vec(),
        b"with spaces \" quote".to_vec(),
        "中文".as_bytes().to_vec(),
        vec![0xff, 0xfe, b'x'],
    ];
    for bytes in values {
        let v = NativeValue::new(std::ffi::OsString::from_vec(bytes.clone()));
        let mut json = serde_json::to_value(&v).unwrap();
        json["display"] = serde_json::json!("not identity");
        let back: NativeValue = serde_json::from_value(json).unwrap();
        assert_eq!(back.0, bytes);
    }
    let threads: Vec<_> = (0..8)
        .map(|_| std::thread::spawn(|| (0..100).map(|_| new_id().unwrap()).collect::<Vec<_>>()))
        .collect();
    let ids: Vec<_> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
}
#[test]
fn a21_a22_frames_reject_truncation_and_large_declared_size() {
    let packet = Packet::Ack(Ack {
        run_id: "r".into(),
        invocation_id: "i".into(),
        seq: 0,
        accepted: true,
        reason: None,
    });
    let mut encoded = vec![];
    wire::write_packet(&mut encoded, &packet, MAX_FRAME).unwrap();
    for end in 1..encoded.len() {
        assert!(wire::read_bytes(&mut Cursor::new(&encoded[..end]), MAX_FRAME).is_err());
    }
    assert!(wire::read_bytes(&mut Cursor::new([0xff; 4]), 32).is_err());
    let decoded = wire::decode(
        &wire::read_bytes(&mut Cursor::new(&encoded), MAX_FRAME)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(decoded, Packet::Ack(_)));
    assert!(
        wire::read_bytes(&mut Cursor::new([]), MAX_FRAME)
            .unwrap()
            .is_none()
    );
}
