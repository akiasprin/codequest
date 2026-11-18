use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureCapabilities {
    pub language: Language,
    pub exclusive_build: bool,
    pub preserve_existing_chain: bool,
    pub semantic_relations: bool,
    pub immutable_compiler_inputs: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Go,
    C,
    Cpp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationKind {
    Compile,
    Probe,
    Unclassified,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionObservation {
    KnownRustcStarted,
    UnidentifiedProcessStarted,
    NotObserved,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigurationCoverage {
    Partial,
    ExplicitAndCompilerCfg,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitRole {
    Library,
    Binary,
    Test,
    BuildScript,
    ProcMacro,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitBinding {
    ObservedInvocation,
    ReusedArtifact,
    UnattributedArtifact,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobAccounting {
    ReconciledWithinScope,
    Incomplete,
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureBudget {
    pub build_timeout_ms: u64,
    pub probe_timeout_ms: u64,
    pub frame_bytes: usize,
    pub queue_bytes: usize,
    pub journal_bytes: u64,
    pub content_bytes: u64,
    pub drain_timeout_ms: u64,
    pub cancel_grace_ms: u64,
    pub report_timeout_ms: u64,
    pub invocation_report_timeout_ms: u64,
    pub argument_file_bytes: usize,
    pub argument_total_bytes: usize,
}
impl Default for CaptureBudget {
    fn default() -> Self {
        Self {
            build_timeout_ms: 900_000,
            probe_timeout_ms: 10_000,
            frame_bytes: 8 * 1024 * 1024,
            queue_bytes: 32 * 1024 * 1024,
            journal_bytes: 256 * 1024 * 1024,
            content_bytes: 512 * 1024 * 1024,
            drain_timeout_ms: 5_000,
            cancel_grace_ms: 2_000,
            report_timeout_ms: 200,
            invocation_report_timeout_ms: 1_000,
            argument_file_bytes: 1024 * 1024,
            argument_total_bytes: 4 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentChange {
    pub key: String,
    pub previous: Observed<NativeValue>,
    pub effective: NativeValue,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturePlan {
    pub run_id: String,
    pub project: NativeValue,
    pub output: NativeValue,
    pub target: NativeValue,
    pub build_tool: NativeValue,
    pub recorder: NativeValue,
    pub recorder_digest: String,
    pub requested_args: Vec<String>,
    pub effective_args: Vec<String>,
    pub environment_changes: Vec<EnvironmentChange>,
    pub extra_env_keys: Vec<String>,
    pub budget: CaptureBudget,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildOutcome {
    Succeeded,
    Failed,
    Cancelled,
    NotStarted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalIntegrity {
    Sound,
    WithGaps,
    Invalid,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealReceipt {
    pub run_id: String,
    pub protocol_version: u32,
    pub files: Vec<JournalFile>,
    pub event_count: usize,
    pub sealed_ms: u64,
    pub integrity: JournalIntegrity,
    pub gaps: Vec<Gap>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalFile {
    pub name: String,
    pub blob: BlobId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "value")]
pub enum SealOutcome {
    Sealed(SealReceipt),
    NotSealed(String),
    Unknown(String),
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureHealth {
    pub accepted: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub active_connections: usize,
    pub issues: Vec<Gap>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolchainObservation {
    pub command: NativeValue,
    pub version: Observed<String>,
    pub host: Observed<String>,
    pub sysroot: Observed<NativeValue>,
    pub evidence: EvidenceRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeObservation {
    pub command: NativeValue,
    pub cwd: Observed<NativeValue>,
    pub args: Vec<String>,
    pub output: NativeValue,
    pub stderr: NativeValue,
    pub exit: ExitObservation,
    pub duration_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilerInvocation {
    pub id: String,
    pub started: Option<Envelope>,
    pub process: Option<Envelope>,
    pub finished: Option<Envelope>,
    pub argument_files: Vec<Envelope>,
    pub classification: InvocationKind,
    pub toolchain: Observed<String>,
    pub execution: ExecutionObservation,
    pub evidence: Vec<EvidenceRef>,
}
/// Language-specific observations stay typed; adding an analyst adds a
/// configuration variant without changing module, unit or evidence identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "language", content = "observations", rename_all = "snake_case")]
pub enum ConfigurationObservation {
    Rust(RustConfiguration),
    Native(NativeConfiguration),
}
impl ConfigurationObservation {
    pub fn language(&self) -> Language {
        match self {
            Self::Rust(_) => Language::Rust,
            Self::Native(c) => c.language,
        }
    }
    pub fn rust(&self) -> Option<&RustConfiguration> {
        match self {
            Self::Rust(config) => Some(config),
            Self::Native(_) => None,
        }
    }
    pub fn coverage(&self) -> ConfigurationCoverage {
        match self {
            Self::Rust(config) => config.coverage,
            Self::Native(_) => ConfigurationCoverage::Partial,
        }
    }
    pub fn evidence(&self) -> Option<&EvidenceRef> {
        match self {
            Self::Rust(config) => config.compiler_cfg_evidence.as_ref(),
            Self::Native(_) => None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeConfiguration {
    pub language: Language,
    pub arguments: Vec<NativeValue>,
    pub environment: std::collections::BTreeMap<String, String>,
    pub selected_files: Vec<NativeValue>,
    pub working_directory: NativeValue,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RustConfiguration {
    pub explicit_cfg: Vec<String>,
    pub expected_cfg: Vec<String>,
    pub compiler_cfg: Observed<Vec<String>>,
    pub compiler_cfg_evidence: Option<EvidenceRef>,
    pub test: bool,
    pub edition: Observed<String>,
    pub options: Vec<NativeValue>,
    pub coverage: ConfigurationCoverage,
    pub gaps: Vec<Gap>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildUnit {
    pub id: String,
    pub language: Language,
    pub name: Observed<String>,
    pub root: Observed<NativeValue>,
    pub output_kinds: Vec<String>,
    pub role: UnitRole,
    pub host: Observed<String>,
    pub target: Observed<String>,
    pub configuration: ConfigurationObservation,
    pub invocation: Observed<String>,
    pub artifacts: Vec<String>,
    pub binding: UnitBinding,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildArtifact {
    pub id: String,
    pub package_id: String,
    pub target_name: String,
    pub target_kinds: Vec<String>,
    pub src_path: NativeValue,
    pub profile: ArtifactProfile,
    pub features: Vec<String>,
    pub paths: Vec<NativeValue>,
    pub fresh: bool,
    pub output_aliases: Vec<OutputAlias>,
    pub producer: Observed<String>,
    pub evidence: EvidenceRef,
}
/// A post-build filesystem identity observation, not evidence of compiler reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputAlias {
    pub invocation_id: String,
    pub reported_path: NativeValue,
    pub compiler_output: NativeValue,
    pub device: u64,
    pub inode: u64,
    pub observed_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactProfile {
    pub opt_level: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub test: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyBinding {
    pub consumer: String,
    pub alias: String,
    pub path: NativeValue,
    pub producer: Observed<String>,
    pub artifact: Observed<String>,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildRun {
    pub id: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub outcome: BuildOutcome,
    pub tool_exit: ExitObservation,
    pub stop_reason: Option<String>,
    pub build_duration_ms: u64,
    pub seal_duration_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureReport {
    pub schema_version: u32,
    pub plan: CapturePlan,
    pub run: BuildRun,
    pub seal: SealOutcome,
    pub health: CaptureHealth,
    pub probes: Vec<ProbeObservation>,
    pub preflight_toolchain: ToolchainObservation,
    pub toolchains: Vec<ToolchainObservation>,
    pub invocations: Vec<CompilerInvocation>,
    pub artifacts: Vec<BuildArtifact>,
    pub units: Vec<BuildUnit>,
    pub dependencies: Vec<DependencyBinding>,
    pub snapshot: SourceSnapshot,
    pub build_messages: Vec<serde_json::Value>,
    pub job_accounting: JobAccounting,
    pub gaps: Vec<Gap>,
    pub duration_ms: u64,
}
impl CaptureReport {
    pub fn exit_code(&self) -> i32 {
        if self.run.outcome == BuildOutcome::Cancelled
            && self.run.stop_reason.as_deref() == Some("user_cancelled")
        {
            return 130;
        }
        if !matches!(&self.seal,SealOutcome::Sealed(r) if r.integrity==JournalIntegrity::Sound)
            || !self.health.issues.is_empty()
            || self.job_accounting != JobAccounting::ReconciledWithinScope
            || self.run.stop_reason.is_some()
        {
            return 2;
        }
        match self.run.outcome {
            BuildOutcome::Succeeded => 0,
            BuildOutcome::Failed => 1,
            _ => 2,
        }
    }
}
