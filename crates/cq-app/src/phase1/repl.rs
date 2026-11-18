use super::{Options, Service};
use cq_core::{
    Error, Result,
    control::CaptureControl,
    ports::facts::FactReader,
    query::{QueryRequest, paged::Engine},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, BufRead, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Release);
}
#[derive(Clone, Default)]
struct Session {
    project: Option<String>,
    revision: Option<String>,
    context: Option<String>,
    guard: Option<Arc<cq_store::runtime::RemoteView>>,
    last: Option<(String, Value)>,
    expanded: BTreeSet<String>,
    handles: BTreeMap<String, String>,
}
impl Session {
    fn switch(
        &mut self,
        service: &Service,
        project: &str,
        revision: Option<&str>,
        context: Option<String>,
    ) -> Result<()> {
        service.store.project_info(project)?;
        let (rev, guard) = if service.store.paged_enabled(project) {
            let view = Arc::new(service.store.open_session_facts(project, revision)?);
            if let Some(context) = &context {
                Engine {
                    reader: view.as_ref(),
                    model: None,
                    control: None,
                }
                .execute(
                    &QueryRequest {
                        context: Some(context.clone()),
                        ..Default::default()
                    },
                    None,
                )?;
            }
            (view.revision().to_owned(), Some(view))
        } else {
            let s = service.snapshot(project, revision)?;
            if context.as_ref().is_some_and(|id| {
                !s.contexts
                    .iter()
                    .any(|c| &c.id == id || &c.entry_unit == id)
            }) {
                return Err(Error::new("MissingContext", "unknown context"));
            }
            (s.revision.clone(), None)
        };
        self.project = Some(project.into());
        self.revision = Some(rev);
        self.context = context;
        self.guard = guard;
        self.last = None;
        self.expanded.clear();
        self.handles.clear();
        Ok(())
    }
    fn view(&self) -> Value {
        json!({"project":self.project,"revision":self.revision,"context":self.context,"expanded":self.expanded,"handles":self.handles})
    }
    fn execute(
        &mut self,
        service: &Arc<Service>,
        line: &str,
        control: &CaptureControl,
    ) -> Result<Value> {
        let (command, original) = line.split_once(' ').unwrap_or((line, ""));
        let (first, rest) = original.split_once(' ').unwrap_or((original, ""));
        if first.starts_with('@') && !self.handles.contains_key(first) {
            return Err(Error::new("UnknownHandle", first));
        }
        let resolved = self.handles.get(first).map(|id| {
            if rest.is_empty() {
                id.clone()
            } else {
                format!("{id} {rest}")
            }
        });
        let tail = resolved.as_deref().unwrap_or(original);
        match command {
            "help" => Ok(
                json!({"session":["use PROJECT","revision head|SHA","context all|ID","view","expand @N [operation]","collapse @N","next","refresh","job ID","cancel [request ID|job ID]","quit"],"queries":["overview","search TEXT","semantic TEXT","details ID","definition ID","members ID","callees ID","callers ID","references ID","trace ID","source ID","docs","document ID","coverage"],"structured":"command { JSON arguments }","default_limit":7}),
            ),
            "projects" => Ok(json!(service.store.list_projects()?)),
            "use" => {
                self.switch(service, tail, None, None)?;
                Ok(self.view())
            }
            "revision" => {
                let p = self
                    .project
                    .clone()
                    .ok_or_else(|| Error::new("MissingProject", "use PROJECT"))?;
                let ctx = self.context.clone();
                self.switch(
                    service,
                    &p,
                    if tail == "head" { None } else { Some(tail) },
                    ctx,
                )?;
                Ok(self.view())
            }
            "context" => {
                let p = self
                    .project
                    .clone()
                    .ok_or_else(|| Error::new("MissingProject", "use PROJECT"))?;
                let rev = self.revision.clone();
                self.switch(
                    service,
                    &p,
                    rev.as_deref(),
                    if tail == "all" {
                        None
                    } else {
                        Some(tail.into())
                    },
                )?;
                Ok(self.view())
            }
            "view" => Ok(self.view()),
            "collapse" => {
                self.expanded.remove(tail);
                Ok(
                    json!({"collapsed":tail,"hidden":"facts retained; expand to disclose again","view":self.view()}),
                )
            }
            "refresh" => service.start_refresh(
                self.project
                    .as_deref()
                    .ok_or_else(|| Error::new("MissingProject", "use PROJECT"))?,
            ),
            "job" => service.job(tail, false),
            "cancel" => service.job(tail, true),
            _ => {
                let p = self
                    .project
                    .as_deref()
                    .ok_or_else(|| Error::new("MissingProject", "use PROJECT"))?;
                let (op, mut args) = if command == "next" {
                    let (op, args) = self
                        .last
                        .clone()
                        .ok_or_else(|| Error::new("NoContinuation", "no previous page"))?;
                    if args["cursor"].is_null() {
                        return Err(Error::new("NoContinuation", "previous query is complete"));
                    }
                    (op, args)
                } else if command == "expand" {
                    let (id, op) = tail.split_once(' ').unwrap_or((tail, "details"));
                    (op.into(), json!({"subject":id}))
                } else {
                    let args = if tail.starts_with('{') {
                        serde_json::from_str(tail)?
                    } else if ["search", "semantic", "search_code", "doc_search", "tour"]
                        .contains(&command)
                    {
                        json!({"text":tail})
                    } else if tail.is_empty() {
                        json!({})
                    } else {
                        json!({"subject":tail})
                    };
                    (command.into(), args)
                };
                if !args.is_object() {
                    return Err(Error::new(
                        "InvalidArguments",
                        "JSON arguments must be an object",
                    ));
                }
                if args.get("context").is_none() {
                    args["context"] = json!(self.context);
                }
                let mut value = if let Some(guard) = self.guard.as_deref() {
                    service.dispatch_from_view(p, &op, args.clone(), guard, control)?
                } else {
                    service.dispatch_controlled(
                        p,
                        &op,
                        args.clone(),
                        self.revision.as_deref(),
                        control,
                    )?
                };
                if command == "expand" {
                    self.expanded.insert(
                        tail.split_once(' ')
                            .map(|(id, _)| id)
                            .unwrap_or(tail)
                            .into(),
                    );
                }
                args["cursor"] = value["next_cursor"].clone();
                self.last = Some((op, args));
                let mut shown = serde_json::Map::new();
                for item in value["items"].as_array().into_iter().flatten() {
                    if let Some(id) = item["id"]
                        .as_str()
                        .or_else(|| item["entity"]["id"].as_str())
                        .or_else(|| item["entity_ref"].as_str())
                    {
                        let h = self
                            .handles
                            .iter()
                            .find(|(_, v)| v.as_str() == id)
                            .map(|(h, _)| h.clone())
                            .unwrap_or_else(|| format!("@{}", self.handles.len() + 1));
                        self.handles.insert(h.clone(), id.into());
                        shown.insert(h, json!(id));
                    }
                }
                if !shown.is_empty() {
                    value["handles"] = json!(shown);
                }
                Ok(value)
            }
        }
    }
}
enum Event {
    Line(String),
    End,
    Complete(String, Box<Session>, Result<Value>),
}
fn output(v: Value) -> Result<()> {
    println!("{}", serde_json::to_string(&v)?);
    io::stdout().flush()?;
    Ok(())
}
/// Input/control stays live while the worker executes. Cancellation is scoped to
/// an exact request generation, so a late cancel cannot hit a reused connection.
pub fn run(service: Arc<Service>, options: &Options) -> Result<()> {
    let terminal = io::stdin().is_terminal();
    let mut session = Session::default();
    if let Some(p) = options.text("--project") {
        let _opening = super::progress::Progress::start(format!("连接 StoreHost 并打开项目 {p}"));
        session.switch(&service, &p, None, None)?;
    }
    if options.yes("--watch")
        && let Some(p) = &session.project
    {
        service.watch(p)?;
    }
    if terminal {
        eprintln!(
            "CodeQuest 已就绪 · 项目 {} · 版本 {}",
            session.project.as_deref().unwrap_or("未选择"),
            session
                .revision
                .as_deref()
                .map(|r| &r[..r.len().min(12)])
                .unwrap_or("—")
        );
        eprintln!(
            "输入 help 查看命令；quit 退出；Ctrl+C 取消查询。\n日志：{}",
            service.store.root.join("store-host.log").display()
        );
        prompt(&session)?;
    }
    let old_signal = if terminal {
        Some(unsafe { libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t) })
    } else {
        None
    };
    let (tx, rx) = mpsc::channel();
    let input = tx.clone();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if input.send(Event::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = input.send(Event::End);
    });
    let mut queued = VecDeque::new();
    let mut active: Option<(String, CaptureControl)> = None;
    let mut ended = false;
    loop {
        if INTERRUPTED.swap(false, Ordering::AcqRel) {
            if let Some((id, c)) = &active {
                c.cancel();
                output(json!({"request":id,"state":"cancellation_requested"}))?;
            } else {
                break;
            }
        }
        if active.is_none() {
            if let Some(line) = queued.pop_front() {
                let line: String = line;
                let line = line.trim().to_owned();
                if line.is_empty() {
                    if terminal {
                        prompt(&session)?;
                    }
                    continue;
                }
                if ["quit", "exit", ":q"].contains(&line.as_str()) {
                    break;
                }
                let id = format!("q-{}", cq_capture_protocol::new_id()?);
                let control = CaptureControl::default();
                let worker_control = control.clone();
                let mut worker_session = session.clone();
                let worker_service = service.clone();
                let done = tx.clone();
                active = Some((id.clone(), control));
                std::thread::spawn(move || {
                    let _waiting = super::progress::Progress::waiting(format!(
                        "执行 {}",
                        line.split_whitespace().next().unwrap_or("查询")
                    ));
                    let result = worker_session.execute(&worker_service, &line, &worker_control);
                    let _ = done.send(Event::Complete(id, Box::new(worker_session), result));
                });
            } else if ended {
                break;
            }
        }
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(Event::Line(line)) => {
                let (command, tail) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
                if command == "cancel" && (tail.is_empty() || tail.starts_with("q-")) {
                    if let Some((id, c)) = &active {
                        if tail.is_empty() || tail == id {
                            c.cancel();
                            output(json!({"request":id,"state":"cancellation_requested"}))?;
                        } else {
                            output(
                                json!({"error":{"code":"StaleRequest","message":"active request differs"}}),
                            )?;
                        }
                    } else {
                        output(
                            json!({"error":{"code":"NoActiveRequest","message":"no query to cancel"}}),
                        )?;
                    }
                } else {
                    queued.push_back(line);
                }
            }
            Ok(Event::Complete(id, next, result)) => {
                let mut v = match result {
                    Ok(v) => {
                        session = *next;
                        v
                    }
                    Err(e) => json!({"error":{"code":e.code(),"message":e.message()}}),
                };
                if !v.is_object() {
                    v = json!({"result":v});
                }
                v["request"] = json!(id);
                v["view"] = session.view();
                if let Some(p) = &session.project {
                    let head = service.store.head(p)?;
                    if head != session.revision {
                        v["notice"] = json!({"new_head":head,"pinned_revision":session.revision,"continue_with":"revision head","sampled_ms":cq_capture_protocol::now_ms()});
                    }
                }
                output(v)?;
                active = None;
                if terminal && queued.is_empty() && !ended {
                    prompt(&session)?;
                }
            }
            Ok(Event::End) => ended = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some((_, control)) = &active {
        control.cancel();
    }
    if let Some(old) = old_signal {
        unsafe { libc::signal(libc::SIGINT, old) };
    }
    service.shutdown();
    Ok(())
}
fn prompt(session: &Session) -> Result<()> {
    eprint!("cq[{}]> ", session.project.as_deref().unwrap_or("未选择"));
    io::stderr().flush()?;
    Ok(())
}
