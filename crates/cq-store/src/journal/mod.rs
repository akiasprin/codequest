mod collector;
mod writer;
pub use collector::LocalCapture;
use cq_core::{Error, Result, model::*};
use std::{io::Read, path::Path};

pub(crate) fn checked_file(path: &Path, id: &BlobId) -> Result<Vec<u8>> {
    if id.len > 1024 * 1024 * 1024 {
        return Err(Error::new("JournalLimit", "verified file exceeds 1 GiB"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(id.len + 1)
        .read_to_end(&mut bytes)?;
    if BlobId::of(&bytes) != *id {
        return Err(Error::new("CorruptJournal", path.display().to_string()));
    }
    Ok(bytes)
}
pub fn verify_receipt(root: &Path, receipt: &SealReceipt) -> Result<()> {
    if receipt.protocol_version != cq_capture_protocol::VERSION {
        return Err(Error::new(
            "ProtocolMismatch",
            "unsupported sealed journal protocol",
        ));
    }
    let bytes = std::fs::read(root.join("seal.json"))?;
    let stored: SealReceipt = serde_json::from_slice(&bytes)?;
    if serde_json::to_vec(&stored)? != serde_json::to_vec(receipt)? {
        return Err(Error::new("CorruptJournal", "seal receipt mismatch"));
    }
    let mut seen = std::collections::HashSet::new();
    for file in &receipt.files {
        if !["events.log", "cargo.stdout", "cargo.stderr", "probes.json"]
            .contains(&file.name.as_str())
            || !seen.insert(&file.name)
        {
            return Err(Error::new(
                "CorruptJournal",
                "invalid journal file manifest",
            ));
        }
        checked_file(&root.join(&file.name), &file.blob)?;
    }
    if seen.len() != 4 {
        return Err(Error::new("CorruptJournal", "incomplete journal manifest"));
    }
    let mut events = std::fs::File::open(root.join("events.log"))?;
    let mut count = 0;
    while let Some(bytes) =
        cq_capture_protocol::wire::read_bytes(&mut events, cq_capture_protocol::MAX_FRAME)?
    {
        let cq_capture_protocol::Packet::Event(e) = cq_capture_protocol::wire::decode(&bytes)?
        else {
            return Err(Error::new("CorruptJournal", "non-event record"));
        };
        if e.run_id != receipt.run_id || e.protocol_version != receipt.protocol_version {
            return Err(Error::new("CorruptJournal", "event identity mismatch"));
        }
        count += 1;
    }
    if count != receipt.event_count {
        return Err(Error::new("CorruptJournal", "event count mismatch"));
    }
    Ok(())
}
pub(crate) fn verify_report_evidence(root: &Path, report: &CaptureReport) -> Result<()> {
    let SealOutcome::Sealed(receipt) = &report.seal else {
        return Ok(());
    };
    verify_receipt(root, receipt)?;
    let mut file = std::fs::File::open(root.join("events.log"))?;
    let mut events = Vec::new();
    let mut keys = std::collections::HashMap::new();
    while let Some(bytes) =
        cq_capture_protocol::wire::read_bytes(&mut file, cq_capture_protocol::MAX_FRAME)?
    {
        let cq_capture_protocol::Packet::Event(e) = cq_capture_protocol::wire::decode(&bytes)?
        else {
            unreachable!()
        };
        keys.insert((e.invocation_id.clone(), e.seq), events.len());
        events.push(e);
    }
    for inv in &report.invocations {
        for evidence in &inv.evidence {
            if evidence.source != "events.log"
                || evidence
                    .index
                    .and_then(|i| events.get(i))
                    .is_none_or(|e| e.invocation_id != inv.id)
            {
                return Err(Error::new(
                    "InvalidEvidence",
                    "invocation evidence mismatch",
                ));
            }
        }
        for e in inv
            .started
            .iter()
            .chain(inv.process.iter())
            .chain(inv.finished.iter())
            .chain(inv.argument_files.iter())
        {
            let original = keys
                .get(&(inv.id.clone(), e.seq))
                .and_then(|i| events.get(*i))
                .ok_or_else(|| Error::new("InvalidEvidence", "missing captured event"))?;
            if serde_json::to_vec(e)? != serde_json::to_vec(original)? {
                return Err(Error::new(
                    "InvalidEvidence",
                    "report event differs from journal",
                ));
            }
        }
    }
    let bytes = std::fs::read(root.join("cargo.stdout"))?;
    let messages: Vec<serde_json::Value> = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    if messages != report.build_messages {
        return Err(Error::new(
            "InvalidEvidence",
            "Cargo messages differ from sealed output",
        ));
    }
    Ok(())
}
