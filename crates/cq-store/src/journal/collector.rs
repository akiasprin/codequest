use super::writer::{Command, write_loop};
use cq_capture_protocol::{self as p, Ack, Packet};
use cq_core::{Error, Result, model::*, ports::*};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Shared {
    pub health: Mutex<CaptureHealth>,
    pub closing: AtomicBool,
    pub pending: AtomicUsize,
    pub written: AtomicU64,
    pub outputs: Mutex<Vec<File>>,
    pub budget: CaptureBudget,
}
impl Shared {
    pub fn issue(&self, code: &str, message: impl Into<String>) {
        let mut h = self.health.lock().unwrap();
        if h.issues.len() < 256 {
            h.issues.push(Gap::new(code, "journal", message));
        }
    }
    #[allow(
        deprecated,
        reason = "fetch_update supports the declared Rust 1.91 minimum"
    )]
    pub fn reserve_disk(&self, n: usize) -> Result<()> {
        self.written
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                v.checked_add(n as u64)
                    .filter(|sum| *sum <= self.budget.journal_bytes)
            })
            .map_err(|_| Error::new("JournalLimit", "journal budget exceeded"))?;
        Ok(())
    }
}
pub struct LocalCapture {
    root: PathBuf,
    endpoint: PathBuf,
    run: String,
    shared: Arc<Shared>,
    sender: mpsc::Sender<Command>,
    receipt: Mutex<Option<SealReceipt>>,
}
impl LocalCapture {
    pub fn open(root: PathBuf, run: &str, budget: CaptureBudget) -> Result<Self> {
        if !crate::atomic::valid_id(run) {
            return Err(Error::new("InvalidRunId", run));
        }
        fs::create_dir_all(&root)?;
        if root.join("events.log").exists() {
            return Err(Error::new("RunExists", run));
        }
        // Socket paths have a small OS limit; the durable journal stays at root.
        let endpoint = std::env::temp_dir().join(format!("cq-{}.sock", p::new_id()?));
        let listener = UnixListener::bind(&endpoint)?;
        listener.set_nonblocking(true)?;
        let events = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("events.log"))?;
        let mut outputs = Vec::new();
        for name in ["cargo.stdout", "cargo.stderr", "probes.json"] {
            outputs.push(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(root.join(name))?,
            );
        }
        let shared = Arc::new(Shared {
            health: Mutex::new(CaptureHealth::default()),
            closing: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            written: AtomicU64::new(0),
            outputs: Mutex::new(outputs),
            budget,
        });
        let (sender, receiver) = mpsc::channel();
        let wr_shared = shared.clone();
        let wr_root = root.clone();
        let wr_run = run.to_owned();
        thread::spawn(move || write_loop(events, receiver, wr_shared, wr_root, wr_run));
        let ac_shared = shared.clone();
        let ac_sender = sender.clone();
        let ac_run = run.to_owned();
        let ac_endpoint = endpoint.clone();
        thread::spawn(move || {
            while !ac_shared.closing.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut h = ac_shared.health.lock().unwrap();
                        if h.active_connections >= 64 {
                            drop(h);
                            ac_shared.issue("ConnectionLimit", "64 active capture streams");
                            continue;
                        }
                        h.active_connections += 1;
                        drop(h);
                        let shared = ac_shared.clone();
                        let sender = ac_sender.clone();
                        let run = ac_run.clone();
                        thread::spawn(move || {
                            serve(stream, &run, &sender, &shared);
                            shared.health.lock().unwrap().active_connections -= 1;
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => {
                        ac_shared.issue("ChannelUnavailable", e.to_string());
                        break;
                    }
                }
            }
            let _ = fs::remove_file(ac_endpoint);
        });
        Ok(Self {
            root,
            endpoint,
            run: run.into(),
            shared,
            sender,
            receipt: Mutex::new(None),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}
#[allow(
    deprecated,
    reason = "fetch_update supports the declared Rust 1.91 minimum"
)]
fn serve(mut stream: UnixStream, run: &str, sender: &mpsc::Sender<Command>, shared: &Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    while !shared.closing.load(Ordering::SeqCst) {
        // Only an idle timeout is retryable. A timeout after consuming part of a
        // frame closes the connection; continuing would destroy framing.
        let mut first = [0u8; 1];
        match stream.read(&mut first) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(e) => {
                shared.issue("ProtocolError", e.to_string());
                break;
            }
        }
        let mut header = [first[0], 0, 0, 0];
        if let Err(e) = stream.read_exact(&mut header[1..]) {
            shared.issue("ProtocolError", e.to_string());
            break;
        }
        let n = u32::from_be_bytes(header) as usize;
        if n == 0 || n > shared.budget.frame_bytes {
            shared.issue("FrameLimit", "invalid declared frame size");
            break;
        }
        // Reserve before allocating/reading the payload: concurrent socket
        // readers share the same byte budget as records waiting for the writer.
        if shared
            .pending
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                v.checked_add(n)
                    .filter(|sum| *sum <= shared.budget.queue_bytes)
            })
            .is_err()
        {
            shared.issue("QueueLimit", "pending byte budget exceeded");
            break;
        }
        let mut bytes = vec![0; n];
        if let Err(e) = stream.read_exact(&mut bytes) {
            shared.pending.fetch_sub(n, Ordering::SeqCst);
            shared.issue("ProtocolError", e.to_string());
            break;
        }
        let envelope = match p::wire::decode(&bytes) {
            Ok(Packet::Event(e))
                if e.protocol_version == p::VERSION
                    && e.run_id == run
                    && crate::atomic::valid_id(&e.invocation_id)
                    && e.seq < u64::MAX =>
            {
                e
            }
            _ => {
                shared.pending.fetch_sub(n, Ordering::SeqCst);
                shared.health.lock().unwrap().rejected += 1;
                shared.issue("ProtocolMismatch", "invalid version, run or payload");
                break;
            }
        };
        let (tx, rx) = mpsc::channel();
        if sender
            .send(Command::Record {
                bytes,
                envelope: Box::new(envelope.clone()),
                answer: tx,
            })
            .is_err()
        {
            shared.pending.fetch_sub(n, Ordering::SeqCst);
            break;
        }
        let answer = rx.recv_timeout(Duration::from_millis(shared.budget.drain_timeout_ms));
        let (accepted, reason) = match answer {
            Ok(v) => v,
            Err(_) => break,
        };
        let ack = Packet::Ack(Ack {
            run_id: run.into(),
            invocation_id: envelope.invocation_id,
            seq: envelope.seq,
            accepted,
            reason,
        });
        if p::wire::write_packet(&mut stream, &ack, shared.budget.frame_bytes).is_err() {
            break;
        }
    }
}
impl CaptureJournal for LocalCapture {
    fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    fn write_output(&self, stream: OutputStream, bytes: &[u8]) -> Result<()> {
        if self.shared.closing.load(Ordering::SeqCst) {
            return Err(Error::new("SealedRun", "output closed"));
        }
        if let Err(e) = self.shared.reserve_disk(bytes.len()) {
            self.shared.issue("JournalLimit", e.to_string());
            return Err(e);
        }
        let index = match stream {
            OutputStream::Stdout => 0,
            OutputStream::Stderr => 1,
            OutputStream::Probes => 2,
        };
        let result = self.shared.outputs.lock().unwrap()[index].write_all(bytes);
        if let Err(e) = &result {
            self.shared.issue("JournalWrite", e.to_string());
        }
        result.map_err(Into::into)
    }
    fn seal(&self) -> Result<SealOutcome> {
        if let Some(receipt) = self.receipt.lock().unwrap().clone() {
            return Ok(SealOutcome::Sealed(receipt));
        }
        self.shared.closing.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_millis(self.shared.budget.drain_timeout_ms);
        while self.shared.health.lock().unwrap().active_connections > 0 && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        if self.shared.health.lock().unwrap().active_connections > 0 {
            self.shared.issue(
                "DrainIncomplete",
                "capture connections exceeded drain deadline",
            );
        }
        let (tx, rx) = mpsc::channel();
        if self.sender.send(Command::Seal { answer: tx }).is_err() {
            return self.recover();
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(r)) => {
                *self.receipt.lock().unwrap() = Some(r.clone());
                Ok(SealOutcome::Sealed(r))
            }
            Ok(Err(e)) => Ok(SealOutcome::NotSealed(e)),
            Err(_) => Ok(SealOutcome::Unknown(
                "seal confirmation deadline; recover before reading".into(),
            )),
        }
    }
    fn recover(&self) -> Result<SealOutcome> {
        if !self.root.join("seal.json").exists() {
            return Ok(SealOutcome::NotSealed("no committed seal".into()));
        }
        let receipt: SealReceipt = serde_json::from_slice(&fs::read(self.root.join("seal.json"))?)?;
        if receipt.run_id != self.run {
            return Err(Error::new("CorruptJournal", "run mismatch"));
        }
        super::verify_receipt(&self.root, &receipt)?;
        Ok(SealOutcome::Sealed(receipt))
    }
    fn read(&self, receipt: &SealReceipt, offset: usize, limit: usize) -> Result<EventPage> {
        read_page(&self.root, receipt, offset, limit)
    }
    fn output(&self, receipt: &SealReceipt, stream: OutputStream) -> Result<Vec<u8>> {
        super::verify_receipt(&self.root, receipt)?;
        let f = receipt
            .files
            .iter()
            .find(|f| f.name == stream.name())
            .ok_or_else(|| Error::new("CorruptJournal", "missing output"))?;
        super::checked_file(&self.root.join(&f.name), &f.blob)
    }
    fn health(&self) -> CaptureHealth {
        self.shared.health.lock().unwrap().clone()
    }
}
pub fn read_page(
    root: &Path,
    receipt: &SealReceipt,
    offset: usize,
    limit: usize,
) -> Result<EventPage> {
    if limit == 0 {
        return Err(Error::new("InvalidBudget", "zero page size"));
    }
    super::verify_receipt(root, receipt)?;
    let mut file = File::open(root.join("events.log"))?;
    let mut count = 0;
    let mut items = Vec::new();
    let mut more = false;
    while let Some(bytes) = p::wire::read_bytes(&mut file, p::MAX_FRAME)? {
        let Packet::Event(envelope) = p::wire::decode(&bytes)? else {
            return Err(Error::new("CorruptJournal", "non-event"));
        };
        if count >= offset {
            if items.len() == limit {
                more = true;
                break;
            }
            items.push(envelope);
        }
        count += 1;
    }
    Ok(EventPage {
        next: more.then_some(offset + items.len()),
        items,
    })
}
impl Drop for LocalCapture {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        let _ = fs::remove_file(&self.endpoint);
    }
}
