use crate::{
    Result,
    control::CaptureControl,
    model::*,
    ports::{CaptureJournal, OutputStream},
};
use std::{
    io::Read,
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

pub struct ProcessResult {
    pub exit: ExitObservation,
    pub stop: Option<String>,
    pub duration_ms: u64,
}
pub fn run_cargo(
    mut cmd: Command,
    journal: Arc<dyn CaptureJournal>,
    control: &CaptureControl,
    budget: &CaptureBudget,
) -> Result<ProcessResult> {
    let start = Instant::now();
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(ProcessResult {
                exit: ExitObservation {
                    code: None,
                    signal: None,
                    spawn_error: Some(e.to_string()),
                },
                stop: None,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }
    };
    let (tx, rx) = mpsc::channel();
    for (mut reader, stream) in [
        (
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            OutputStream::Stdout,
        ),
        (Box::new(child.stderr.take().unwrap()), OutputStream::Stderr),
    ] {
        let tx = tx.clone();
        let journal = journal.clone();
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            let mut failed = None;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = journal.write_output(stream, &buffer[..n])
                            && failed.is_none()
                        {
                            failed = Some(e.to_string());
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        failed = Some(e.to_string());
                        break;
                    }
                }
            }
            let _ = tx.send(failed);
        });
    }
    drop(tx);
    let mut stop = None;
    let mut stopped_at = None;
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && stopped_at.is_none() {
            break;
        }
        if stop.is_none()
            && (control.is_cancelled()
                || start.elapsed() >= Duration::from_millis(budget.build_timeout_ms))
        {
            stop = Some(
                if control.is_cancelled() {
                    "user_cancelled"
                } else {
                    "deadline_exceeded"
                }
                .to_owned(),
            );
            stopped_at = Some(Instant::now());
            kill_group(child.id(), libc::SIGTERM);
        }
        if let Some(at) = stopped_at {
            if at.elapsed() >= Duration::from_millis(budget.cancel_grace_ms) {
                kill_group(child.id(), libc::SIGKILL);
                if status.is_none() {
                    status = Some(child.wait()?);
                }
                break;
            }
        } else if status.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let deadline = Instant::now() + Duration::from_millis(budget.drain_timeout_ms);
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(None) => {}
            _ => {
                stop.get_or_insert("output_incomplete".into());
            }
        }
    }
    let status = status.unwrap();
    Ok(ProcessResult {
        exit: ExitObservation {
            code: status.code(),
            signal: status.signal(),
            spawn_error: None,
        },
        stop,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}
pub fn kill_group(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

pub fn probe(mut cmd: Command, args: Vec<String>, timeout_ms: u64) -> ProbeObservation {
    use std::os::unix::process::ExitStatusExt;
    let command = NativeValue::new(cmd.get_program());
    let cwd = cmd
        .get_current_dir()
        .map(|p| Observed::Value(NativeValue::new(p)))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| Observed::Value(NativeValue::new(p)))
                .unwrap_or_else(|e| Observed::Unknown(e.to_string()))
        });
    let start = Instant::now();
    cmd.args(&args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProbeObservation {
                command,
                cwd,
                args,
                output: NativeValue(vec![]),
                stderr: NativeValue(vec![]),
                exit: ExitObservation {
                    code: None,
                    signal: None,
                    spawn_error: Some(e.to_string()),
                },
                duration_ms: 0,
            };
        }
    };
    let (tx, rx) = mpsc::channel();
    for (index, mut reader) in [
        (
            0,
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        ),
        (1, Box::new(child.stderr.take().unwrap())),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = reader
                .by_ref()
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes);
            let exceeded = bytes.len() > 8 * 1024 * 1024;
            if exceeded {
                bytes.truncate(8 * 1024 * 1024);
            }
            let _ = tx.send((index, bytes, exceeded));
        });
    }
    drop(tx);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < Duration::from_millis(timeout_ms) => {
                thread::sleep(Duration::from_millis(5))
            }
            _ => {
                timed_out = true;
                kill_group(child.id(), libc::SIGKILL);
                break child.wait().ok();
            }
        }
    };
    let mut outputs = [Vec::new(), Vec::new()];
    let mut exceeded = false;
    let deadline = Instant::now() + Duration::from_millis(100);
    for _ in 0..2 {
        if let Ok((i, b, large)) =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            outputs[i] = b;
            exceeded |= large;
        } else {
            timed_out = true;
        }
    }
    ProbeObservation {
        command,
        cwd,
        args,
        output: NativeValue(std::mem::take(&mut outputs[0])),
        stderr: NativeValue(std::mem::take(&mut outputs[1])),
        exit: ExitObservation {
            code: status.and_then(|s| s.code()),
            signal: status.and_then(|s| s.signal()),
            spawn_error: if timed_out {
                Some("ProbeDeadlineExceeded".into())
            } else if exceeded {
                Some("ProbeOutputLimit".into())
            } else {
                None
            },
        },
        duration_ms: start.elapsed().as_millis() as u64,
    }
}
