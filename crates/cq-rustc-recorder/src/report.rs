use cq_capture_protocol::{self as p, Envelope, Event, ExitObservation, Packet};
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

pub struct Reporter {
    endpoint: Option<std::path::PathBuf>,
    run: String,
    id: String,
    seq: u64,
    stream: Option<UnixStream>,
    spent: Duration,
    total: Duration,
    per: Duration,
    max: usize,
    errors: Vec<String>,
}
impl Reporter {
    pub fn from_env() -> Self {
        let value = |key: &str, default| {
            std::env::var(key)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(default)
        };
        Self {
            endpoint: std::env::var_os(p::ENDPOINT_ENV).map(Into::into),
            run: std::env::var(p::RUN_ENV).unwrap_or_default(),
            id: p::new_id()
                .unwrap_or_else(|_| format!("failed-{}-{}", std::process::id(), p::now_ms())),
            seq: 0,
            stream: None,
            spent: Duration::ZERO,
            total: Duration::from_millis(value("CQ_CAPTURE_REPORT_TOTAL_MS", 1000)),
            per: Duration::from_millis(value("CQ_CAPTURE_REPORT_MS", 200)),
            max: value("CQ_CAPTURE_FRAME_BYTES", p::MAX_FRAME as u64) as usize,
            errors: vec![],
        }
    }
    pub fn send(&mut self, event: Event) {
        let envelope = Envelope {
            protocol_version: p::VERSION,
            run_id: self.run.clone(),
            invocation_id: self.id.clone(),
            seq: self.seq,
            time_ms: p::now_ms(),
            event,
        };
        self.seq += 1;
        let start = Instant::now();
        let result = self.report(&envelope);
        self.spent += start.elapsed();
        if let Err(e) = result {
            if self.errors.len() < 16 {
                self.errors.push(e.to_string());
            }
            self.stream = None;
        }
    }
    pub fn finish(&mut self, exit: ExitObservation) {
        self.send(Event::InvocationFinished {
            exit,
            reporting_errors: self.errors.clone(),
        })
    }
    fn report(&mut self, envelope: &Envelope) -> io::Result<()> {
        let remaining = self.total.saturating_sub(self.spent).min(self.per);
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "reporting budget exceeded",
            ));
        }
        let endpoint = self.endpoint.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "capture endpoint missing")
        })?;
        let bytes =
            serde_json::to_vec(&Packet::Event(envelope.clone())).map_err(io::Error::other)?;
        let deadline = Instant::now() + remaining;
        // A lost ACK may be retried once, using exactly the same serialized event.
        for attempt in 0..2 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if self.stream.is_none() {
                self.stream = Some(connect(endpoint, left)?);
            }
            let stream = self.stream.as_mut().unwrap();
            let mut io = crate::deadline::DeadlineIo::new(stream, deadline)?;
            let sent = p::wire::write_bytes(&mut io, &bytes, self.max)
                .and_then(|_| p::wire::read_bytes(&mut io, self.max))
                .and_then(|b| {
                    b.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing ACK"))
                })
                .and_then(|b| p::wire::decode(&b));
            match sent {
                Ok(Packet::Ack(ack))
                    if ack.run_id == self.run
                        && ack.invocation_id == self.id
                        && ack.seq == envelope.seq
                        && ack.accepted =>
                {
                    return Ok(());
                }
                Ok(_) => return Err(io::Error::other("event rejected or invalid ACK")),
                Err(_) if attempt == 0 => self.stream = None,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "report deadline"))
    }
}
fn connect(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (a, b) in addr.sun_path.iter_mut().zip(bytes) {
        *a = *b as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let result = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&addr) as _,
        )
    };
    if result < 0 {
        let err = io::Error::last_os_error();
        if ![Some(libc::EINPROGRESS), Some(libc::EAGAIN)].contains(&err.raw_os_error()) {
            return Err(err);
        }
        let mut pollfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let n = unsafe {
            libc::poll(
                &mut pollfd,
                1,
                timeout.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if n <= 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect deadline"));
        }
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
