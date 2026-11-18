use crate::{LocalStore, atomic::immutable_write};
use cq_core::{Error, Result, model::*, query::QueryContinuation};
use lbug::{Connection, Database, SystemConfig, Value as DbValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub root: NativeValue,
    pub head: Option<String>,
    pub created_ms: u64,
    pub languages: Vec<Language>,
}
fn valid_digest(id: &str) -> Result<()> {
    if id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::new("InvalidRevision", "expected SHA-256 revision"));
    }
    Ok(())
}
fn db_error(e: impl std::fmt::Display) -> Error {
    Error::new("GraphDatabase", e.to_string())
}
fn config(read_only: bool) -> SystemConfig {
    SystemConfig::default()
        .buffer_pool_size(512 * 1024 * 1024)
        .max_num_threads(4)
        .max_db_size(4 * 1024 * 1024 * 1024)
        .read_only(read_only)
        .throw_on_wal_replay_failure(true)
}
pub(crate) struct Lock(File);
impl Lock {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let started = std::time::Instant::now();
        while unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(e.into());
            }
            if started.elapsed() > std::time::Duration::from_secs(30) {
                return Err(Error::new(
                    "StoreBusy",
                    "legacy publication/migration lock deadline exceeded",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", cq_capture_protocol::new_id()?));
    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
impl LocalStore {
    pub fn project_path(&self, name: &str) -> PathBuf {
        self.root.join("projects").join(digest(name.as_bytes()))
    }
    pub fn project_info(&self, name: &str) -> Result<ProjectInfo> {
        let path = self.project_path(name).join("project.json");
        if !path.is_file() {
            return Err(Error::new("MissingProject", name));
        }
        let bytes = std::fs::read(path)?;
        let p: ProjectInfo = serde_json::from_slice(&bytes)?;
        if p.name != name {
            return Err(Error::new("CorruptProject", "project identity mismatch"));
        }
        Ok(p)
    }
    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let dir = self.root.join("projects");
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut projects = vec![];
        for e in std::fs::read_dir(dir)? {
            let p = e?.path().join("project.json");
            if p.is_file() {
                projects.push(serde_json::from_slice::<ProjectInfo>(&std::fs::read(p)?)?);
            }
        }
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(projects)
    }
    pub fn head(&self, project: &str) -> Result<Option<String>> {
        if self.paged_enabled(project) {
            let head = self
                .root
                .join("format-2/projects")
                .join(digest(project.as_bytes()))
                .join("HEAD");
            return Ok(Some(serde_json::from_reader(File::open(head)?)?));
        }
        let path = self.project_path(project).join("project.json");
        if !path.exists() {
            return Ok(None);
        }
        Ok(self.project_info(project)?.head)
    }
    pub fn revision_path(&self, revision: &str) -> Result<PathBuf> {
        valid_digest(revision)?;
        Ok(self.root.join("revisions").join(revision))
    }
    pub fn publish_semantics(
        &self,
        snapshot: &SemanticSnapshot,
        expected: Option<&str>,
    ) -> Result<()> {
        if self.paged_enabled(&snapshot.project) {
            self.publish_paged_semantics(snapshot, expected)?;
            return Ok(());
        }
        cq_core::semantic::validate::validate(snapshot, &self.content)?;
        valid_digest(&snapshot.revision)?;
        let project = self.project_path(&snapshot.project);
        std::fs::create_dir_all(&project)?;
        #[cfg(debug_assertions)]
        {
            let hooks = project.join("test-hooks");
            if hooks.join("legacy-before-lock").is_file() {
                std::fs::write(hooks.join("ready-legacy-before-lock"), b"ready")?;
                while !hooks.join("release-legacy-before-lock").is_file() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        let _lock = Lock::open(&project.join("publish.lock"))?;
        if self.paged_enabled(&snapshot.project) {
            self.publish_paged_semantics(snapshot, expected)?;
            return Ok(());
        }
        let current = self.head(&snapshot.project)?;
        if current.as_deref() != expected {
            return Err(Error::new(
                "PublishConflict",
                "head changed while analysis was running",
            ));
        }
        if current.is_some() && self.project_info(&snapshot.project)?.root != snapshot.root {
            return Err(Error::new(
                "ProjectRootMismatch",
                "a project namespace cannot change its repository root",
            ));
        }
        let prior = if let Some(revision) = current.as_deref() {
            self.load_checkpoint(&snapshot.project, revision)
                .ok()
                .flatten()
        } else {
            None
        };
        let revision = self.revision_path(&snapshot.revision)?;
        std::fs::create_dir_all(&revision)?;
        let snapshot_digest = {
            let bytes = serde_json::to_vec(snapshot)?;
            immutable_write(&revision.join("snapshot.json"), &bytes)?;
            digest(&bytes)
        };
        let projection = revision.join("graph.lbdb");
        if !revision.join("committed.json").exists() {
            let scratch = tempfile::tempdir_in(&revision)?;
            let temporary = scratch.path().join("projection.lbdb");
            create_projection(&temporary, snapshot)?;
            if projection.exists() {
                std::fs::remove_file(&projection)?;
            }
            std::fs::rename(temporary, &projection)?;
            File::open(&projection)?.sync_all()?;
        }
        self.stage_checkpoint(snapshot, prior.as_ref(), &revision)?;
        immutable_write(
            &revision.join("committed.json"),
            &serde_json::to_vec(
                &json!({"revision":snapshot.revision,"project":snapshot.project,"snapshot_digest":snapshot_digest,"previous":current,"graph_database":"Ladybug","library_version":lbug::VERSION,"library_source":lbug::get_library_source()}),
            )?,
        )?;
        let mut languages: Vec<_> = snapshot.units.iter().map(|u| u.language).collect();
        languages.sort_by_key(|l| format!("{l:?}"));
        languages.dedup();
        let info = ProjectInfo {
            name: snapshot.project.clone(),
            root: snapshot.root.clone(),
            head: Some(snapshot.revision.clone()),
            created_ms: snapshot.created_ms,
            languages,
        };
        replace(
            &project.join("project.json"),
            &serde_json::to_vec_pretty(&info)?,
        )?;
        Ok(())
    }
    pub fn load_semantics(
        &self,
        project: &str,
        revision: Option<&str>,
    ) -> Result<SemanticSnapshot> {
        if self.paged_enabled(project) {
            return self.export_paged(project, revision);
        }
        let revision = revision
            .map(str::to_owned)
            .or(self.head(project)?)
            .ok_or_else(|| Error::new("MissingRevision", "index this project first"))?;
        let path = self.revision_path(&revision)?;
        let commit: Value = serde_json::from_slice(&std::fs::read(path.join("committed.json"))?)?;
        if commit["revision"] != revision || commit["project"] != project {
            return Err(Error::new(
                "CorruptRevision",
                "missing matching publication record",
            ));
        }
        let bytes = std::fs::read(path.join("snapshot.json"))?;
        if let Some(expected) = commit["snapshot_digest"].as_str() {
            if expected != digest(&bytes) {
                return Err(Error::new(
                    "CorruptRevision",
                    "snapshot publication checksum",
                ));
            }
        } else {
            return Err(Error::new(
                "LegacyRevision",
                "publication predates snapshot checksums; reindex the project",
            ));
        }
        crate::record_snapshot_decode();
        let snapshot: SemanticSnapshot = serde_json::from_slice(&bytes)?;
        if snapshot.project != project || snapshot.revision != revision {
            return Err(Error::new(
                "CorruptRevision",
                "snapshot publication coordinates",
            ));
        }
        cq_core::semantic::validate::validate(&snapshot, &self.content)?;
        Ok(snapshot)
    }
    pub fn save_cursor(&self, state: &QueryContinuation) -> Result<String> {
        let bytes = serde_json::to_vec(state)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(Error::new(
                "CursorBudgetExceeded",
                "continuation exceeds 64 MiB",
            ));
        }
        let id = digest(&bytes);
        if let cq_core::query::ContinuationState::Store { project, .. } = &state.state {
            crate::runtime::Client::new(&self.root)?.call(
                json!({"op":"pin_cursor","project":project,"cursor":id,"revision":state.revision}),
            )?;
        }
        immutable_write(
            &self.root.join("cursors").join(format!("{id}.json")),
            &bytes,
        )?;
        Ok(id)
    }
    pub fn load_cursor(&self, id: &str) -> Result<QueryContinuation> {
        valid_digest(id)?;
        let bytes =
            std::fs::read(self.root.join("cursors").join(format!("{id}.json"))).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::new("CursorExpired", "continuation is unavailable")
                } else {
                    e.into()
                }
            })?;
        if digest(&bytes) != id {
            return Err(Error::new("DigestMismatch", "query continuation"));
        }
        let state: QueryContinuation = serde_json::from_slice(&bytes)?;
        if let cq_core::query::ContinuationState::Store { project, .. } = &state.state {
            crate::runtime::Client::new(&self.root)?.call(json!({"op":"validate_cursor","project":project,"cursor":id,"revision":state.revision}))?;
        }
        Ok(state)
    }
    pub fn save_annotation(&self, annotation: &Annotation) -> Result<()> {
        if matches!(
            annotation.kind,
            AnnotationKind::AuthorRationale | AnnotationKind::AuthorReflection
        ) && annotation.evidence.is_empty()
        {
            return Err(Error::new(
                "MissingEvidence",
                "attributions of author rationale need frozen source/document evidence; use Hypothesis for unsupported inference",
            ));
        }
        if annotation.kind == AnnotationKind::Responsibility && annotation.entities.is_empty() {
            return Err(Error::new(
                "MissingEntity",
                "a responsibility annotation must name its underlying symbols",
            ));
        }
        if let Some(parent) = &annotation.parent {
            let known = self
                .project_path(&annotation.project)
                .join("annotations")
                .join(format!("{parent}.json"));
            if !known.is_file() {
                return Err(Error::new(
                    "MissingAnnotationParent",
                    "revision must refer to an existing annotation in this project",
                ));
            }
        }
        if self.paged_enabled(&annotation.project) {
            use cq_core::ports::facts::FactReader;
            let reader = self.open_facts(&annotation.project, Some(&annotation.revision))?;
            for id in &annotation.entities {
                if reader.get("entity", id)?.is_none() {
                    return Err(Error::new("MissingEntity", id));
                }
            }
            for a in &annotation.evidence {
                reader.source_range(&a.file_revision, a.start, a.end)?;
            }
        } else {
            let snapshot = self.load_semantics(&annotation.project, Some(&annotation.revision))?;
            if annotation
                .entities
                .iter()
                .any(|id| !snapshot.entities.iter().any(|e| &e.id == id))
            {
                return Err(Error::new(
                    "MissingEntity",
                    "annotation references an absent entity",
                ));
            }
            for anchor in &annotation.evidence {
                let view =
                    cq_core::input::InputView::open(snapshot.sources.clone(), &self.content)?;
                view.read(anchor, 16 * 1024 * 1024)?;
            }
        }
        valid_digest(&annotation.id)?;
        immutable_write(
            &self
                .project_path(&annotation.project)
                .join("annotations")
                .join(format!("{}.json", annotation.id)),
            &serde_json::to_vec(annotation)?,
        )?;
        Ok(())
    }
    pub fn annotations(
        &self,
        project: &str,
        current: &SemanticSnapshot,
    ) -> Result<Vec<Annotation>> {
        let path = self.project_path(project).join("annotations");
        if !path.exists() {
            return Ok(vec![]);
        }
        let mut items = vec![];
        for file in std::fs::read_dir(path)? {
            let file = file?.path();
            if file.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let mut a: Annotation = serde_json::from_slice(&std::fs::read(file)?)?;
            if a.revision != current.revision {
                let old = self.load_semantics(project, Some(&a.revision))?;
                let old_deps: Vec<_> = a
                    .depends_on
                    .iter()
                    .filter_map(|id| old.sources.files.iter().find(|f| &f.id == id))
                    .collect();
                if old_deps.is_empty()
                    || old_deps.iter().any(|f| {
                        !current
                            .sources
                            .files
                            .iter()
                            .any(|n| n.logical_path == f.logical_path && n.blob == f.blob)
                    })
                {
                    a.status = ReviewStatus::Stale;
                }
            }
            items.push(a);
        }
        items.sort_by_key(|a| a.created_ms);
        Ok(items)
    }
    pub fn delete_project_index(&self, name: &str) -> Result<()> {
        if self.paged_enabled(name) {
            crate::runtime::Client::new(&self.root)?.call(json!({"op":"delete","project":name}))?;
        }
        let path = self.project_path(name);
        if !path.exists() {
            return Err(Error::new("MissingProject", name));
        }
        let _lock = Lock::open(&path.join("publish.lock"))?;
        let tombstone = self.root.join("deleted-projects").join(format!(
            "{}-{}",
            digest(name.as_bytes()),
            cq_capture_protocol::new_id()?
        ));
        std::fs::create_dir_all(tombstone.parent().unwrap())?;
        std::fs::rename(path, tombstone)?;
        Ok(())
    }
    pub fn cypher(
        &self,
        snapshot: &SemanticSnapshot,
        query: &str,
        limit: usize,
        timeout_ms: u64,
    ) -> Result<Value> {
        if limit == 0 || limit > 100000 || timeout_ms == 0 {
            return Err(Error::new(
                "InvalidBudget",
                "Cypher rows 1..100000 and positive timeout",
            ));
        }
        let first = query
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if !["MATCH", "RETURN", "WITH", "UNWIND", "EXPLAIN", "PROFILE"].contains(&first.as_str()) {
            return Err(Error::new(
                "ReadOnlyQuery",
                "query must start with MATCH/RETURN/WITH/UNWIND/EXPLAIN/PROFILE",
            ));
        }
        let isolation = if self.paged_enabled(&snapshot.project) {
            let export = tempfile::tempdir()?;
            create_projection(&export.path().join("graph.lbdb"), snapshot)?;
            Some(export)
        } else {
            None
        };
        let db = Database::new(
            isolation
                .as_ref()
                .map(|d| d.path().join("graph.lbdb"))
                .unwrap_or(self.revision_path(&snapshot.revision)?.join("graph.lbdb")),
            config(true),
        )
        .map_err(db_error)?;
        let connection = Connection::new(&db).map_err(db_error)?;
        connection.set_query_timeout(timeout_ms);
        let mut prepared = connection.prepare(query).map_err(db_error)?;
        if !prepared.is_read_only() {
            return Err(Error::new("ReadOnlyQuery", "write queries are not allowed"));
        }
        let mut result = connection
            .execute(&mut prepared, vec![])
            .map_err(db_error)?;
        let columns = result.get_column_names();
        let total = result.get_num_tuples() as usize;
        let mut rows = vec![];
        for row in result.by_ref().take(limit) {
            rows.push(Value::Array(row.iter().map(db_value).collect()));
        }
        Ok(
            json!({"revision":snapshot.revision,"columns":columns,"rows":rows,"total":total,"truncated":rows.len()<total,"database":"Ladybug","library_version":lbug::VERSION}),
        )
    }
}
fn db_value(value: &DbValue) -> Value {
    match value {
        DbValue::Null(_) => Value::Null,
        DbValue::Bool(v) => json!(v),
        DbValue::Int64(v) => json!(v),
        DbValue::Int32(v) => json!(v),
        DbValue::UInt64(v) => json!(v),
        DbValue::Double(v) => json!(v),
        DbValue::Float(v) => json!(v),
        DbValue::String(v) => json!(v),
        DbValue::Json(v) => v.clone(),
        DbValue::List(_, values) | DbValue::Array(_, values) => {
            json!(values.iter().map(db_value).collect::<Vec<_>>())
        }
        DbValue::Struct(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), db_value(v)))
                .collect(),
        ),
        _ => json!(value.to_string()),
    }
}
fn create_projection(path: &Path, s: &SemanticSnapshot) -> Result<()> {
    let db = Database::new(path, config(false)).map_err(db_error)?;
    let conn = Connection::new(&db).map_err(db_error)?;
    let mut kinds: BTreeSet<_> = s.entities.iter().map(|e| e.kind).collect();
    kinds.insert(EntityKind::Symbol);
    for kind in &kinds {
        conn.query(&format!("CREATE NODE TABLE `{}`(id STRING PRIMARY KEY, unit STRING, name STRING, qualified_name STRING, file STRING, start INT64, finish INT64, signature STRING, external BOOLEAN, text STRING, stable_handle STRING)",kind.label())).map_err(db_error)?;
    }
    let entities: HashMap<_, _> = s.entities.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut pairs: BTreeMap<RelationKind, BTreeSet<(EntityKind, EntityKind)>> = BTreeMap::new();
    for edge in &s.relations {
        pairs.entry(edge.kind).or_default().insert((
            entities[edge.from.as_str()].kind,
            entities[edge.to.as_str()].kind,
        ));
    }
    for kind in RelationKind::all() {
        let pairs = pairs.entry(kind).or_default();
        if pairs.is_empty() {
            pairs.insert((EntityKind::Symbol, EntityKind::Symbol));
        }
        let pairs = pairs
            .iter()
            .map(|(a, b)| format!("FROM `{}` TO `{}`", a.label(), b.label()))
            .collect::<Vec<_>>()
            .join(", ");
        conn.query(&format!(
            "CREATE REL TABLE `{}`({pairs}, id STRING, occurrence STRING, authority STRING)",
            kind.label()
        ))
        .map_err(db_error)?;
    }
    // COPY performs bounded bulk ingestion. Per-row MATCH/CREATE scales poorly
    // for compiler indexes and keeps excessive transaction state alive.
    let temporary = tempfile::tempdir_in(path.parent().unwrap())?;
    let files: HashMap<_, _> = s
        .sources
        .files
        .iter()
        .map(|f| (f.id.as_str(), f.logical_path.display()))
        .collect();
    let mut node_files: BTreeMap<EntityKind, csv::Writer<File>> = BTreeMap::new();
    for e in &s.entities {
        if let std::collections::btree_map::Entry::Vacant(entry) = node_files.entry(e.kind) {
            let file = temporary
                .path()
                .join(format!("node-{}.csv", e.kind.label()));
            let mut writer = csv::WriterBuilder::new()
                .quote_style(csv::QuoteStyle::Always)
                .from_path(file)
                .map_err(db_error)?;
            writer
                .write_record([
                    "id",
                    "unit",
                    "name",
                    "qualified_name",
                    "file",
                    "start",
                    "finish",
                    "signature",
                    "external",
                    "text",
                    "stable_handle",
                ])
                .map_err(db_error)?;
            entry.insert(writer);
        }
        let source = e.source.as_ref();
        node_files
            .get_mut(&e.kind)
            .unwrap()
            .write_record([
                e.id.clone(),
                e.unit.clone(),
                e.name.clone(),
                e.qualified_name.clone(),
                source
                    .and_then(|a| files.get(a.file_revision.as_str()))
                    .cloned()
                    .unwrap_or_default(),
                source.map(|a| a.start).unwrap_or(0).to_string(),
                source.map(|a| a.end).unwrap_or(0).to_string(),
                e.signature.clone(),
                e.external.to_string(),
                e.attributes.get("search_text").cloned().unwrap_or_default(),
                e.stable_handle.clone().unwrap_or_default(),
            ])
            .map_err(db_error)?;
    }
    for (kind, mut writer) in node_files {
        writer.flush()?;
        let file = temporary.path().join(format!("node-{}.csv", kind.label()));
        let literal = serde_json::to_string(
            file.to_str()
                .ok_or_else(|| Error::new("InvalidGraphPath", "COPY path must be UTF-8"))?,
        )?;
        conn.query(&format!(
            "COPY `{}` FROM {literal} (HEADER=true, PARALLEL=false)",
            kind.label()
        ))
        .map_err(db_error)?;
    }
    let mut edge_files: BTreeMap<(RelationKind, EntityKind, EntityKind), csv::Writer<File>> =
        BTreeMap::new();
    for edge in &s.relations {
        let from = entities[edge.from.as_str()].kind;
        let to = entities[edge.to.as_str()].kind;
        let key = (edge.kind, from, to);
        if let std::collections::btree_map::Entry::Vacant(entry) = edge_files.entry(key) {
            let file = temporary.path().join(format!(
                "edge-{}-{}-{}.csv",
                edge.kind.label(),
                from.label(),
                to.label()
            ));
            let mut writer = csv::WriterBuilder::new()
                .quote_style(csv::QuoteStyle::Always)
                .from_path(file)
                .map_err(db_error)?;
            writer
                .write_record(["from", "to", "id", "occurrence", "authority"])
                .map_err(db_error)?;
            entry.insert(writer);
        }
        edge_files
            .get_mut(&key)
            .unwrap()
            .write_record([
                edge.from.clone(),
                edge.to.clone(),
                edge.id.clone(),
                edge.occurrence.clone().unwrap_or_default(),
                format!("{:?}", s.provenance_records[&edge.provenance].authority),
            ])
            .map_err(db_error)?;
    }
    for ((kind, from, to), mut writer) in edge_files {
        writer.flush()?;
        let file = temporary.path().join(format!(
            "edge-{}-{}-{}.csv",
            kind.label(),
            from.label(),
            to.label()
        ));
        let literal = serde_json::to_string(
            file.to_str()
                .ok_or_else(|| Error::new("InvalidGraphPath", "COPY path must be UTF-8"))?,
        )?;
        conn.query(&format!(
            "COPY `{}` FROM {literal} (HEADER=true, PARALLEL=false, from='{}', to='{}')",
            kind.label(),
            from.label(),
            to.label()
        ))
        .map_err(db_error)?;
    }
    let mut count = conn.query("MATCH (n) RETURN count(n)").map_err(db_error)?;
    let nodes = count.next().and_then(|r| r.first().cloned());
    if !matches!(nodes,Some(DbValue::Int64(n)) if n as usize==s.entities.len()) {
        return Err(Error::new(
            "ProjectionCountMismatch",
            "bulk node count differs from validated facts",
        ));
    }
    let mut count = conn
        .query("MATCH ()-[r]->() RETURN count(r)")
        .map_err(db_error)?;
    let edges = count.next().and_then(|r| r.first().cloned());
    if !matches!(edges,Some(DbValue::Int64(n)) if n as usize==s.relations.len()) {
        return Err(Error::new(
            "ProjectionCountMismatch",
            "bulk relationship count differs from validated facts",
        ));
    }
    conn.query("CHECKPOINT").map_err(db_error)?;
    Ok(())
}
