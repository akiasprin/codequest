use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Authority {
    SemanticResolution,
    RuleDerived,
    AuthorStatement,
    Hypothesis,
    RuntimeObservation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub provider: String,
    pub version: String,
    pub authority: Authority,
    pub build_run: Option<String>,
    pub note: String,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProvenanceId(pub String);
pub enum ProvenanceInput {
    Record(Provenance),
    Id(ProvenanceId),
}
impl From<Provenance> for ProvenanceInput {
    fn from(p: Provenance) -> Self {
        Self::Record(p)
    }
}
impl From<ProvenanceId> for ProvenanceInput {
    fn from(p: ProvenanceId) -> Self {
        Self::Id(p)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Support {
    Supported,
    Partial,
    Unsupported,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityCoverage {
    pub language: Option<Language>,
    pub capability: String,
    pub state: Support,
    pub reason: String,
}
/// Independent native coordinates. No spelling/expansion or declaration/body
/// location is inferred from another when the producer did not supply it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceLocations {
    pub declaration: Option<SourceAnchor>,
    pub definition: Option<SourceAnchor>,
    pub spelling: Option<SourceAnchor>,
    pub expansion: Option<SourceAnchor>,
}
impl SourceLocations {
    pub fn is_empty(&self) -> bool {
        self.anchors().next().is_none()
    }
    pub fn anchors(&self) -> impl Iterator<Item = &SourceAnchor> {
        [
            &self.declaration,
            &self.definition,
            &self.spelling,
            &self.expansion,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_handle: Option<String>,
    #[serde(default, skip_serializing_if = "SourceLocations::is_empty")]
    pub locations: SourceLocations,
    pub id: String,
    pub unit: String,
    pub provider_id: String,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    pub source: Option<SourceAnchor>,
    pub extent: Option<SourceAnchor>,
    pub signature: String,
    /// Native declaring container. Nominal type membership and lexical nesting
    /// are separate graph relations and must not overwrite this identity.
    pub parent: Option<String>,
    pub external: bool,
    pub exported: bool,
    pub attributes: BTreeMap<String, String>,
    pub type_definitions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_navigation: Vec<TypeNavigation>,
    pub provenance: ProvenanceId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeNavigation {
    pub source: SourceAnchor,
    pub extent: Option<SourceAnchor>,
    pub candidates: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EntityKind {
    Project,
    CompilationUnit,
    Package,
    Module,
    File,
    Function,
    Method,
    Struct,
    Enum,
    Variant,
    Union,
    Trait,
    Interface,
    Class,
    Type,
    TypeAlias,
    Field,
    Parameter,
    Variable,
    Constant,
    Macro,
    Impl,
    Body,
    Document,
    DocSection,
    Resource,
    Task,
    Route,
    Artifact,
    Symbol,
}
impl EntityKind {
    pub fn label(self) -> String {
        format!("{self:?}")
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyKind {
    Function,
    Closure,
    AsyncFunction,
    AsyncBlock,
    ConstInitializer,
    StaticInitializer,
    Generated,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Evaluation {
    Immediate,
    Deferred,
    Constant,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Body {
    pub id: String,
    pub unit: String,
    pub owner: String,
    pub parent: Option<String>,
    pub kind: BodyKind,
    pub source: SourceAnchor,
    pub evaluation: Evaluation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RelationKind {
    Contains,
    HasMember,
    LexicallyContains,
    References,
    Calls,
    MayCall,
    UsesType,
    Implements,
    Extends,
    Overrides,
    Imports,
    Exports,
    Accesses,
    Instantiates,
    Dispatches,
    Decorates,
    Tests,
    PassesValue,
    CreatesBody,
    DependsOn,
    Documents,
    DocReferences,
    Mentions,
    SimilarTo,
    Deploys,
    Runs,
    RoutesTo,
    ObservedCall,
}
impl RelationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Contains => "CONTAINS",
            Self::HasMember => "HAS_MEMBER",
            Self::LexicallyContains => "LEXICALLY_CONTAINS",
            Self::References => "REFERS_TO",
            Self::Calls => "CALLS",
            Self::MayCall => "MAY_CALL",
            Self::UsesType => "USES_TYPE",
            Self::Implements => "IMPLEMENTS",
            Self::Extends => "EXTENDS",
            Self::Overrides => "OVERRIDES",
            Self::Imports => "IMPORTS",
            Self::Exports => "EXPORTS",
            Self::Accesses => "ACCESSES",
            Self::Instantiates => "INSTANTIATES",
            Self::Dispatches => "DISPATCHES",
            Self::Decorates => "DECORATES",
            Self::Tests => "TESTS",
            Self::PassesValue => "PASSES_VALUE",
            Self::CreatesBody => "CREATES_BODY",
            Self::DependsOn => "DEPENDS_ON",
            Self::Documents => "DOCUMENTS",
            Self::DocReferences => "DOC_REFERENCES",
            Self::Mentions => "MENTIONS",
            Self::SimilarTo => "SIMILAR_TO",
            Self::Deploys => "DEPLOYS",
            Self::Runs => "RUNS",
            Self::RoutesTo => "ROUTES_TO",
            Self::ObservedCall => "OBSERVED_CALL",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::all().into_iter().find(|k| {
            k.label() == s.to_ascii_uppercase() || format!("{k:?}").eq_ignore_ascii_case(s)
        })
    }
    pub fn all() -> Vec<Self> {
        vec![
            Self::Contains,
            Self::HasMember,
            Self::LexicallyContains,
            Self::References,
            Self::Calls,
            Self::MayCall,
            Self::UsesType,
            Self::Implements,
            Self::Extends,
            Self::Overrides,
            Self::Imports,
            Self::Exports,
            Self::Accesses,
            Self::Instantiates,
            Self::Dispatches,
            Self::Decorates,
            Self::Tests,
            Self::PassesValue,
            Self::CreatesBody,
            Self::DependsOn,
            Self::Documents,
            Self::DocReferences,
            Self::Mentions,
            Self::SimilarTo,
            Self::Deploys,
            Self::Runs,
            Self::RoutesTo,
            Self::ObservedCall,
        ]
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "value")]
pub enum Resolution {
    Resolved(String),
    Candidates(Vec<String>),
    Unknown(String),
    NotApplicable,
}
impl Resolution {
    pub fn resolved(&self) -> Option<&str> {
        if let Self::Resolved(s) = self {
            Some(s)
        } else {
            None
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dispatch {
    Direct,
    Trait,
    Virtual,
    Indirect,
    Macro,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Occurrence {
    #[serde(default, skip_serializing_if = "SourceLocations::is_empty")]
    pub locations: SourceLocations,
    pub id: String,
    pub unit: String,
    pub owner: Option<String>,
    pub source: SourceAnchor,
    pub kind: RelationKind,
    pub declaration: Resolution,
    pub executable: Resolution,
    pub dispatch: Dispatch,
    pub evaluation: Evaluation,
    pub arguments: Vec<SourceAnchor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argument_bindings: Vec<ArgumentBinding>,
    pub provenance: ProvenanceId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArgumentBinding {
    pub position: usize,
    pub source: SourceAnchor,
    pub parameter: Resolution,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relation {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: RelationKind,
    pub occurrence: Option<String>,
    pub source: Option<SourceAnchor>,
    pub provenance: ProvenanceId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentBlock {
    pub id: String,
    pub file_revision: String,
    pub kind: DocumentKind,
    pub title: String,
    pub text: String,
    pub source: SourceAnchor,
    pub owner: Option<String>,
    pub parent: Option<String>,
    pub links: Vec<DocumentLink>,
    pub provenance: ProvenanceId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentKind {
    Rustdoc,
    GoDoc,
    CodeComment,
    Markdown,
    ReStructuredText,
    PlainText,
    PdfPage,
    ArchitectureDecision,
    Configuration,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentLink {
    pub text: String,
    pub destination: String,
    pub target: Resolution,
    pub authority: Authority,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisContext {
    pub id: String,
    pub entry_unit: String,
    pub units: Vec<String>,
    pub status: Support,
    pub gaps: Vec<Gap>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeAnalysisStats {
    pub database_reused: bool,
    pub changed_files: u64,
    pub workspace_reloaded: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderRecord {
    #[serde(default)]
    pub analysis: Option<NativeAnalysisStats>,
    pub name: String,
    pub version: String,
    pub binary_digest: String,
    pub input_digest: String,
    pub protocol: String,
    #[serde(default)]
    pub reused_from: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaterializedInput {
    pub path: String,
    pub file: FileRevision,
    pub unit: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildObservation {
    pub id: String,
    pub language: Language,
    pub command: NativeValue,
    pub arguments: Vec<NativeValue>,
    pub cwd: NativeValue,
    pub outcome: BuildOutcome,
    pub exit: ExitObservation,
    pub events: Vec<Envelope>,
    pub captured_report: Option<String>,
    pub gaps: Vec<Gap>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SemanticSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production: Option<crate::semantic::production::Production>,
    pub provenance_records: BTreeMap<ProvenanceId, Provenance>,
    pub schema_version: u32,
    pub revision: String,
    pub project: String,
    pub root: NativeValue,
    pub created_ms: u64,
    pub previous: Option<String>,
    #[serde(default)]
    pub analysis_request: Option<serde_json::Value>,
    #[serde(default)]
    pub working_tree_manifest: std::collections::BTreeMap<String, String>,
    pub sources: SourceSnapshot,
    #[serde(default)]
    pub analysis_inputs: Vec<FileRevision>,
    #[serde(default)]
    pub analysis_directories: Vec<(NativeValue, String)>,
    pub builds: Vec<BuildObservation>,
    pub units: Vec<BuildUnit>,
    #[serde(default)]
    pub build_dependencies: Vec<DependencyBinding>,
    pub contexts: Vec<AnalysisContext>,
    pub providers: Vec<ProviderRecord>,
    pub entities: Vec<Entity>,
    pub bodies: Vec<Body>,
    pub occurrences: Vec<Occurrence>,
    pub relations: Vec<Relation>,
    pub documents: Vec<DocumentBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_traces: Vec<RuntimeTrace>,
    pub coverage: Vec<CapabilityCoverage>,
    pub gaps: Vec<Gap>,
}
impl SemanticSnapshot {
    /// Copy observation/configuration metadata without cloning semantic payloads.
    pub fn metadata(&self) -> Self {
        Self {
            production: self.production.clone(),
            provenance_records: self.provenance_records.clone(),
            schema_version: self.schema_version,
            revision: self.revision.clone(),
            project: self.project.clone(),
            root: self.root.clone(),
            created_ms: self.created_ms,
            previous: self.previous.clone(),
            analysis_request: self.analysis_request.clone(),
            working_tree_manifest: self.working_tree_manifest.clone(),
            sources: self.sources.clone(),
            analysis_inputs: self.analysis_inputs.clone(),
            analysis_directories: self.analysis_directories.clone(),
            builds: self.builds.clone(),
            units: self.units.clone(),
            build_dependencies: self.build_dependencies.clone(),
            contexts: self.contexts.clone(),
            providers: self.providers.clone(),
            entities: vec![],
            bodies: vec![],
            occurrences: vec![],
            relations: vec![],
            documents: vec![],
            runtime_traces: self.runtime_traces.clone(),
            coverage: self.coverage.clone(),
            gaps: self.gaps.clone(),
        }
    }
    pub fn empty(project: String, root: NativeValue) -> Self {
        Self {
            production: None,
            provenance_records: BTreeMap::new(),
            schema_version: 2,
            revision: String::new(),
            project,
            root,
            created_ms: cq_capture_protocol::now_ms(),
            previous: None,
            analysis_request: None,
            working_tree_manifest: Default::default(),
            sources: SourceSnapshot::seal(vec![], InputBinding::Unknown, vec![]),
            analysis_inputs: vec![],
            analysis_directories: vec![],
            builds: vec![],
            units: vec![],
            build_dependencies: vec![],
            contexts: vec![],
            providers: vec![],
            entities: vec![],
            bodies: vec![],
            occurrences: vec![],
            relations: vec![],
            documents: vec![],
            runtime_traces: vec![],
            coverage: vec![],
            gaps: vec![],
        }
    }
    pub fn intern_provenance(&mut self, provenance: Provenance) -> ProvenanceId {
        let id = ProvenanceId(digest(
            &serde_json::to_vec(&provenance).expect("provenance identity"),
        ));
        self.provenance_records
            .entry(id.clone())
            .or_insert(provenance);
        id
    }
    pub fn seal(&mut self) -> crate::Result<()> {
        self.entities.sort_by(|a, b| a.id.cmp(&b.id));
        self.entities.dedup_by(|a, b| {
            a.id == b.id && serde_json::to_vec(a).ok() == serde_json::to_vec(b).ok()
        });
        self.relations.sort_by(|a, b| a.id.cmp(&b.id));
        self.relations.dedup_by(|a, b| {
            a.id == b.id && serde_json::to_vec(a).ok() == serde_json::to_vec(b).ok()
        });
        self.occurrences.sort_by(|a, b| a.id.cmp(&b.id));
        self.occurrences.dedup_by(|a, b| {
            a.id == b.id && serde_json::to_vec(a).ok() == serde_json::to_vec(b).ok()
        });
        self.bodies.sort_by(|a, b| a.id.cmp(&b.id));
        self.bodies.dedup_by(|a, b| {
            a.id == b.id && serde_json::to_vec(a).ok() == serde_json::to_vec(b).ok()
        });
        self.documents.sort_by(|a, b| a.id.cmp(&b.id));
        self.revision.clear();
        use sha2::{Digest, Sha256};
        struct HashWriter(Sha256);
        impl std::io::Write for HashWriter {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.update(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut hash = HashWriter(Sha256::new());
        if self.production.as_ref().is_some_and(|p| p.streamed) {
            hash.0.update(b"codequest-scoped-revision-v1\0");
            serde_json::to_writer(&mut hash, &self.metadata())?;
        } else {
            serde_json::to_writer(&mut hash, &self)?;
        }
        self.revision = format!("{:x}", hash.0.finalize());
        Ok(())
    }
    pub fn add_relation(
        &mut self,
        from: &str,
        to: &str,
        kind: RelationKind,
        source: Option<SourceAnchor>,
        occurrence: Option<String>,
        provenance: impl Into<ProvenanceInput>,
    ) {
        let provenance = match provenance.into() {
            ProvenanceInput::Record(p) => self.intern_provenance(p),
            ProvenanceInput::Id(id) => id,
        };
        let id = digest(
            &serde_json::to_vec(&(from, to, kind, &source, &occurrence, &provenance))
                .expect("relation identity"),
        );
        self.relations.push(Relation {
            id,
            from: from.into(),
            to: to.into(),
            kind,
            source,
            occurrence,
            provenance,
        });
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub project: String,
    pub revision: String,
    pub parent: Option<String>,
    pub kind: AnnotationKind,
    pub text: String,
    pub author: String,
    pub entities: Vec<String>,
    pub evidence: Vec<SourceAnchor>,
    pub depends_on: Vec<String>,
    pub status: ReviewStatus,
    pub created_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationKind {
    Responsibility,
    Mechanism,
    Protocol,
    AuthorRationale,
    AuthorReflection,
    Hypothesis,
    Question,
    ArchitectureDecision,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    Proposed,
    Confirmed,
    Rejected,
    Stale,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeTrace {
    pub id: String,
    pub from: String,
    pub to: String,
    pub count: u64,
    pub source: String,
    pub raw: BlobId,
    pub observed_revision: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeCache {
    #[serde(default)]
    pub include_queries: bool,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub directories: Vec<(NativeValue, String)>,
    pub key: String,
    pub raw: BlobId,
    pub sources: Vec<FileRevision>,
}
