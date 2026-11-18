//! Per-project resident native HIR workers. Compiler input bytes are still
//! frozen and validated on every request; cancellation kills the owned group.
use crate::{Error, Result, control::CaptureControl, model::digest};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};
struct Worker {
    child: Child,
    input: ChildStdin,
    replies: mpsc::Receiver<std::result::Result<Value, String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Worker {
    fn start(provider: &Path, log: &Path) -> Result<Self> {
        // Linux binds PDEATHSIG to the spawning *thread*. MCP request threads
        // are short-lived, so the stdout reader owns spawning and stays alive
        // for the entire native session.
        let provider = provider.to_path_buf();
        let log = log.to_path_buf();
        let (ready, started) = mpsc::sync_channel(0);
        let (send, replies) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let (mut child, output) = match Self::spawn(&provider, &log) {
                Ok(value) => value,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            let input = child.stdin.take().unwrap();
            if let Err(undelivered) = ready.send(Ok((child, input))) {
                if let Ok((mut child, _)) = undelivered.0 {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                }
                return;
            }
            let mut stream = BufReader::new(output);
            loop {
                let mut line = String::new();
                let result = (&mut stream).take(1024 * 1024 + 1).read_line(&mut line);
                let value = match result {
                    Ok(0) => break,
                    Ok(_) if !line.ends_with('\n') => {
                        Err("native response exceeded frame limit".into())
                    }
                    Ok(_) => serde_json::from_str(&line).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let bad = value.is_err();
                if send.send(value).is_err() || bad {
                    break;
                }
            }
        });
        match started.recv() {
            Ok(Ok((child, input))) => Ok(Self {
                child,
                input,
                replies,
                reader: Some(reader),
            }),
            Ok(Err(error)) => {
                let _ = reader.join();
                Err(error)
            }
            Err(_) => {
                let _ = reader.join();
                Err(Error::new(
                    "ProviderTransport",
                    "native worker failed to start",
                ))
            }
        }
    }
    fn spawn(provider: &Path, log: &Path) -> Result<(Child, std::process::ChildStdout)> {
        let mut command = Command::new(provider);
        command
            .arg("codequest-serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)?,
            );
        let parent = std::process::id() as i32;
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let output = child.stdout.take().unwrap();
        Ok((child, output))
    }
    fn request(&mut self, root: &Path, output: &Path, control: &CaptureControl) -> Result<()> {
        let project = root.join("rust-project.json");
        let crates = std::fs::read(&project)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["crates"].as_array().map(Vec::len))
            .unwrap_or(0);
        let seconds =
            timeout_seconds(std::env::var("CQ_HIR_TIMEOUT_SECS").ok().as_deref(), crates)?;
        self.request_with_deadline(root, output, control, Duration::from_secs(seconds))
    }
    fn request_with_deadline(
        &mut self,
        root: &Path,
        output: &Path,
        control: &CaptureControl,
        deadline: Duration,
    ) -> Result<()> {
        if control.is_cancelled() {
            return Err(Error::new("Cancelled", "native HIR request cancelled"));
        }
        let id = cq_capture_protocol::new_id()?;
        let request =
            json!({"protocol":"codequest-session","version":1,"id":id,"root":root,"output":output});
        writeln!(self.input, "{}", serde_json::to_string(&request)?)
            .and_then(|_| self.input.flush())
            .map_err(|e| Error::new("ProviderTransport", e.to_string()))?;
        let started = Instant::now();
        loop {
            if control.is_cancelled() {
                return Err(Error::new("Cancelled", "native HIR request cancelled"));
            }
            if started.elapsed() > deadline {
                return Err(Error::new("ProviderTimeout", "native HIR request deadline"));
            }
            match self.replies.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(reply)) => {
                    if reply["id"] != id
                        || reply["protocol"] != "codequest-session"
                        || reply["version"] != 1
                    {
                        return Err(Error::new(
                            "InvalidProviderOutput",
                            "stale/malformed native session reply",
                        ));
                    }
                    if reply["state"] == "progress" {
                        if let Ok(mut log) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(output.with_extension("progress.jsonl"))
                        {
                            let _ = writeln!(log, "{}", reply);
                        }
                        continue;
                    }
                    if reply["ok"] != true {
                        return Err(Error::new(
                            "ProviderFailed",
                            reply["error"].as_str().unwrap_or("HIR worker failed"),
                        ));
                    }
                    return Ok(());
                }
                Ok(Err(error)) => return Err(Error::new("InvalidProviderOutput", error)),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::new("ProviderTransport", "native HIR worker exited"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}
fn timeout_seconds(config: Option<&str>, crates: usize) -> Result<u64> {
    match config {
        Some(v) => v.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
            Error::new(
                "InvalidProviderBudget",
                "CQ_HIR_TIMEOUT_SECS must be a positive integer",
            )
        }),
        None => Ok(900u64.saturating_add((crates as u64).saturating_mul(20))),
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
type Slot = Arc<Mutex<Option<Worker>>>;
static WORKERS: OnceLock<Mutex<BTreeMap<String, Slot>>> = OnceLock::new();
pub fn export(provider: &Path, root: &Path, output: &Path, control: &CaptureControl) -> Result<()> {
    if control.is_cancelled() {
        return Err(Error::new("Cancelled", "native HIR request cancelled"));
    }
    let key = digest(&serde_json::to_vec(&(
        provider,
        root,
        digest(&std::fs::read(provider)?),
    ))?);
    let slot = {
        let mut workers = WORKERS.get_or_init(Default::default).lock().unwrap();
        if workers.len() >= 4
            && !workers.contains_key(&key)
            && let Some(first) = workers.keys().next().cloned()
        {
            workers.remove(&first);
        }
        workers
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut slot = slot.lock().unwrap();
    if slot.is_none() {
        *slot = Some(Worker::start(
            provider,
            &output.with_extension("session.stderr"),
        )?);
    }
    let mut result = slot.as_mut().unwrap().request(root, output, control);
    if matches!(
        &result,
        Err(Error::Domain {
            code: "ProviderTransport",
            ..
        })
    ) && !control.is_cancelled()
    {
        // One reconnect for transport failure only. Invalid semantic output,
        // cancellation and timeouts fail closed without replaying requests.
        *slot = None;
        *slot = Some(Worker::start(
            provider,
            &output.with_extension("session.stderr"),
        )?);
        result = slot.as_mut().unwrap().request(root, output, control);
    }
    if result.is_err() {
        *slot = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hir_deadline_scales_with_observed_crates_and_validates_override() {
        assert_eq!(timeout_seconds(None, 0).unwrap(), 900);
        assert_eq!(timeout_seconds(None, 100).unwrap(), 2900);
        assert_eq!(timeout_seconds(Some("123"), 100).unwrap(), 123);
        for v in ["0", "", "-1", "not-a-number"] {
            assert!(timeout_seconds(Some(v), 100).is_err());
        }
    }
    use std::os::unix::fs::PermissionsExt;
    fn fixture(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider");
        std::fs::write(
            &path,
            format!("#!/usr/bin/env python3\nimport sys,json,time,pathlib,subprocess\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }
    const REPLY: &str = "for line in sys.stdin:\n r=json.loads(line); print(json.dumps(dict(r,ok=True)),flush=True)";
    fn code(error: Error) -> &'static str {
        match error {
            Error::Domain { code, .. } => code,
            _ => "unexpected",
        }
    }
    #[test]
    fn worker_survives_request_thread_exit_and_replies_twice() {
        let (dir, provider) = fixture(REPLY);
        let log = dir.path().join("stderr");
        let mut worker = std::thread::spawn(move || Worker::start(&provider, &log).unwrap())
            .join()
            .unwrap();
        for _ in 0..2 {
            worker
                .request(
                    dir.path(),
                    &dir.path().join("out"),
                    &CaptureControl::default(),
                )
                .unwrap();
        }
    }
    #[test]
    fn malformed_stale_wrong_version_and_failed_replies_fail_closed() {
        for (statement, expected) in [
            ("print('not json',flush=True)", "InvalidProviderOutput"),
            (
                "print(json.dumps(dict(r,id='stale',ok=True)),flush=True)",
                "InvalidProviderOutput",
            ),
            (
                "print(json.dumps(dict(r,version=2,ok=True)),flush=True)",
                "InvalidProviderOutput",
            ),
            (
                "print(json.dumps(dict(r,protocol='other',ok=True)),flush=True)",
                "InvalidProviderOutput",
            ),
            (
                "print(json.dumps(dict(r,ok=False,error='semantic failure')),flush=True)",
                "ProviderFailed",
            ),
            ("print('x'*1048577,flush=True)", "InvalidProviderOutput"),
        ] {
            let (dir, provider) = fixture(&format!(
                "for line in sys.stdin:\n r=json.loads(line); {statement}"
            ));
            let mut worker = Worker::start(&provider, &dir.path().join("stderr")).unwrap();
            let error = worker
                .request(
                    dir.path(),
                    &dir.path().join("out"),
                    &CaptureControl::default(),
                )
                .unwrap_err();
            assert_eq!(code(error), expected, "{statement}");
        }
    }
    #[test]
    fn eof_is_a_transport_failure() {
        let (dir, provider) = fixture("sys.stdin.readline()");
        let mut worker = Worker::start(&provider, &dir.path().join("stderr")).unwrap();
        assert_eq!(
            code(
                worker
                    .request(
                        dir.path(),
                        &dir.path().join("out"),
                        &CaptureControl::default()
                    )
                    .unwrap_err()
            ),
            "ProviderTransport"
        );
    }
    #[test]
    fn timeout_and_cancel_kill_owned_worker() {
        for cancel in [false, true] {
            let (dir, provider) = fixture("sys.stdin.readline();time.sleep(60)");
            let mut worker = Worker::start(&provider, &dir.path().join("stderr")).unwrap();
            let pid = worker.child.id() as i32;
            let control = CaptureControl::default();
            let trigger = control.clone();
            let thread = std::thread::spawn(move || {
                if cancel {
                    std::thread::sleep(Duration::from_millis(40));
                    trigger.cancel();
                }
            });
            let error = worker
                .request_with_deadline(
                    dir.path(),
                    &dir.path().join("out"),
                    &control,
                    Duration::from_millis(150),
                )
                .unwrap_err();
            assert_eq!(
                code(error),
                if cancel {
                    "Cancelled"
                } else {
                    "ProviderTimeout"
                }
            );
            drop(worker);
            thread.join().unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        }
    }
    #[test]
    fn reconnects_once_after_transport_failure() {
        let (dir, provider) = fixture(
            "marker=pathlib.Path(sys.argv[0]+'.started')\nif not marker.exists():\n marker.touch();sys.stdin.readline();sys.exit(0)\nfor line in sys.stdin:\n r=json.loads(line);print(json.dumps(dict(r,ok=True)),flush=True)",
        );
        export(
            &provider,
            dir.path(),
            &dir.path().join("out"),
            &CaptureControl::default(),
        )
        .unwrap();
    }
    #[test]
    fn reconnect_has_a_fixed_limit_and_semantic_failure_is_not_retried() {
        for semantic in [false, true] {
            let (dir, provider) = fixture(&format!(
                "marker=pathlib.Path(sys.argv[0]+'.starts')\nwith marker.open('a') as f:f.write('1')\nline=sys.stdin.readline()\n{}",
                if semantic {
                    "r=json.loads(line);print(json.dumps(dict(r,ok=False,error='rejected')),flush=True)"
                } else {
                    "sys.exit(0)"
                }
            ));
            let error = export(
                &provider,
                dir.path(),
                &dir.path().join("out"),
                &CaptureControl::default(),
            )
            .unwrap_err();
            assert_eq!(
                code(error),
                if semantic {
                    "ProviderFailed"
                } else {
                    "ProviderTransport"
                }
            );
            assert_eq!(
                std::fs::read(provider.with_extension("starts"))
                    .unwrap()
                    .len(),
                if semantic { 1 } else { 2 }
            );
        }
    }
}
