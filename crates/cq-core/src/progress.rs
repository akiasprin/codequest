//! Best-effort progress snapshots. Counts describe one phase and one unit;
//! unlike facts, they never participate in publication or its integrity seal.
use crate::model::digest;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Task states, not sampled CPU utilization. These counters are telemetry only.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Pipeline {
    pub workers: usize,
    pub active: usize,
    pub planning: usize,
    pub preparing: usize,
    pub waiting_admission: usize,
    pub waiting_memory: usize,
    pub waiting_priority: usize,
    pub queued: usize,
    pub ready: usize,
    pub merging: usize,
    pub inflight_bytes: u64,
    pub budget_bytes: u64,
    pub archive_rows: u64,
    pub archive_reads: u64,
    pub archive_read_bytes: u64,
}

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
        if state == "complete" && value["warning_count"].as_u64().unwrap_or(0) > 0 {
            value["state"] = json!("complete_with_warnings");
        }
        value["updated_ms"] = json!(cq_capture_protocol::now_ms());
        let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec(&value).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}
/// A warning updates the progress bar rather than printing an unstructured log.
pub fn warning(store: &Path, project: &str, warning: &Value) {
    let file = path(store, project);
    let mut value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"project":project}));
    value["warning_count"] = json!(value["warning_count"].as_u64().unwrap_or(0) + 1);
    value["last_warning"] = warning.clone();
    value["updated_ms"] = json!(cq_capture_protocol::now_ms());
    let tmp = file.with_extension(format!("{}.warning.tmp", std::process::id()));
    let _ = std::fs::create_dir_all(file.parent().unwrap());
    if std::fs::write(&tmp, serde_json::to_vec(&value).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, file);
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
    pub fn task(&mut self, phase: &str, label: &str, done: u64, total: Option<u64>, unit: &str) {
        self.value.as_object_mut().unwrap().remove("pipeline");
        if self.value["phase"] != phase {
            self.detail(phase, label, done, total, unit, 1);
        } else {
            self.value["total"] = json!(total);
            self.count(done);
        }
    }
    pub fn begin(store: &Path, project: &str) -> Self {
        let mut p = Self::stage(store, project, "preflight", "准备索引", None, "项");
        p.value["started_ms"] = json!(cq_capture_protocol::now_ms());
        p.value["owner_pid"] = json!(std::process::id());
        p.value.as_object_mut().unwrap().remove("warning_count");
        p.value.as_object_mut().unwrap().remove("last_warning");
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
        Self::stage_with_workers(store, project, phase, label, total, unit, 1)
    }
    pub fn stage_with_workers(
        store: &Path,
        project: &str,
        phase: &str,
        label: &str,
        total: Option<u64>,
        unit: &str,
        workers: usize,
    ) -> Self {
        let path = path(store, project);
        let old = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
        let now = cq_capture_protocol::now_ms();
        let mut p = Self {
            path,
            value: json!({"project":project,"phase":phase,"label":label,"completed":0,"total":total,"unit":unit,"workers":workers,"state":"running","started_ms":old["started_ms"].as_u64().unwrap_or(now),"owner_pid":old["owner_pid"].as_u64().unwrap_or(std::process::id()as u64),"phase_started_ms":now,"updated_ms":now}),
            last: Instant::now(),
        };
        for field in ["warning_count", "last_warning"] {
            if let Some(v) = old.get(field) {
                p.value[field] = v.clone();
            }
        }
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
    pub fn pipeline(&mut self, completed: u64, pipeline: &Pipeline) {
        self.value["workers"] = json!(pipeline.workers);
        self.value["pipeline"] = json!(pipeline);
        self.count(completed);
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
        // A coordinator may outlive a subordinate stage that raised a warning.
        if let Ok(bytes) = std::fs::read(&self.path)
            && let Ok(latest) = serde_json::from_slice::<Value>(&bytes)
            && latest["started_ms"] == self.value["started_ms"]
            && latest["warning_count"].as_u64().unwrap_or(0)
                > self.value["warning_count"].as_u64().unwrap_or(0)
        {
            self.value["warning_count"] = latest["warning_count"].clone();
            self.value["last_warning"] = latest["last_warning"].clone();
        }
        self.value["updated_ms"] = json!(cq_capture_protocol::now_ms());
        if self.value["state"] == "complete"
            && self.value["warning_count"].as_u64().unwrap_or(0) > 0
        {
            self.value["state"] = json!("complete_with_warnings");
        }
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
    let detail = value
        .get("pipeline")
        .filter(|p| p.is_object())
        .map(|p| {
            format!(
                " | 活跃 {}（规划 {} / 还原 {}）待领取 {} 待准入 {}（内存 {} / 优先级 {}）待合并 {}",
                p["active"],
                p["planning"],
                p["preparing"],
                p["queued"],
                p["waiting_admission"],
                p["waiting_memory"].as_u64().unwrap_or(0),
                p["waiting_priority"].as_u64().unwrap_or(0),
                p["ready"]
            )
        })
        .unwrap_or_default();
    let warnings = value["warning_count"].as_u64().unwrap_or(0);
    let warning = if warnings > 0 {
        format!(" | 告警 {warnings}：已拒绝超限内容，使用 gaps 查询原文位置")
    } else {
        String::new()
    };
    format!("{label} [{bar}] {count} | {elapsed_secs}s | {workers} workers{detail}{warning}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warnings_survive_phase_and_coordinator_completion_and_reset_next_run() {
        let temp = tempfile::tempdir().unwrap();
        let mut coordinator = Progress::begin(temp.path(), "warn");
        warning(
            temp.path(),
            "warn",
            &json!({"code":"OversizedValueRejected"}),
        );
        let mut next = Progress::stage(temp.path(), "warn", "encode", "编码", Some(1), "事实");
        next.count(1);
        coordinator.finish("complete");
        let read = || {
            serde_json::from_slice::<Value>(&std::fs::read(path(temp.path(), "warn")).unwrap())
                .unwrap()
        };
        let value = read();
        assert_eq!(value["warning_count"], 1);
        assert_eq!(value["state"], "complete_with_warnings");
        assert!(format(&value, 2, 0).contains("告警 1"));
        // A distinct run must not inherit the previous run's warning.
        std::thread::sleep(Duration::from_millis(2));
        Progress::begin(temp.path(), "warn");
        assert!(read()["warning_count"].is_null());
    }
    #[test]
    fn parallel_phase_persists_its_budget_on_the_initial_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let mut p =
            Progress::stage_with_workers(temp.path(), "p", "encode", "编码", Some(12), "事实", 128);
        let initial: Value =
            serde_json::from_slice(&std::fs::read(path(temp.path(), "p")).unwrap()).unwrap();
        assert_eq!(initial["workers"], 128);
        assert_eq!(initial["completed"], 0);
        assert_eq!(initial["total"], 12);
        p.count(12);
        let final_value: Value =
            serde_json::from_slice(&std::fs::read(path(temp.path(), "p")).unwrap()).unwrap();
        assert_eq!(final_value["workers"], 128);
        assert!(format(&final_value, 3, 0).contains("12/12"));
        assert!(format(&final_value, 3, 0).contains("128 workers"));
    }
}
