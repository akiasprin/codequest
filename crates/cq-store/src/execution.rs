//! Durable, exclusively owned indexing executions. UI counters never certify
//! work: a completed stage points at a sealed output and its record generation.
use crate::{LocalStore, raw};
use cq_core::{
    Error, Result,
    execution::{Artifact, Checkpoint, Entry, Run, Stage, StageState},
    model::{SemanticSnapshot, digest},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Mutex,
};
pub(crate) mod artifact;
mod retention;
const IMPLEMENTATION: &str = "codequest-stages/1";
#[derive(Clone, Serialize, Deserialize)]
struct Envelope {
    run: Run,
    request: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    seal: String,
}
impl Envelope {
    fn seal(&mut self) -> Result<()> {
        self.seal.clear();
        self.seal = digest(&serde_json::to_vec(self)?);
        Ok(())
    }
    fn verify(&self) -> Result<()> {
        let mut next = self.clone();
        next.seal()?;
        if next.seal != self.seal
            || self.run.version != 1
            || self.run.plan.is_empty()
            || self.run.configuration != digest(&serde_json::to_vec(&self.request)?)
        {
            return Err(Error::new("CorruptExecution", "run format/seal"));
        }
        for (stage, attempts) in &self.run.stages {
            if !self.run.plan.contains(stage) {
                return Err(Error::new("CorruptExecution", "foreign stage"));
            }
            for (i, a) in attempts.iter().enumerate() {
                if a.number != i as u64 + 1 {
                    return Err(Error::new("CorruptExecution", "attempt sequence"));
                }
                if let Some(p) = &a.checkpoint {
                    p.verify()?;
                    if p.stage != *stage || p.attempt != a.number {
                        return Err(Error::new("CorruptExecution", "checkpoint binding"));
                    }
                }
                if a.state == StageState::Completed
                    && a.checkpoint
                        .as_ref()
                        .is_none_or(|p| p.total != Some(p.completed))
                {
                    return Err(Error::new("CorruptExecution", "incomplete completion"));
                }
            }
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct Output {
    header: SemanticSnapshot,
    records: Option<raw::Checkpoint>,
    cursor: Value,
    files: Vec<raw::Image>,
}
fn image(path: &Path) -> Result<raw::Image> {
    use sha2::{Digest, Sha256};
    let path = path.canonicalize()?;
    let mut file = File::open(&path)?;
    let mut hash = Sha256::new();
    let bytes = std::io::copy(&mut file, &mut hash)?;
    Ok(raw::Image {
        path,
        bytes,
        sha256: format!("{:x}", hash.finalize()),
    })
}
fn files(value: &Value, out: &mut Vec<raw::Image>) -> Result<()> {
    match value {
        Value::String(s) if Path::new(s).is_absolute() && Path::new(s).is_file() => {
            out.push(image(Path::new(s))?)
        }
        Value::Array(a) => {
            for v in a {
                files(v, out)?;
            }
        }
        Value::Object(m) => {
            for v in m.values() {
                files(v, out)?;
            }
        }
        _ => (),
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
pub struct Candidate {
    pub header: SemanticSnapshot,
    pub revision: crate::facts::writer::Revision,
    pub transport: Value,
    encode: String,
    directories: String,
}
pub struct Execution {
    directory: PathBuf,
    value: Mutex<Envelope>,
    until: Option<Stage>,
    control: Mutex<cq_core::control::CaptureControl>,
    _owner: File,
}
fn load(path: &Path) -> Result<Envelope> {
    let e: Envelope = serde_json::from_reader(File::open(path.join("state.json"))?)?;
    e.verify()?;
    Ok(e)
}
fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    if !crate::atomic::valid_id(id) {
        return Err(Error::new("InvalidExecutionId", id));
    }
    Ok(root.join("executions").join(id))
}
impl Execution {
    pub fn create(
        root: &Path,
        project: &str,
        source: &Value,
        request: Value,
        plan: Vec<Stage>,
        until: Option<Stage>,
    ) -> Result<Self> {
        let id = cq_capture_protocol::new_id()?;
        let path = directory(root, &id)?;
        crate::atomic::create_directory(&path)?;
        let e = Envelope {
            run: Run::new(
                id,
                project.into(),
                digest(&serde_json::to_vec(source)?),
                digest(&serde_json::to_vec(&request)?),
                plan,
            )?,
            request,
            last_error: None,
            seal: String::new(),
        };
        let this = Self {
            _owner: Self::lock(&path)?,
            directory: path,
            value: Mutex::new(e),
            until,
            control: Mutex::new(Default::default()),
        };
        this.save(&mut this.value.lock().unwrap())?;
        crate::revisions::save(&this.directory.join("source.json"), source)?;
        Ok(this)
    }
    fn lock(path: &Path) -> Result<File> {
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join("owner.lock"))?;
        if unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::new(
                "ExecutionBusy",
                "another process owns this execution",
            ));
        }
        Ok(owner)
    }
    pub fn open(
        root: &Path,
        id: &str,
        source: &Value,
        rerun: Option<Stage>,
        until: Option<Stage>,
    ) -> Result<Self> {
        let path = directory(root, id)?;
        let owner = Self::lock(&path)?;
        let mut value = load(&path)?;
        if value.run.id != id || value.run.source != digest(&serde_json::to_vec(source)?) {
            return Err(Error::new(
                "ExecutionInputChanged",
                "source tree changed; start a new execution",
            ));
        }
        // Exclusive ownership proves a prior frontend no longer writes stages.
        for history in value.run.stages.values_mut() {
            if let Some(a) = history.last_mut()
                && a.state == StageState::Running
            {
                a.state = StageState::Interrupted;
            }
        }
        if let Some(stage) = rerun {
            let position = value
                .run
                .plan
                .iter()
                .position(|s| *s == stage)
                .ok_or_else(|| Error::new("UnknownStage", stage.name()))?;
            for before in &value.run.plan[..position] {
                if value
                    .run
                    .current(*before)
                    .is_none_or(|a| a.state != StageState::Completed)
                {
                    return Err(Error::new("StageDependencyIncomplete", before.name()));
                }
            }
            for after in &value.run.plan[position..].to_vec() {
                if let Some(a) = value.run.stages.get_mut(after).and_then(|v| v.last_mut()) {
                    a.state = StageState::Stale;
                }
            }
        }
        let this = Self {
            directory: path,
            value: Mutex::new(value),
            until,
            control: Mutex::new(Default::default()),
            _owner: owner,
        };
        this.save(&mut this.value.lock().unwrap())?;
        Ok(this)
    }
    fn save(&self, value: &mut Envelope) -> Result<()> {
        value.seal()?;
        crate::revisions::save(&self.directory.join("state.json"), value)
    }
    pub fn candidate_template(&self) -> Result<Option<Candidate>> {
        let path = self.directory.join("candidate.json");
        if !path.exists() {
            return Ok(None);
        }
        let value = self.value.lock().unwrap();
        let Some(point) = value
            .run
            .current(Stage::Encode)
            .filter(|a| a.state == StageState::Completed)
            .and_then(|a| a.checkpoint.as_ref())
        else {
            return Ok(None);
        };
        let image: raw::Image = serde_json::from_reader(File::open(path)?)?;
        let candidate: Candidate =
            artifact::restore(&self.directory.join("artifacts/candidate"), &image)?;
        candidate.revision.verify()?;
        if candidate.encode != point.seal {
            return Ok(None);
        }
        Ok(Some(candidate))
    }
    pub fn candidate(&self) -> Result<Option<Candidate>> {
        let path = self.directory.join("candidate.json");
        if !path.exists() {
            return Ok(None);
        }
        let value = self.value.lock().unwrap();
        let done = |stage| {
            value
                .run
                .current(stage)
                .filter(|a| a.state == StageState::Completed)
                .and_then(|a| a.checkpoint.as_ref())
                .map(|p| p.seal.clone())
        };
        let (Some(encode), Some(directories)) = (done(Stage::Encode), done(Stage::Directories))
        else {
            return Ok(None);
        };
        let image: raw::Image = serde_json::from_reader(File::open(path)?)?;
        let candidate: Candidate =
            artifact::restore(&self.directory.join("artifacts/candidate"), &image)?;
        candidate.revision.verify()?;
        if candidate.encode != encode || candidate.directories != directories {
            return Ok(None);
        }
        Ok(Some(candidate))
    }
    pub fn save_candidate(
        &self,
        header: &SemanticSnapshot,
        revision: &crate::facts::writer::Revision,
        transport: Value,
    ) -> Result<()> {
        let value = self.value.lock().unwrap();
        let seal = |stage| {
            value
                .run
                .current(stage)
                .and_then(|a| a.checkpoint.as_ref())
                .map(|p| p.seal.clone())
                .ok_or_else(|| Error::new("StageIncomplete", "candidate prerequisites"))
        };
        let candidate = Candidate {
            header: header.clone(),
            revision: revision.clone(),
            transport,
            encode: seal(Stage::Encode)?,
            directories: seal(Stage::Directories)?,
        };
        let image = artifact::retain(&self.directory.join("artifacts/candidate"), &candidate)?;
        crate::revisions::save(&self.directory.join("candidate.json"), &image)
    }
    pub fn stable_metadata_metrics(&self, header: &mut SemanticSnapshot) -> Result<()> {
        let value = self.value.lock().unwrap();
        let current = value
            .run
            .current(Stage::Canonicalize)
            .map(|a| a.number)
            .unwrap_or(0);
        let actual = header
            .production
            .as_ref()
            .map(|p| p.stage_ms.clone())
            .unwrap_or_default();
        let path = self.directory.join("measurements").join(format!(
            "canonicalize-{current}-{}.json",
            cq_capture_protocol::new_id()?
        ));
        crate::atomic::immutable_write(&path, &serde_json::to_vec(&actual)?)?;
        // Timing belongs to the attempt. Preserve the first metadata record
        // for this same frozen input so timing alone cannot invent a revision.
        if let Some(attempts) = value.run.stages.get(&Stage::Canonicalize) {
            for attempt in attempts {
                if let Some(point) = &attempt.checkpoint {
                    let output = self.output(point)?;
                    if output.header.working_tree_manifest == header.working_tree_manifest
                        && output
                            .header
                            .builds
                            .iter()
                            .map(|b| &b.id)
                            .eq(header.builds.iter().map(|b| &b.id))
                    {
                        if let (Some(old), Some(new)) =
                            (&output.header.production, &mut header.production)
                        {
                            new.stage_ms = old.stage_ms.clone();
                        }
                        break;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn record_error(&self, error: &Error) -> Result<()> {
        let mut value = self.value.lock().unwrap();
        value.last_error = Some(error.to_string());
        for attempts in value.run.stages.values_mut() {
            if let Some(a) = attempts.last_mut()
                && a.state == StageState::Running
            {
                a.error = Some(error.to_string());
                a.state = if matches!(error.code(), "Cancelled" | "Io") {
                    StageState::Interrupted
                } else {
                    StageState::Failed
                };
            }
        }
        self.save(&mut value)
    }
    pub fn attach_control(&self, control: &cq_core::control::CaptureControl) {
        *self.control.lock().unwrap() = control.clone();
    }
    pub fn control(&self) -> cq_core::control::CaptureControl {
        self.control.lock().unwrap().clone()
    }
    fn hook(&self, stage: Stage, point: &str) -> Result<()> {
        #[cfg(debug_assertions)]
        {
            let root = self.directory.parent().and_then(Path::parent).unwrap();
            let hooks = root.join("format-2/test-hooks");
            let name = format!("{}-{point}", stage.name());
            if hooks.join(&name).exists() {
                std::fs::write(hooks.join(format!("ready-{name}")), b"ready")?;
                while !hooks.join(format!("release-{name}")).exists() {
                    if self.control().is_cancelled() {
                        return Err(Error::new("Cancelled", "execution interrupted"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        let _ = (stage, point);
        Ok(())
    }
    pub fn completed(&self, stage: Stage) -> bool {
        self.value
            .lock()
            .unwrap()
            .run
            .current(stage)
            .is_some_and(|a| a.state == StageState::Completed)
    }
    pub fn id(&self) -> String {
        self.value.lock().unwrap().run.id.clone()
    }
    pub fn reuse(
        &self,
        stage: Stage,
        header: &mut SemanticSnapshot,
        records: Option<&raw::Journal>,
    ) -> Result<bool> {
        if self
            .value
            .lock()
            .unwrap()
            .run
            .current(stage)
            .is_none_or(|a| a.state != StageState::Completed)
        {
            return Ok(false);
        }
        Ok(self.start(stage, header, records)?.reused)
    }
    pub fn task(&self, stage: Stage) -> Result<String> {
        let v = self.value.lock().unwrap();
        let a = v
            .run
            .current(stage)
            .ok_or_else(|| Error::new("StageNotRunning", stage.name()))?;
        Ok(format!("{}:{}:{}", v.run.id, stage.name(), a.number))
    }
    pub fn cursor(&self, stage: Stage) -> Value {
        self.value
            .lock()
            .unwrap()
            .run
            .current(stage)
            .and_then(|a| a.checkpoint.as_ref())
            .map(|p| p.cursor.clone())
            .unwrap_or(Value::Null)
    }
    pub fn request(root: &Path, id: &str) -> Result<Value> {
        Ok(load(&directory(root, id)?)?.request)
    }
    pub fn source(root: &Path, id: &str) -> Result<Value> {
        Ok(serde_json::from_reader(File::open(
            directory(root, id)?.join("source.json"),
        )?)?)
    }
    pub fn status(root: &Path, id: &str) -> Result<Value> {
        let path = directory(root, id)?;
        let value = load(&path)?;
        let active = match Self::lock(&path) {
            Ok(_) => false,
            Err(e) if e.code() == "ExecutionBusy" => true,
            Err(e) => return Err(e),
        };
        let progress = std::fs::read(cq_core::progress::path(root, &value.run.project))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        Ok(
            json!({"owner_active":active,"progress":progress,"last_error":value.last_error,"execution":value.run.id,"project":value.run.project,"stages":value.run.plan.iter().map(|s|json!({"stage":s.name(),"attempt":value.run.current(*s),"history":value.run.stages.get(s)})).collect::<Vec<_>>() }),
        )
    }
    pub fn list(root: &Path) -> Result<Value> {
        let path = root.join("executions");
        let mut out = vec![];
        if path.exists() {
            for p in std::fs::read_dir(path)? {
                let p = p?.path();
                if p.join("state.json").exists() {
                    let e = load(&p)?;
                    out.push(json!({"execution":e.run.id,"project":e.run.project,"next_stage":e.run.plan.iter().find(|s|e.run.current(**s).is_none_or(|a|a.state != StageState::Completed)).map(|s|s.name())}));
                }
            }
        }
        out.sort_by_key(|v| v["execution"].as_str().unwrap_or("").to_owned());
        Ok(json!(out))
    }
    fn input(value: &Envelope, stage: Stage) -> Result<String> {
        let position = value
            .run
            .plan
            .iter()
            .position(|s| *s == stage)
            .ok_or_else(|| Error::new("UnknownStage", stage.name()))?;
        let prior = if position == 0 {
            None
        } else {
            value
                .run
                .current(value.run.plan[position - 1])
                .and_then(|a| a.checkpoint.as_ref())
                .map(|p| p.seal.as_str())
        };
        Ok(digest(&serde_json::to_vec(&(
            &value.run.source,
            &value.run.configuration,
            stage,
            prior,
        ))?))
    }
    pub fn start(
        &self,
        stage: Stage,
        header: &mut SemanticSnapshot,
        records: Option<&raw::Journal>,
    ) -> Result<Entry> {
        if self.control().is_cancelled() {
            return Err(Error::new("Cancelled", "execution interrupted"));
        }
        let mut value = self.value.lock().unwrap();
        let input = Self::input(&value, stage)?;
        let attempt = value.run.current(stage).cloned();
        if let Some(a) = attempt
            && matches!(
                a.state,
                StageState::Completed | StageState::Interrupted | StageState::Failed
            )
            && a.checkpoint.is_some()
        {
            let p = a
                .checkpoint
                .ok_or_else(|| Error::new("MissingStageCheckpoint", stage.name()))?;
            if p.input != input || p.implementation != IMPLEMENTATION {
                return Err(Error::new("StageCheckpointMismatch", stage.name()));
            }
            let output = self.output(&p)?;
            if let (Some(j), Some(point)) = (records, &output.records) {
                j.restore_progress(
                    point,
                    &self.directory.join("artifacts/records"),
                    Some(&value.run.project),
                )?;
            }
            *header = output.header;
            let reused = a.state == StageState::Completed;
            if !reused {
                value.run.resume(stage, &input, IMPLEMENTATION)?;
                self.save(&mut value)?;
            }
            return Ok(Entry {
                reused,
                cursor: output.cursor,
            });
        }
        if value
            .run
            .current(stage)
            .is_some_and(|a| a.state == StageState::Interrupted && a.checkpoint.is_none())
        {
            value
                .run
                .stages
                .get_mut(&stage)
                .unwrap()
                .last_mut()
                .unwrap()
                .state = StageState::Running;
        } else {
            value.run.begin(stage, true)?;
        }
        self.save(&mut value)?;
        Ok(Entry::default())
    }
    fn output(&self, point: &Checkpoint) -> Result<Output> {
        point.verify()?;
        if point.artifacts.len() != 1 {
            return Err(Error::new("CorruptExecution", "stage output count"));
        }
        let a = &point.artifacts[0];
        let path = self.directory.join(&a.path).canonicalize()?;
        if !path.starts_with(self.directory.canonicalize()?) {
            return Err(Error::new("CorruptExecution", "artifact path"));
        }
        let b = std::fs::read(path)?;
        if b.len() as u64 != a.bytes || digest(&b) != a.sha256 {
            return Err(Error::new("DigestMismatch", "stage output"));
        }
        let output: Output = serde_json::from_slice(&b)?;
        for expected in &output.files {
            let actual = image(&expected.path)?;
            if actual.bytes != expected.bytes || actual.sha256 != expected.sha256 {
                return Err(Error::new("DigestMismatch", "stage external artifact"));
            }
        }
        Ok(output)
    }
    pub fn finish(
        &self,
        stage: Stage,
        header: &SemanticSnapshot,
        records: Option<&raw::Journal>,
        cursor: Value,
    ) -> Result<()> {
        let prior = self
            .value
            .lock()
            .unwrap()
            .run
            .current(stage)
            .and_then(|a| a.checkpoint.as_ref())
            .and_then(|p| p.total);
        let total = prior.unwrap_or(1);
        self.checkpoint(stage, header, records, cursor, total, total, true)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn checkpoint(
        &self,
        stage: Stage,
        header: &SemanticSnapshot,
        records: Option<&raw::Journal>,
        cursor: Value,
        completed: u64,
        total: u64,
        complete: bool,
    ) -> Result<()> {
        if !complete && self.control().is_cancelled() {
            return Err(Error::new("Cancelled", "execution interrupted"));
        }
        let mut value = self.value.lock().unwrap();
        let previous = value
            .run
            .current(stage)
            .and_then(|a| a.checkpoint.as_ref())
            .cloned();
        let attempt = value
            .run
            .current(stage)
            .ok_or_else(|| Error::new("StageNotRunning", stage.name()))?
            .number;
        let record_point = records
            .map(|j| j.checkpoint(&self.directory.join("artifacts/records")))
            .transpose()?;
        let mut header = header.clone();
        crate::facts::limits::reject_oversized_build_events(
            self.directory.parent().and_then(Path::parent).unwrap(),
            &mut header,
        )?;
        let mut artifacts = vec![];
        files(&cursor, &mut artifacts)?;
        let bytes = serde_json::to_vec(&Output {
            header,
            records: record_point,
            cursor: cursor.clone(),
            files: artifacts,
        })?;
        let sha = digest(&bytes);
        let relative = format!("artifacts/{}/{attempt}/{sha}.json", stage.name());
        crate::atomic::immutable_write(&self.directory.join(&relative), &bytes)?;
        let mut point = Checkpoint {
            version: 1,
            stage,
            attempt,
            input: Self::input(&value, stage)?,
            implementation: IMPLEMENTATION.into(),
            batches: value
                .run
                .current(stage)
                .and_then(|a| a.checkpoint.as_ref())
                .map_or(1, |p| p.batches + 1),
            completed,
            total: Some(total),
            unit: value
                .run
                .current(stage)
                .and_then(|a| a.checkpoint.as_ref())
                .map(|p| p.unit.clone())
                .unwrap_or_else(|| {
                    if stage == Stage::Normalize {
                        if cursor.get("tu").is_some() {
                            "translation_units"
                        } else {
                            "frames"
                        }
                    } else {
                        "stage"
                    }
                    .into()
                }),
            artifacts: vec![Artifact {
                path: relative.clone(),
                bytes: bytes.len() as u64,
                sha256: sha,
                format: "semantic-stage/1".into(),
            }],
            cursor,
            seal: String::new(),
        };
        point.seal()?;
        value.run.checkpoint(point)?;
        if complete {
            value.run.finish(stage)?;
        }
        self.save(&mut value)?;
        let id = value.run.id.clone();
        drop(value);
        if let Some(old) = previous {
            for a in old.artifacts {
                if a.path != relative {
                    let _ = std::fs::remove_file(self.directory.join(a.path));
                }
            }
        }
        self.hook(stage, if complete { "complete" } else { "checkpoint" })?;
        if self.control().is_cancelled() {
            return Err(Error::new(
                "Cancelled",
                "completed output was saved; resume the execution",
            ));
        }
        if complete && self.until == Some(stage) {
            return Err(Error::new(
                "StagePaused",
                format!(
                    "execution {} stopped after {}; resume with cq resume --run {}",
                    id,
                    stage.name(),
                    id
                ),
            ));
        }
        Ok(())
    }
    pub fn stage_file(&self, stage: Stage, name: &str) -> Result<PathBuf> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(Error::new("InvalidStagePath", name));
        }
        let value = self.value.lock().unwrap();
        let number = value
            .run
            .current(stage)
            .ok_or_else(|| Error::new("StageNotRunning", stage.name()))?
            .number;
        let path = self
            .directory
            .join("work")
            .join(stage.name())
            .join(number.to_string());
        crate::atomic::create_directory(&path)?;
        Ok(path.join(name))
    }
}
impl LocalStore {
    pub fn execution_scope(&self, execution: std::sync::Arc<Execution>) -> Result<Self> {
        let mut scope = self.streaming_scope()?;
        scope.execution = Some(execution);
        Ok(scope)
    }
}
