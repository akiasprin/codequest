use super::collector::Shared;
use cq_capture_protocol::{self as p, Envelope, Event};
use cq_core::{Result, model::*};
use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    path::PathBuf,
    sync::atomic::Ordering,
    sync::{Arc, mpsc},
};

pub(super) enum Command {
    Record {
        bytes: Vec<u8>,
        envelope: Box<Envelope>,
        answer: mpsc::Sender<(bool, Option<String>)>,
    },
    Seal {
        answer: mpsc::Sender<std::result::Result<SealReceipt, String>>,
    },
}
#[derive(Default)]
struct Progress {
    next: u64,
    started: bool,
    spawned: bool,
    finished: bool,
}
pub(super) fn write_loop(
    mut file: File,
    rx: mpsc::Receiver<Command>,
    shared: Arc<Shared>,
    root: PathBuf,
    run: String,
) {
    let mut seen = HashMap::new();
    let mut progress: HashMap<String, Progress> = HashMap::new();
    while let Ok(command) = rx.recv() {
        match command {
            Command::Record {
                bytes,
                envelope,
                answer,
            } => {
                let result = (|| -> Result<()> {
                    let key = (envelope.invocation_id.clone(), envelope.seq);
                    let hash = digest(&bytes);
                    if let Some(prior) = seen.get(&key) {
                        if prior != &hash {
                            shared.issue("ConflictingEvent", "same key, different bytes");
                            return Err(cq_core::Error::new("ConflictingEvent", "event conflict"));
                        }
                        shared.health.lock().unwrap().duplicates += 1;
                        return Ok(());
                    }
                    shared.reserve_disk(bytes.len() + 4)?;
                    p::wire::write_bytes(&mut file, &bytes, shared.budget.frame_bytes)?;
                    let state = progress.entry(envelope.invocation_id.clone()).or_default();
                    if state.next != envelope.seq || state.finished {
                        shared.issue(
                            "MissingEvent",
                            format!(
                                "{} unexpected sequence {}",
                                envelope.invocation_id, envelope.seq
                            ),
                        );
                    }
                    state.next = envelope.seq + 1;
                    match &envelope.event {
                        Event::InvocationStarted { .. } => {
                            if state.started || envelope.seq != 0 {
                                shared.issue("ProtocolError", "duplicate/misplaced start");
                            }
                            state.started = true;
                        }
                        Event::ProcessStarted { .. } => {
                            if !state.started || state.spawned {
                                shared.issue("ProtocolError", "misplaced process start");
                            }
                            state.spawned = true;
                        }
                        Event::InvocationFinished {
                            exit,
                            reporting_errors,
                        } => {
                            if !state.started || (!state.spawned && exit.spawn_error.is_none()) {
                                shared.issue("MissingEvent", "finish has no process/start");
                            }
                            if !reporting_errors.is_empty() {
                                shared.issue("ReportingFailure", reporting_errors.join("; "));
                            }
                            state.finished = true;
                        }
                        Event::ArgumentFileObserved { .. } => {
                            if !state.started {
                                shared.issue("MissingEvent", "argument has no start");
                            }
                        }
                    }
                    seen.insert(key, hash);
                    shared.health.lock().unwrap().accepted += 1;
                    Ok(())
                })();
                shared.pending.fetch_sub(bytes.len(), Ordering::SeqCst);
                let response = match result {
                    Ok(()) => (true, None),
                    Err(e) => {
                        shared.health.lock().unwrap().rejected += 1;
                        shared.issue("JournalRecord", e.to_string());
                        (false, Some(e.to_string()))
                    }
                };
                let _ = answer.send(response);
            }
            Command::Seal { answer } => {
                for (id, state) in &progress {
                    if !state.started || !state.finished {
                        shared.issue("MissingEvent", format!("incomplete invocation {id}"));
                    }
                }
                let result = (|| -> Result<SealReceipt> {
                    file.flush()?;
                    file.sync_all()?;
                    for f in shared.outputs.lock().unwrap().iter_mut() {
                        f.flush()?;
                        f.sync_all()?;
                    }
                    let mut files = Vec::new();
                    for name in ["events.log", "cargo.stdout", "cargo.stderr", "probes.json"] {
                        files.push(JournalFile {
                            name: name.into(),
                            blob: BlobId::of(&std::fs::read(root.join(name))?),
                        });
                    }
                    let health = shared.health.lock().unwrap().clone();
                    let integrity = if health.issues.iter().any(|g| g.code == "ConflictingEvent") {
                        JournalIntegrity::Invalid
                    } else if health.issues.is_empty() {
                        JournalIntegrity::Sound
                    } else {
                        JournalIntegrity::WithGaps
                    };
                    let receipt = SealReceipt {
                        run_id: run.clone(),
                        protocol_version: p::VERSION,
                        files,
                        event_count: health.accepted,
                        sealed_ms: p::now_ms(),
                        integrity,
                        gaps: health.issues,
                    };
                    crate::atomic::immutable_write(
                        &root.join("seal.json"),
                        &serde_json::to_vec_pretty(&receipt)?,
                    )?;
                    Ok(receipt)
                })();
                let _ = answer.send(result.map_err(|e| e.to_string()));
                break;
            }
        }
    }
}
