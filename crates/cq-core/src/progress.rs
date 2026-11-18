//! Best-effort progress snapshots. Counts describe one phase and one unit;
//! unlike facts, they never participate in publication or its integrity seal.
use crate::model::digest;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn path(store: &Path, project: &str) -> PathBuf {
    store
        .join("progress")
        .join(format!("{}.json", digest(project.as_bytes())))
}
pub fn finish(store: &Path, project: &str, state: &str) {
    let p = path(store, project);
    if let Ok(bytes) = std::fs::read(&p)
        && let Ok(mut value) = serde_json::from_slice::<Value>(&bytes)
    {
        value["state"] = json!(state);
        value["updated_ms"] = json!(cq_capture_protocol::now_ms());
        let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec(&value).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}
pub struct Progress {
    path: PathBuf,
    value: Value,
    last: Instant,
}
pub struct Reader<R> {
    pub inner: R,
    pub progress: Option<Progress>,
    pub completed: u64,
}
impl<R: std::io::Read> std::io::Read for Reader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(bytes)?;
        self.completed += n as u64;
        if let Some(p) = &mut self.progress {
            p.count(self.completed);
        }
        Ok(n)
    }
}
impl Progress {
    pub fn begin(store: &Path, project: &str) -> Self {
        let mut p = Self::stage(store, project, "preflight", "准备索引", None, "项");
        p.value["started_ms"] = json!(cq_capture_protocol::now_ms());
        p.value["owner_pid"] = json!(std::process::id());
        p.persist();
        p
    }
    pub fn stage(
        store: &Path,
        project: &str,
        phase: &str,
        label: &str,
        total: Option<u64>,
        unit: &str,
    ) -> Self {
        let path = path(store, project);
        let old = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
        let now = cq_capture_protocol::now_ms();
        let mut p = Self {
            path,
            value: json!({"project":project,"phase":phase,"label":label,"completed":0,"total":total,"unit":unit,"workers":1,"state":"running","started_ms":old["started_ms"].as_u64().unwrap_or(now),"owner_pid":old["owner_pid"].as_u64().unwrap_or(std::process::id()as u64),"phase_started_ms":now,"updated_ms":now}),
            last: Instant::now(),
        };
        p.persist();
        p
    }
    pub fn count(&mut self, completed: u64) {
        self.value["completed"] = json!(completed);
        if self.last.elapsed() >= Duration::from_millis(200)
            || self.value["total"].as_u64() == Some(completed)
        {
            self.persist();
        }
    }
    pub fn detail(
        &mut self,
        phase: &str,
        label: &str,
        completed: u64,
        total: Option<u64>,
        unit: &str,
        workers: u64,
    ) {
        if self.value["phase"] != phase {
            self.value["phase_started_ms"] = json!(cq_capture_protocol::now_ms());
        }
        self.value["phase"] = json!(phase);
        self.value["label"] = json!(label);
        self.value["completed"] = json!(completed);
        self.value["total"] = json!(total);
        self.value["unit"] = json!(unit);
        self.value["workers"] = json!(workers);
        self.persist();
    }
    pub fn finish(&mut self, state: &str) {
        self.value["state"] = json!(state);
        self.persist();
    }
    fn persist(&mut self) {
        self.last = Instant::now();
        self.value["updated_ms"] = json!(cq_capture_protocol::now_ms());
        let _ = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(self.path.parent().unwrap())?;
            let tmp = self
                .path
                .with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(&self.value)?)?;
            std::fs::rename(tmp, &self.path)
        })();
    }
}

pub fn format(value: &Value, elapsed_secs: u64, tick: usize) -> String {
    let done = value["completed"].as_u64().unwrap_or(0);
    let label = value["label"].as_str().unwrap_or("准备任务");
    let unit = value["unit"].as_str().unwrap_or("项");
    let workers = value["workers"].as_u64().unwrap_or(1);
    let (bar, count) = if let Some(total) = value["total"].as_u64() {
        let size = done
            .min(total)
            .saturating_mul(24)
            .checked_div(total)
            .unwrap_or(24) as usize;
        (
            format!("{}{}", "#".repeat(size), "-".repeat(24 - size)),
            format!("{done}/{total} {unit}，剩余 {}", total.saturating_sub(done)),
        )
    } else {
        let mut b = vec![' '; 24];
        b[tick % 24] = '>';
        (
            b.into_iter().collect(),
            format!("已完成 {done} {unit}，总量统计中"),
        )
    };
    format!("{label} [{bar}] {count} | {elapsed_secs}s | {workers} workers")
}
