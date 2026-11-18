//! Bounded subprocess output for semantic helpers and document extraction.
use crate::{Error, Result, control::CaptureControl};
use std::{
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
pub fn output(
    mut command: Command,
    control: &CaptureControl,
    timeout_ms: u64,
    max_bytes: usize,
) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let overflow = Arc::new(AtomicBool::new(false));
    let collect = |reader: Box<dyn Read + Send>| {
        let overflow = overflow.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut out = vec![];
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let remaining = max_bytes.saturating_sub(out.len());
                        out.extend_from_slice(&buffer[..n.min(remaining)]);
                        if n > remaining {
                            overflow.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(out)
        })
    };
    let stdout = collect(Box::new(child.stdout.take().unwrap()));
    let stderr = collect(Box::new(child.stderr.take().unwrap()));
    let start = Instant::now();
    let mut stopped = None;
    let status = loop {
        if control.is_cancelled()
            || start.elapsed() >= Duration::from_millis(timeout_ms)
            || overflow.load(Ordering::Relaxed)
        {
            stopped = Some(if control.is_cancelled() {
                "Cancelled"
            } else if overflow.load(Ordering::Relaxed) {
                "ProcessOutputLimit"
            } else {
                "ProcessDeadline"
            });
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Descendants must not keep pipes alive after the owning process exits.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let stdout = stdout
        .join()
        .map_err(|_| Error::new("ProcessReaderPanic", "stdout"))??;
    let stderr = stderr
        .join()
        .map_err(|_| Error::new("ProcessReaderPanic", "stderr"))??;
    if let Some(code) = stopped {
        return Err(Error::new(code, String::from_utf8_lossy(&stderr)));
    }
    if overflow.load(Ordering::Relaxed) {
        return Err(Error::new("ProcessOutputLimit", "subprocess output limit"));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
