//! Durable execution contracts. A checkpoint certifies an input generation,
//! immutable output artifacts and a safe cursor; UI progress is not a checkpoint.
use crate::{Error, Result, model::digest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub reused: bool,
    pub cursor: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Capture,
    Materialize,
    NativeExport,
    Archive,
    Normalize,
    Relations,
    Documents,
    Canonicalize,
    Statistics,
    Encode,
    Directories,
    AuditInventory,
    AuditPayloads,
    AuditDirectories,
    AuditScopes,
    GraphInventory,
    GraphPages,
    GraphFacts,
    Publish,
    Validate,
}
impl Stage {
    pub const ALL: [Self; 20] = [
        Self::Capture,
        Self::Materialize,
        Self::NativeExport,
        Self::Archive,
        Self::Normalize,
        Self::Relations,
        Self::Documents,
        Self::Canonicalize,
        Self::Statistics,
        Self::Encode,
        Self::Directories,
        Self::AuditInventory,
        Self::AuditPayloads,
        Self::AuditDirectories,
        Self::AuditScopes,
        Self::GraphInventory,
        Self::GraphPages,
        Self::GraphFacts,
        Self::Publish,
        Self::Validate,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Materialize => "materialize",
            Self::NativeExport => "native_export",
            Self::Archive => "archive",
            Self::Normalize => "normalize",
            Self::Relations => "relations",
            Self::Documents => "documents",
            Self::Canonicalize => "canonicalize",
            Self::Statistics => "statistics",
            Self::Encode => "encode",
            Self::Directories => "directories",
            Self::AuditInventory => "audit_inventory",
            Self::AuditPayloads => "audit_payloads",
            Self::AuditDirectories => "audit_directories",
            Self::AuditScopes => "audit_scopes",
            Self::GraphInventory => "graph_inventory",
            Self::GraphPages => "graph_pages",
            Self::GraphFacts => "graph_facts",
            Self::Publish => "publish",
            Self::Validate => "validate",
        }
    }
    pub fn parse(name: &str) -> Result<Self> {
        let name = match name {
            "build" => "capture",
            "hir_export" | "crate_export" => "native_export",
            "prepare" => "canonicalize",
            "encode.directories" | "sort_ranges" | "bulk_tree" | "bulk_merge" => "directories",
            "audit.inventory" => "audit_inventory",
            "audit.pages" => "audit_payloads",
            "audit.directories" => "audit_directories",
            "audit.sources" | "audit.scopes" => "audit_scopes",
            "native_inventory" => "graph_inventory",
            "native_pages" => "graph_pages",
            "native_facts" => "graph_facts",
            "activate" => "publish",
            other => other,
        };
        Self::ALL
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| Error::new("UnknownStage", name))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageState {
    Pending,
    Running,
    Interrupted,
    Completed,
    Failed,
    Stale,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Relative to the execution's artifact directory, never an arbitrary path.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub format: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub stage: Stage,
    pub attempt: u64,
    pub input: String,
    /// Compatibility ID for this stage's format/algorithm, not the whole CLI.
    pub implementation: String,
    pub batches: u64,
    pub completed: u64,
    pub total: Option<u64>,
    pub unit: String,
    pub artifacts: Vec<Artifact>,
    pub cursor: serde_json::Value,
    pub seal: String,
}
impl Checkpoint {
    pub fn seal(&mut self) -> Result<()> {
        self.seal.clear();
        self.seal = digest(&serde_json::to_vec(self)?);
        Ok(())
    }
    pub fn verify(&self) -> Result<()> {
        let mut value = self.clone();
        value.seal()?;
        if self.version != 1
            || self.seal != value.seal
            || self.total.is_some_and(|n| self.completed > n)
            || self.input.is_empty()
            || self.implementation.is_empty()
        {
            return Err(Error::new(
                "InvalidStageCheckpoint",
                "version, binding, counts or seal",
            ));
        }
        let mut paths = std::collections::BTreeSet::new();
        for a in &self.artifacts {
            if a.path.is_empty()
                || a.path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
                || a.path.starts_with('/')
                || !paths.insert(&a.path)
                || a.sha256.len() != 64
                || !a
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::new(
                    "InvalidStageCheckpoint",
                    "artifact path/hash or duplicate",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub number: u64,
    pub state: StageState,
    pub checkpoint: Option<Checkpoint>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub version: u32,
    pub id: String,
    pub project: String,
    pub source: String,
    pub configuration: String,
    pub plan: Vec<Stage>,
    pub stages: BTreeMap<Stage, Vec<Attempt>>,
}
impl Run {
    pub fn new(
        id: String,
        project: String,
        source: String,
        configuration: String,
        plan: Vec<Stage>,
    ) -> Result<Self> {
        if id.is_empty()
            || project.is_empty()
            || source.is_empty()
            || configuration.is_empty()
            || plan.is_empty()
            || plan.windows(2).any(|v| v[0] >= v[1])
        {
            return Err(Error::new(
                "InvalidStagePlan",
                "identity and ordered unique stages are required",
            ));
        }
        Ok(Self {
            version: 1,
            id,
            project,
            source,
            configuration,
            plan,
            stages: BTreeMap::new(),
        })
    }
    pub fn current(&self, stage: Stage) -> Option<&Attempt> {
        self.stages.get(&stage).and_then(|v| v.last())
    }
    pub fn begin(&mut self, stage: Stage, rerun: bool) -> Result<u64> {
        let position = self
            .plan
            .iter()
            .position(|s| *s == stage)
            .ok_or_else(|| Error::new("UnknownStage", stage.name()))?;
        for before in &self.plan[..position] {
            if self
                .current(*before)
                .is_none_or(|a| a.state != StageState::Completed)
            {
                return Err(Error::new("StageDependencyIncomplete", before.name()));
            }
        }
        if !rerun
            && self
                .current(stage)
                .is_some_and(|a| a.state == StageState::Completed)
        {
            return Err(Error::new("StageAlreadyCompleted", stage.name()));
        }
        if !rerun
            && self.current(stage).is_some_and(|a| {
                matches!(
                    a.state,
                    StageState::Interrupted | StageState::Failed | StageState::Running
                )
            })
        {
            return Err(Error::new("StageResumeRequired", stage.name()));
        }
        for after in &self.plan[position + 1..] {
            if let Some(a) = self.stages.get_mut(after).and_then(|v| v.last_mut()) {
                a.state = StageState::Stale;
            }
        }
        let history = self.stages.entry(stage).or_default();
        let number = history.last().map_or(1, |a| a.number + 1);
        history.push(Attempt {
            number,
            state: StageState::Running,
            checkpoint: None,
            error: None,
        });
        Ok(number)
    }
    pub fn checkpoint(&mut self, point: Checkpoint) -> Result<()> {
        point.verify()?;
        let a = self
            .stages
            .get_mut(&point.stage)
            .and_then(|v| v.last_mut())
            .ok_or_else(|| Error::new("StageNotRunning", point.stage.name()))?;
        if a.state != StageState::Running || a.number != point.attempt {
            return Err(Error::new("StaleStageAttempt", point.stage.name()));
        }
        if let Some(old) = &a.checkpoint
            && (point.input != old.input
                || point.implementation != old.implementation
                || point.batches < old.batches
                || point.completed < old.completed
                || point.total != old.total
                || point.unit != old.unit)
        {
            return Err(Error::new("StageCheckpointRegression", point.stage.name()));
        }
        a.checkpoint = Some(point);
        Ok(())
    }
    pub fn resume(
        &mut self,
        stage: Stage,
        input: &str,
        implementation: &str,
    ) -> Result<Checkpoint> {
        let position = self
            .plan
            .iter()
            .position(|s| *s == stage)
            .ok_or_else(|| Error::new("UnknownStage", stage.name()))?;
        for before in &self.plan[..position] {
            if self
                .current(*before)
                .is_none_or(|a| a.state != StageState::Completed)
            {
                return Err(Error::new("StageDependencyIncomplete", before.name()));
            }
        }
        let a = self
            .stages
            .get_mut(&stage)
            .and_then(|v| v.last_mut())
            .ok_or_else(|| Error::new("MissingStageCheckpoint", stage.name()))?;
        if !matches!(
            a.state,
            StageState::Interrupted | StageState::Failed | StageState::Running
        ) {
            return Err(Error::new("StageNotResumable", stage.name()));
        }
        let point = a
            .checkpoint
            .as_ref()
            .ok_or_else(|| Error::new("MissingStageCheckpoint", stage.name()))?;
        point.verify()?;
        if point.input != input || point.implementation != implementation {
            return Err(Error::new("StageCheckpointMismatch", stage.name()));
        }
        a.state = StageState::Running;
        a.error = None;
        Ok(point.clone())
    }
    pub fn finish(&mut self, stage: Stage) -> Result<()> {
        let a = self
            .stages
            .get_mut(&stage)
            .and_then(|v| v.last_mut())
            .ok_or_else(|| Error::new("StageNotRunning", stage.name()))?;
        let p = a
            .checkpoint
            .as_ref()
            .ok_or_else(|| Error::new("MissingStageCheckpoint", stage.name()))?;
        p.verify()?;
        if a.state != StageState::Running || p.total != Some(p.completed) {
            return Err(Error::new("StageIncomplete", stage.name()));
        }
        a.state = StageState::Completed;
        Ok(())
    }
    pub fn interrupt(&mut self, stage: Stage, error: Option<String>) -> Result<()> {
        let a = self
            .stages
            .get_mut(&stage)
            .and_then(|v| v.last_mut())
            .ok_or_else(|| Error::new("StageNotRunning", stage.name()))?;
        if a.state != StageState::Running {
            return Err(Error::new("StageNotRunning", stage.name()));
        }
        a.state = StageState::Interrupted;
        a.error = error;
        Ok(())
    }
}
