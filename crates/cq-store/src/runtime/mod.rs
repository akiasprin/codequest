//! Local IPC keeps one native Database owner across CLI, MCP and REPL clients.
//! Frames are bounded and the socket is accessible only by the current user.
use crate::{
    facts::writer::Revision,
    revisions::{StoreHost, View},
};
use cq_core::{Error, Result, model::digest, ports::facts::*};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const MAX_FRAME: usize = 16 * 1024 * 1024;
fn socket(root: &Path) -> PathBuf {
    std::env::temp_dir().join(format!(
        "codequest-{}-{}.sock",
        unsafe { libc::geteuid() },
        &digest(root.as_os_str().as_encoded_bytes())[..24]
    ))
}
fn send(s: &mut UnixStream, v: &Value) -> Result<()> {
    let b = serde_json::to_vec(v)?;
    if b.len() > MAX_FRAME {
        return Err(Error::new("IpcBudget", "frame exceeds 16 MiB"));
    }
    s.write_all(&(b.len() as u32).to_be_bytes())?;
    s.write_all(&b)?;
    s.flush()?;
    Ok(())
}
fn receive(s: &mut UnixStream) -> Result<Value> {
    let mut n = [0; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if n > MAX_FRAME {
        return Err(Error::new("IpcBudget", "frame exceeds 16 MiB"));
    }
    let mut b = vec![0; n];
    s.read_exact(&mut b)?;
    Ok(serde_json::from_slice(&b)?)
}
#[derive(Clone)]
pub struct Client {
    root: PathBuf,
}
impl Client {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: root.canonicalize()?,
        })
    }
    fn connect(&self) -> Result<UnixStream> {
        let path = socket(&self.root);
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
        let executable = std::env::var_os("CQ_STORE_HOST")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|p| p.join("cq-store-host")))
                    .filter(|p| p.is_file())
            })
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent()?.parent().map(|p| p.join("cq-store-host")))
                    .filter(|p| p.is_file())
            })
            .or_else(|| {
                std::env::current_exe().ok().and_then(|p| {
                    p.ancestors()
                        .skip(1)
                        .take(7)
                        .map(|p| p.join("cq-store-host"))
                        .find(|p| p.is_file())
                })
            })
            .ok_or_else(|| {
                Error::new(
                    "MissingStoreHost",
                    "build cq-store-host alongside cq or set CQ_STORE_HOST",
                )
            })?;
        let mut child = std::process::Command::new(executable)
            .arg("--store")
            .arg(&self.root)
            // A terminal interrupt belongs to its client, not every user of
            // the shared host. Keep the daemon outside that foreground group.
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if let Ok(s) = UnixStream::connect(&path) {
                return Ok(s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(Error::new(
            "StoreHostUnavailable",
            "host startup failed; run cq-store-host --store PATH for diagnostics",
        ))
    }
    pub fn call(&self, v: Value) -> Result<Value> {
        if !v.is_object() {
            return Err(Error::new("IpcProtocol", "request must be an object"));
        }
        let retry = matches!(
            v["op"].as_str(),
            Some(
                "ping"
                    | "get"
                    | "scan"
                    | "count"
                    | "source"
                    | "open"
                    | "reconnect"
                    | "begin_read"
                    | "validate_cursor"
                    | "resolve"
                    | "pin_cursor"
                    | "release"
            )
        );
        for attempt in 0..3 {
            let result = self.call_once(v.clone());
            if retry && matches!(&result, Err(Error::Io(_))) && attempt < 2 {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            return result;
        }
        unreachable!()
    }
    fn call_once(&self, mut v: Value) -> Result<Value> {
        v["_client_pid"] = json!(std::process::id());
        let mut s = self.connect()?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        send(&mut s, &v)?;
        let response = receive(&mut s)?;
        if let Some(e) = response.get("error") {
            return Err(Error::Remote {
                code: e["code"].as_str().unwrap_or("StoreHost").into(),
                message: e["message"].as_str().unwrap_or("IPC error").into(),
            });
        }
        Ok(response["result"].clone())
    }
    fn release_existing(&self, id: &str) {
        if let Ok(mut s) = UnixStream::connect(socket(&self.root)) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            if send(&mut s, &json!({"op":"release","view":id})).is_ok() {
                let _ = receive(&mut s);
            }
        }
    }
    pub fn open(&self, project: &str, revision: Option<&str>) -> Result<RemoteView> {
        self.open_kind(project, revision, false)
    }
    pub fn open_session(&self, project: &str, revision: Option<&str>) -> Result<RemoteView> {
        self.open_kind(project, revision, true)
    }
    fn open_kind(
        &self,
        project: &str,
        revision: Option<&str>,
        session: bool,
    ) -> Result<RemoteView> {
        let response = self
            .call(json!({"op":"open","project":project,"revision":revision,"session":session}))?;
        self.view(project, response)
    }
    fn view(&self, project: &str, response: Value) -> Result<RemoteView> {
        Ok(RemoteView {
            client: self.clone(),
            project: project.into(),
            revision: response["revision"]
                .as_str()
                .ok_or_else(|| Error::new("IpcProtocol", "missing revision"))?
                .into(),
            metadata: response["metadata"].clone(),
            state: Mutex::new((
                response["view"]
                    .as_str()
                    .ok_or_else(|| Error::new("IpcProtocol", "missing view"))?
                    .into(),
                response["lease"]
                    .as_str()
                    .ok_or_else(|| Error::new("IpcProtocol", "missing lease"))?
                    .into(),
            )),
            stats: Mutex::new(ReadStats::default()),
            session: response["session"].as_bool().unwrap_or(false),
            deadline: Mutex::new(None),
        })
    }
    pub fn publish(&self, r: &Revision, expected: Option<&str>) -> Result<CommitResult> {
        match self
            .call(json!({"op":"publish","project":r.project,"manifest":r,"expected":expected}))
        {
            Ok(v) => Ok(serde_json::from_value(v)?),
            Err(e) => Ok(CommitResult {
                revision: r.revision.clone(),
                outcome: CommitOutcome::Unknown,
                detail: Some(e.to_string()),
            }),
        }
    }
}
pub struct RemoteView {
    deadline: Mutex<Option<Instant>>,
    client: Client,
    project: String,
    revision: String,
    metadata: Value,
    state: Mutex<(String, String)>,
    stats: Mutex<ReadStats>,
    session: bool,
}
impl RemoteView {
    fn request(&self, mut v: Value) -> Result<Value> {
        if let Some(deadline) = *self.deadline.lock().unwrap() {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            if left == 0 {
                return Err(Error::new("QueryTimeout", "request budget exhausted"));
            }
            v["timeout_ms"] = json!(left);
        }
        let mut state = self.state.lock().unwrap();
        v["view"] = json!(state.0);
        let response = match self.client.call(v.clone()) {
            Err(e) if e.code() == "ViewDisconnected" => {
                let r=self.client.call(json!({"op":"reconnect","project":self.project,"revision":self.revision,"lease":state.1,"session":self.session}))?;
                state.0 = r["view"].as_str().unwrap().into();
                state.1 = r["lease"].as_str().unwrap().into();
                v["view"] = json!(state.0);
                self.client.call(v)?
            }
            other => other?,
        };
        if let Some(s) = response.get("stats") {
            *self.stats.lock().unwrap() = serde_json::from_value(s.clone())?;
        }
        Ok(response["value"].clone())
    }
}
impl Drop for RemoteView {
    fn drop(&mut self) {
        let state = self.state.lock().unwrap();
        self.client.release_existing(&state.0);
    }
}
impl FactReader for RemoteView {
    fn set_timeout(&self, timeout_ms: u64) {
        *self.deadline.lock().unwrap() = Some(Instant::now() + Duration::from_millis(timeout_ms));
    }
    fn request_view(&self) -> Result<Option<Box<dyn FactReader>>> {
        if !self.session {
            return Ok(None);
        }
        for attempt in 0..2 {
            let id = self.state.lock().unwrap().0.clone();
            match self.client.call(json!({"op":"begin_read","view":id})) {
                Ok(response) => {
                    return Ok(Some(Box::new(self.client.view(&self.project, response)?)));
                }
                Err(e) if e.code() == "ViewDisconnected" && attempt == 0 => {
                    self.request(json!({"op":"count","group":"all:unit"}))?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            "ViewDisconnected",
            "session could not reconnect",
        ))
    }

    fn revision(&self) -> &str {
        &self.revision
    }
    fn project(&self) -> &str {
        &self.project
    }
    fn metadata(&self) -> &Value {
        &self.metadata
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<FactRow>> {
        Ok(serde_json::from_value(
            self.request(json!({"op":"get","family":family,"id":id}))?,
        )?)
    }
    fn scan(&self, group: &str, after: Option<&str>, limit: usize) -> Result<FactBatch> {
        Ok(serde_json::from_value(self.request(
            json!({"op":"scan","group":group,"after":after,"limit":limit}),
        )?)?)
    }
    fn count(&self, group: &str) -> Result<u64> {
        self.request(json!({"op":"count","group":group}))?
            .as_u64()
            .ok_or_else(|| Error::new("IpcProtocol", "invalid count"))
    }
    fn source_range(&self, file: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        if start > end || end - start > 8 * 1024 * 1024 {
            return Err(Error::new("SourceRange", "invalid or oversized range"));
        }
        let mut out = vec![];
        let mut position = start;
        while position < end {
            let next = (position + 1024 * 1024).min(end);
            let part: Vec<u8> = serde_json::from_value(
                self.request(json!({"op":"source","file":file,"start":position,"end":next}))?,
            )?;
            out.extend(part);
            position = next;
        }
        Ok(out)
    }
    fn stats(&self) -> ReadStats {
        self.stats.lock().unwrap().clone()
    }
}
struct HeldView {
    view: Arc<View>,
    pid: u32,
    last: Instant,
}
struct Server {
    root: PathBuf,
    hosts: Mutex<BTreeMap<String, Arc<StoreHost>>>,
    views: Mutex<BTreeMap<String, HeldView>>,
}
impl Server {
    fn prune_views(&self) {
        self.views.lock().unwrap().retain(|_, held| {
            let known_owner = held.pid != 0;
            let dead = known_owner
                && unsafe { libc::kill(held.pid as i32, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if dead {
                return false;
            }
            if known_owner && held.view.is_session() {
                // Idle interactive sessions are still live clients. Renew
                // only valid leases; revocation/expiration is never undone.
                return match held.view.renew_session_lease() {
                    Ok(()) => true,
                    Err(e) => e.code() != "CursorExpired",
                };
            }
            held.last.elapsed() < Duration::from_secs(120)
        });
    }
    fn host(&self, project: &str) -> Result<Arc<StoreHost>> {
        let mut hosts = self.hosts.lock().unwrap();
        if let Some(h) = hosts.get(project) {
            return Ok(h.clone());
        }
        if hosts.len() >= 3 {
            let old = hosts
                .iter()
                .find(|(_, h)| Arc::strong_count(h) == 1)
                .map(|(p, _)| p.clone());
            if let Some(p) = old {
                hosts.remove(&p);
            } else {
                return Err(Error::new(
                    "StoreBusy",
                    "three projects have active views; release one before opening another",
                ));
            }
        }
        let host = StoreHost::open(&self.root, project)?;
        hosts.insert(project.into(), host.clone());
        Ok(host)
    }
    fn request(&self, v: Value) -> Result<Value> {
        let s = |k: &str| {
            v[k].as_str()
                .ok_or_else(|| Error::new("IpcProtocol", format!("missing {k}")))
        };
        let op = s("op")?;
        if op == "ping" {
            return Ok(json!({"pid":std::process::id(),"format":2}));
        }
        if op == "delete" {
            let project = s("project")?;
            let views = self.views.lock().unwrap();
            if views.values().any(|v| v.view.project() == project) {
                return Err(Error::new(
                    "IndexBusy",
                    "release active project views before deletion",
                ));
            }
            let _objects =
                crate::revisions::retention::ObjectGate::lock(&self.root.join("format-2"))?;
            let mut hosts = self.hosts.lock().unwrap();
            if hosts.get(project).is_some_and(|h| Arc::strong_count(h) > 1) {
                return Err(Error::new("IndexBusy", "project has an active operation"));
            }
            let path = self
                .root
                .join("format-2/projects")
                .join(digest(project.as_bytes()));
            drop(hosts.remove(project));
            if path.exists() {
                let deleted = self.root.join("format-2/deleted-projects").join(format!(
                    "{}-{}",
                    digest(project.as_bytes()),
                    cq_capture_protocol::new_id()?
                ));
                std::fs::create_dir_all(deleted.parent().unwrap())?;
                std::fs::rename(path, &deleted)?;
                std::fs::File::open(deleted.parent().unwrap())?.sync_all()?;
            }
            return Ok(Value::Null);
        }
        if op == "begin_read" {
            let source = {
                let mut views = self.views.lock().unwrap();
                let held = views
                    .get_mut(s("view")?)
                    .ok_or_else(|| Error::new("ViewDisconnected", "session is disconnected"))?;
                held.last = Instant::now();
                held.view.clone()
            };
            let read = Arc::new(source.request()?);
            let id = cq_capture_protocol::new_id()?;
            let response = json!({"view":id,"lease":read.lease(),"revision":read.revision(),"metadata":read.metadata(),"session":false});
            let mut views = self.views.lock().unwrap();
            if views.len() >= 128 {
                return Err(Error::new("StoreBusy", "active view budget"));
            }
            views.insert(
                id,
                HeldView {
                    view: read,
                    pid: v["_client_pid"].as_u64().unwrap_or(0) as u32,
                    last: Instant::now(),
                },
            );
            return Ok(response);
        }
        if op == "release" {
            self.views.lock().unwrap().remove(s("view")?);
            return Ok(Value::Null);
        }
        if [
            "open",
            "reconnect",
            "publish",
            "resolve",
            "rebuild",
            "compact",
            "export",
            "pin_cursor",
            "validate_cursor",
            "retention_preview",
            "retention_apply",
            "collect_epochs",
            "diagnostics",
        ]
        .contains(&op)
        {
            let h = self.host(s("project")?)?;
            return match op {
                "diagnostics" => h.diagnostics(v["revision"].as_str()),
                "pin_cursor" => {
                    h.pin_cursor(s("cursor")?, s("revision")?)?;
                    Ok(Value::Null)
                }
                "validate_cursor" => {
                    h.validate_lease(&format!("cursor-{}", s("cursor")?), s("revision")?)?;
                    Ok(Value::Null)
                }
                "retention_preview" => {
                    h.plan_response(&h.preview_retention(v["keep"].as_u64().unwrap_or(8) as usize)?)
                }
                "retention_apply" => {
                    let plan = if let Some(id) = v["plan_file"].as_str() {
                        h.read_plan_reference(id)?
                    } else {
                        serde_json::from_value(v["plan"].clone())?
                    };
                    h.plan_response(&h.apply_retention(&plan)?)
                }
                "collect_epochs" => Ok(json!(h.collect_epochs()?)),
                "open" | "reconnect" => {
                    if op == "reconnect" {
                        h.validate_lease(s("lease")?, s("revision")?)?;
                    }
                    let view = Arc::new(if v["session"] == true {
                        h.open_session(v["revision"].as_str())?
                    } else {
                        h.open_view(v["revision"].as_str())?
                    });
                    let id = cq_capture_protocol::new_id()?;
                    let result = json!({"view":id,"lease":view.lease(),"revision":view.revision(),"metadata":view.metadata(),"session":view.is_session()});
                    let mut views = self.views.lock().unwrap();
                    if views.len() >= 128 {
                        return Err(Error::new("StoreBusy", "active view limit"));
                    }
                    views.insert(
                        id,
                        HeldView {
                            view,
                            pid: v["_client_pid"].as_u64().unwrap_or(0) as u32,
                            last: Instant::now(),
                        },
                    );
                    Ok(result)
                }
                "publish" => {
                    let r: Revision = serde_json::from_value(v["manifest"].clone())?;
                    Ok(serde_json::to_value(h.publish(&r, v["expected"].as_str()))?)
                }
                "resolve" => Ok(serde_json::to_value(h.resolve(s("revision")?)?)?),
                "rebuild" => Ok(json!({"epoch":h.rebuild()?})),
                "compact" => h.compact(),
                "export" => {
                    let r = h.open_view(v["revision"].as_str())?;
                    let snapshot = crate::facts::writer::export(&h.objects, r.manifest())?;
                    let dir = self.root.join("format-2/exports");
                    std::fs::create_dir_all(&dir)?;
                    let p = dir.join(cq_capture_protocol::new_id()?);
                    save_export(&p, &snapshot)?;
                    Ok(json!({"path":p}))
                }
                _ => unreachable!(),
            };
        }
        let view = {
            let mut views = self.views.lock().unwrap();
            let held = views.get_mut(s("view")?).ok_or_else(|| {
                Error::new("ViewDisconnected", "reconnect the same leased revision")
            })?;
            held.last = Instant::now();
            held.view.clone()
        };
        let read = if view.is_session() {
            Some(view.request()?)
        } else {
            None
        };
        let view = read.as_ref().unwrap_or(view.as_ref());
        if let Some(timeout) = v["timeout_ms"].as_u64() {
            view.set_timeout(timeout);
        }
        let value = match op {
            "get" => serde_json::to_value(view.get(s("family")?, s("id")?)?)?,
            "scan" => serde_json::to_value(
                view.scan(
                    s("group")?,
                    v["after"].as_str(),
                    v["limit"]
                        .as_u64()
                        .ok_or_else(|| Error::new("InvalidBudget", "limit"))?
                        as usize,
                )?,
            )?,
            "count" => json!(view.count(s("group")?)?),
            "source" => json!(
                view.source_range(
                    s("file")?,
                    v["start"]
                        .as_u64()
                        .ok_or_else(|| Error::new("SourceRange", "start"))?,
                    v["end"]
                        .as_u64()
                        .ok_or_else(|| Error::new("SourceRange", "end"))?
                )?
            ),
            _ => return Err(Error::new("IpcProtocol", op)),
        };
        Ok(json!({"value":value,"stats":view.stats()}))
    }
}
fn save_export(path: &Path, s: &cq_core::model::SemanticSnapshot) -> Result<()> {
    let mut f = std::fs::File::create(path)?;
    serde_json::to_writer(&mut f, s)?;
    f.sync_all()?;
    Ok(())
}
pub fn serve(root: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("format-2-host.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    let path = socket(&root);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let server = Arc::new(Server {
        root,
        hosts: Mutex::new(BTreeMap::new()),
        views: Mutex::new(BTreeMap::new()),
    });
    use std::sync::atomic::{AtomicUsize, Ordering};
    listener.set_nonblocking(true)?;
    let active = Arc::new(AtomicUsize::new(0));
    let mut last = Instant::now();
    let mut last_prune = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                last = Instant::now();
                stream.set_read_timeout(Some(Duration::from_secs(120)))?;
                if active.load(Ordering::Acquire) >= 8 {
                    let _ = send(
                        &mut stream,
                        &json!({"error":{"code":"StoreBusy","message":"eight active store requests; retry after one completes"}}),
                    );
                    continue;
                }
                active.fetch_add(1, Ordering::AcqRel);
                let active = active.clone();
                let server = server.clone();
                std::thread::spawn(move || {
                    struct Active(Arc<AtomicUsize>);
                    impl Drop for Active {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    let _active = Active(active);
                    let response = (|| {
                        let request = receive(&mut stream)?;
                        server.request(request)
                    })();
                    let value = match response {
                        Ok(v) => json!({"result":v}),
                        Err(e) => json!({"error":{"code":e.code(),"message":e.message()}}),
                    };
                    let _ = send(&mut stream, &value);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last_prune.elapsed() > Duration::from_secs(1) {
                    server.prune_views();
                    last_prune = Instant::now();
                }
                if active.load(Ordering::Acquire) == 0
                    && server.views.lock().unwrap().is_empty()
                    && last.elapsed() > Duration::from_secs(60)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
    drop(listener);
    drop(server);
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::writer;

    #[test]
    fn idle_live_session_keeps_its_lease_and_dead_owner_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let host = StoreHost::open(dir.path(), "idle-session").unwrap();
        let revision = digest(b"idle-session-revision");
        let mut manifest = writer::Revision {
            format: 2,
            project: "idle-session".into(),
            revision: revision.clone(),
            parent: None,
            sequence: 1,
            roots: Default::default(),
            metadata: json!({"project":"idle-session","revision":revision}),
            changes: Default::default(),
            seal: String::new(),
        };
        manifest.seal().unwrap();
        assert_eq!(
            host.publish(&manifest, None).outcome,
            CommitOutcome::Committed
        );
        let session = host.open_session(None).unwrap();
        let lease = session.lease().to_owned();
        let server = Server {
            root: dir.path().to_owned(),
            hosts: Mutex::new(BTreeMap::new()),
            views: Mutex::new(BTreeMap::from([(
                "session".into(),
                HeldView {
                    view: Arc::new(session),
                    pid: std::process::id(),
                    last: Instant::now() - Duration::from_secs(121),
                },
            )])),
        };
        server.prune_views();
        host.validate_lease(&lease, &revision).unwrap();
        let read = server.views.lock().unwrap()["session"]
            .view
            .request()
            .unwrap();
        assert_eq!(read.revision(), revision);
        drop(read);

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        server.views.lock().unwrap().get_mut("session").unwrap().pid = dead_pid;
        server.prune_views();
        assert!(server.views.lock().unwrap().is_empty());
        assert_eq!(
            host.validate_lease(&lease, &revision).unwrap_err().code(),
            "CursorExpired"
        );

        let ordinary = host.open_view(None).unwrap();
        let ordinary_lease = ordinary.lease().to_owned();
        server.views.lock().unwrap().insert(
            "abandoned-read".into(),
            HeldView {
                view: Arc::new(ordinary),
                pid: std::process::id(),
                last: Instant::now() - Duration::from_secs(121),
            },
        );
        server.prune_views();
        assert!(server.views.lock().unwrap().is_empty());
        assert!(host.validate_lease(&ordinary_lease, &revision).is_err());

        let revoked = host.open_session(None).unwrap();
        host.release_lease(revoked.lease()).unwrap();
        server.views.lock().unwrap().insert(
            "revoked-session".into(),
            HeldView {
                view: Arc::new(revoked),
                pid: std::process::id(),
                last: Instant::now(),
            },
        );
        server.prune_views();
        assert!(server.views.lock().unwrap().is_empty());
    }
}
