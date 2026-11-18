use super::{
    index::{IndexRequest, index_repository},
    query,
};
use cq_core::{
    Error, Result, control::CaptureControl, model::*, ports::ContentRepository, query::QueryEngine,
};
use cq_store::LocalStore;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

pub struct Service {
    pub store: LocalStore,
    jobs: Mutex<HashMap<String, Job>>,
    model: Option<Arc<cq_core::query::embeddings::TokenVectors>>,
}
struct ReadExecution<'a> {
    control: &'a CaptureControl,
    guard: Option<&'a dyn cq_core::ports::facts::FactReader>,
}
struct Job {
    control: CaptureControl,
    state: Value,
}
impl Service {
    pub fn open(root: &Path) -> Result<Self> {
        let model = cq_core::query::embeddings::TokenVectors::configured()?;
        Ok(Self {
            store: LocalStore::open(root)?,
            jobs: Mutex::new(HashMap::new()),
            model,
        })
    }
    pub fn snapshot(&self, project: &str, revision: Option<&str>) -> Result<Arc<SemanticSnapshot>> {
        let rev = revision
            .map(str::to_owned)
            .or(self.store.head(project)?)
            .ok_or_else(|| Error::new("MissingRevision", "index this project first"))?;
        Ok(Arc::new(self.store.load_semantics(project, Some(&rev))?))
    }
    pub fn index(&self, request: IndexRequest) -> Result<Value> {
        match index_repository(&request, &self.store, &CaptureControl::default()) {
            Err(Error::PublicationInProgress { project, revision }) => Ok(
                json!({"state":"publication_in_progress","project":project,"revision":revision,"continue_with":{"operation":"publication_status","revision":revision}}),
            ),
            Err(error) => {
                cq_core::progress::finish(&self.store.root, &request.name, "failed");
                Err(error)
            }
            other => other,
        }
    }
    fn load_request(&self, project: &str) -> Result<IndexRequest> {
        let request: IndexRequest = serde_json::from_value(self.store.index_request(project)?)?;
        let info = self.store.project_info(project)?;
        if request.name != project || request.root != info.root.path() {
            return Err(Error::new(
                "InvalidCheckpoint",
                "saved request belongs to a different project/root",
            ));
        }
        Ok(request)
    }
    pub fn refresh(&self, project: &str) -> Result<Value> {
        match self.refresh_inner(project) {
            Err(Error::PublicationInProgress { project, revision }) => Ok(
                json!({"state":"publication_in_progress","project":project,"revision":revision,"continue_with":{"operation":"publication_status","revision":revision}}),
            ),
            other => other,
        }
    }
    fn refresh_inner(&self, project: &str) -> Result<Value> {
        let request = self.load_request(project)?;
        if self.store.paged_enabled(project) {
            if let Some(value) = super::checkpoint::restore_paged(self, &request)? {
                return Ok(value);
            }
            return self.index(request);
        }
        if let Ok(Some(snapshot)) = super::checkpoint::restore(self, &request) {
            let mut summary = index_summary(&snapshot);
            summary["update"] = json!({"mode":"checkpoint_unchanged","new_build_observed":false,"reused_build_runs":snapshot.builds.iter().map(|b|&b.id).collect::<Vec<_>>()});
            return Ok(summary);
        }
        self.index(request)
    }
    pub fn start(self: &Arc<Self>, request: IndexRequest) -> Result<Value> {
        let pending = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, job)| job.state["state"] == "publication_in_progress")
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in pending {
            self.job(&id, false)?;
        }
        let mut jobs = self.jobs.lock().unwrap();
        {
            for (id, job) in jobs.iter() {
                if (job.state["state"] == "running"
                    || job.state["state"] == "publication_in_progress")
                    && job.state["project"] == request.name
                {
                    return Ok(json!({"job":id,"state":job.state["state"],"joined":true}));
                }
            }
            if jobs
                .values()
                .filter(|job| job.state["state"] == "running")
                .count()
                >= 2
            {
                return Err(Error::new(
                    "IndexBusy",
                    "two compiler indexing jobs are already active",
                ));
            }
        }
        let id = cq_capture_protocol::new_id()?;
        let control = CaptureControl::default();
        jobs.insert(
            id.clone(),
            Job {
                control: control.clone(),
                state: json!({"state":"running","project":request.name}),
            },
        );
        drop(jobs);
        let service = self.clone();
        let job_id = id.clone();
        std::thread::spawn(move || {
            let result = index_repository(&request, &service.store, &control);
            let state = match result {
                Ok(v) => json!({"state":"complete","result":v}),
                Err(Error::PublicationInProgress { project, revision }) => {
                    json!({"state":"publication_in_progress","project":project,"revision":revision,"continue_with":{"operation":"publication_status","revision":revision}})
                }
                Err(e) => {
                    if let Some(revision) = e.candidate_revision() {
                        json!({"state":"commit_unknown","revision":revision,"project":request.name,"error":e.to_string(),"continue_with":{"operation":"resolve","revision":revision}})
                    } else {
                        json!({"state":if control.is_cancelled(){"cancelled"}else{"failed"},"error":e.to_string()})
                    }
                }
            };
            if let Some(j) = service.jobs.lock().unwrap().get_mut(&job_id) {
                j.state = state;
            }
        });
        Ok(json!({"job":id,"state":"running"}))
    }
    pub fn watch(self: &Arc<Self>, project: &str) -> Result<Value> {
        let request = self.load_request(project)?;
        let id = cq_capture_protocol::new_id()?;
        let control = CaptureControl::default();
        self.jobs.lock().unwrap().insert(
            id.clone(),
            Job {
                control: control.clone(),
                state: json!({"state":"watching","project":project}),
            },
        );
        let service = self.clone();
        let job = id.clone();
        std::thread::spawn(move || {
            let mut previous = cq_core::source_tree::manifest(&request.root).ok();
            loop {
                for _ in 0..20 {
                    if control.is_cancelled() {
                        if let Some(j) = service.jobs.lock().unwrap().get_mut(&job) {
                            j.state = json!({"state":"cancelled"});
                        }
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                match cq_core::source_tree::manifest(&request.root) {
                    Ok(current) => {
                        if previous.as_ref() == Some(&current) {
                            continue;
                        }
                        previous = Some(current);
                        if let Some(j) = service.jobs.lock().unwrap().get_mut(&job) {
                            j.state = json!({"state":"refreshing","project":request.name});
                        }
                        let result = index_repository(&request, &service.store, &control);
                        let state = match result {
                            Ok(s) => json!({"state":"watching","last_result":s}),
                            Err(e) => json!({"state":"watching","last_error":e.to_string()}),
                        };
                        if let Some(j) = service.jobs.lock().unwrap().get_mut(&job) {
                            j.state = state;
                        }
                    }
                    Err(e) => {
                        if let Some(j) = service.jobs.lock().unwrap().get_mut(&job) {
                            j.state = json!({"state":"watching","last_error":e.to_string()});
                        }
                    }
                }
            }
        });
        Ok(json!({"watch":id,"state":"watching"}))
    }
    pub fn shutdown(&self) {
        for job in self.jobs.lock().unwrap().values() {
            job.control.cancel();
        }
        // Give compiler-owning workers time to kill and reap their process groups.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if !self
                .jobs
                .lock()
                .unwrap()
                .values()
                .any(|j| j.state["state"] == "running" || j.state["state"] == "watching")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    pub fn job(&self, id: &str, cancel: bool) -> Result<Value> {
        let jobs = self.jobs.lock().unwrap();
        let j = jobs.get(id).ok_or_else(|| Error::new("MissingJob", id))?;
        if cancel {
            j.control.cancel();
        }
        let mut state = j.state.clone();
        let cancelled = j.control.is_cancelled();
        drop(jobs);
        if state["state"] == "publication_in_progress" {
            let project = state["project"].as_str().unwrap_or_default();
            let revision = state["revision"].as_str().unwrap_or_default();
            let status = self.dispatch(
                project,
                "publication_status",
                json!({"revision":revision}),
                None,
            )?;
            if let Some(outcome) = status["result"]["outcome"].as_str() {
                state = json!({"state":match outcome {"Committed"=>"complete","NotCommitted"=>"failed",_=>"commit_unknown"},"project":project,"revision":revision,"result":status});
                if let Some(j) = self.jobs.lock().unwrap().get_mut(id) {
                    j.state = state.clone();
                }
            }
        }
        Ok(json!({"job":id,"status":state,"cancellation_requested":cancelled}))
    }
    pub fn dispatch(
        &self,
        project: &str,
        operation: &str,
        args: Value,
        revision: Option<&str>,
    ) -> Result<Value> {
        self.dispatch_controlled(
            project,
            operation,
            args,
            revision,
            &CaptureControl::default(),
        )
    }
    pub fn dispatch_controlled(
        &self,
        project: &str,
        operation: &str,
        args: Value,
        revision: Option<&str>,
        control: &CaptureControl,
    ) -> Result<Value> {
        self.dispatch_inner(project, operation, args, revision, control, None)
    }
    pub fn dispatch_from_view(
        &self,
        project: &str,
        operation: &str,
        args: Value,
        reader: &dyn cq_core::ports::facts::FactReader,
        control: &CaptureControl,
    ) -> Result<Value> {
        self.dispatch_inner(
            project,
            operation,
            args,
            Some(reader.revision()),
            control,
            Some(reader),
        )
    }
    fn dispatch_inner(
        &self,
        project: &str,
        operation: &str,
        args: Value,
        revision: Option<&str>,
        control: &CaptureControl,
        guard: Option<&dyn cq_core::ports::facts::FactReader>,
    ) -> Result<Value> {
        if !args.is_object() {
            return Err(Error::new(
                "InvalidArguments",
                "query arguments must be an object",
            ));
        }
        let selected_operation = if operation == "query" {
            args["operation"].as_str().unwrap_or("overview")
        } else {
            operation
        }
        .to_owned();
        let operation = selected_operation.as_str();
        if operation == "resolve" {
            let revision = args["revision"].as_str().or(revision).ok_or_else(|| {
                Error::new("MissingRevision", "resolve needs the candidate revision")
            })?;
            return Ok(serde_json::to_value(
                self.store.resolve_publication(project, revision)?,
            )?);
        }
        if operation == "publication_status" {
            let revision = args["revision"].as_str().or(revision).ok_or_else(|| {
                Error::new(
                    "MissingRevision",
                    "publication_status requires a revision handle",
                )
            })?;
            let result = cq_store::runtime::Client::new(&self.store.root)?
                .call(json!({"op":"publication_status","project":project,"revision":revision}))?;
            if result["result"]["outcome"] == "Committed" {
                self.store.resolve_publication(project, revision)?;
            }
            return Ok(result);
        }
        if operation == "next" {
            let token = args["cursor"]
                .as_str()
                .ok_or_else(|| Error::new("MissingCursor", "next needs cursor"))?;
            let state = self.store.load_cursor(token)?;
            if let cq_core::query::ContinuationState::Store { project: owner, .. } = &state.state
                && owner != project
            {
                return Err(Error::new(
                    "WrongProject",
                    "cursor belongs to another project",
                ));
            }
            if state.request.operation == "next" {
                return Err(Error::new("InvalidCursor", "recursive continuation"));
            }
            let mut request = serde_json::to_value(&state.request)?;
            for key in ["limit", "max_bytes", "max_work", "timeout_ms"] {
                if let Some(value) = args.get(key) {
                    request[key] = value.clone();
                }
            }
            request["cursor"] = json!(token);
            return self.dispatch_inner(
                project,
                &state.request.operation,
                request,
                Some(&state.revision),
                control,
                guard,
            );
        }
        if operation == "delete" {
            self.store.delete_project_index(project)?;
            return Ok(json!({"state":"index_deleted","project":project}));
        }
        let cursor = args["cursor"]
            .as_str()
            .map(|id| self.store.load_cursor(id))
            .transpose()?;
        if let Some(cursor) = &cursor
            && let cq_core::query::ContinuationState::Store { project: owner, .. } = &cursor.state
            && owner != project
        {
            return Err(Error::new(
                "WrongProject",
                "cursor belongs to another project",
            ));
        }
        if let (Some(cursor), Some(revision)) = (&cursor, revision)
            && cursor.revision != revision
        {
            return Err(Error::new(
                "StaleCursor",
                "explicit revision differs from cursor",
            ));
        }
        let target_revision = if operation == "compare" {
            args["target"].as_str()
        } else {
            None
        };
        let pinned = cursor
            .as_ref()
            .map(|c| c.revision.as_str())
            .or(revision)
            .or(target_revision);
        let decodes_before = cq_store::snapshot_decode_count();
        if self.store.paged_enabled(project)
            && let Some(mut value) = self.dispatch_paged(
                project,
                operation,
                args.clone(),
                pinned,
                cursor.clone(),
                ReadExecution { control, guard },
            )?
        {
            if value.is_object() {
                value["read_stats"]["full_snapshot_decodes"] =
                    json!(cq_store::snapshot_decode_count() - decodes_before);
            }
            return Ok(value);
        }
        if self.store.paged_enabled(project) {
            let view = self.store.open_facts(project, pinned)?;
            let r = self
                .store
                .paged_manifest(project, cq_core::ports::facts::FactReader::revision(&view))?;
            let records = r.metadata["counts"]["entities"].as_u64().unwrap_or(0)
                + r.metadata["counts"]["relations"].as_u64().unwrap_or(0);
            let max_work = args["max_work"].as_u64().unwrap_or(1_000_000);
            let max_job_bytes = args["max_job_bytes"].as_u64().unwrap_or(64 * 1024 * 1024);
            if records > max_work || r.changes.inventory_bytes > max_job_bytes {
                let mut retry = args.clone();
                retry["operation"] = json!(operation);
                retry["max_work"] = json!(records.max(max_work));
                retry["max_job_bytes"] = json!(r.changes.inventory_bytes.max(max_job_bytes));
                return Err(Error::new(
                    "QueryWorkBudget",
                    json!({"message":"global/compatibility job requires explicit export; the suggested budgets permit admission, not a latency guarantee", "revision":r.revision,"required_records":records,"required_inventory_bytes":r.changes.inventory_bytes,"continue_with":retry}).to_string(),
                ));
            }
        }
        let snapshot = self.snapshot(project, pinned)?;
        match operation {
            "status" => Ok({
                let c = changes(&snapshot);
                json!({"project":project,"revision":snapshot.revision,"created_ms":snapshot.created_ms,"counts":index_summary(&snapshot),"changes":{"stale":c["stale"],"added_count":c["added"].as_array().map(Vec::len),"modified_count":c["modified"].as_array().map(Vec::len),"removed_count":c["removed"].as_array().map(Vec::len),"continue_with":{"operation":"changes"}},"coverage":snapshot.coverage.iter().take(3).collect::<Vec<_>>(),"gaps":snapshot.gaps.iter().take(7).collect::<Vec<_>>(),"hidden":[{"category":"coverage","count":snapshot.coverage.len().saturating_sub(3),"continue_with":{"operation":"coverage"}},{"category":"gaps","count":snapshot.gaps.len().saturating_sub(7),"continue_with":{"operation":"gaps"}}]})
            }),
            "changes" => Ok(changes(&snapshot)),
            "compare" => {
                let base = args["base"]
                    .as_str()
                    .ok_or_else(|| Error::new("MissingBase", "compare needs base revision"))?;
                let old = self.snapshot(project, Some(base))?;
                Ok(compare(&old, &snapshot))
            }
            "generation" => {
                let generation = self
                    .store
                    .load_checkpoint(project, &snapshot.revision)?
                    .ok_or_else(|| {
                        Error::new("MissingCheckpoint", "refresh to establish a generation")
                    })?;
                let delta = self.store.generation_delta(project, &snapshot.revision)?;
                Ok(
                    json!({"revision":snapshot.revision,"generation":generation.generation,"universe":generation.universe,"sequence":generation.sequence,"base_generation":delta.base_generation,"upserts":delta.upserts.iter().map(|s|&s.key).collect::<Vec<_>>(),"deletes":delta.deletes,"manifest":generation.manifest,"meaning":"validated semantic data deltas; analysis reuse is separately reported"}),
                )
            }
            "annotations" => Ok(json!(self.store.annotations(project, &snapshot)?)),
            "annotate" => {
                let mut value = args;
                value["id"] = json!(cq_capture_protocol::new_id().map(|id| digest(id.as_bytes()))?);
                value["project"] = json!(project);
                value["revision"] = json!(snapshot.revision);
                value["created_ms"] = json!(cq_capture_protocol::now_ms());
                for (k, v) in [
                    ("parent", Value::Null),
                    ("kind", json!("Hypothesis")),
                    ("author", json!("unattributed")),
                    ("entities", json!([])),
                    ("evidence", json!([])),
                    ("depends_on", json!([])),
                    ("status", json!("Proposed")),
                ] {
                    if value.get(k).is_none() {
                        value[k] = v;
                    }
                }
                let mut annotation: Annotation = serde_json::from_value(value)?;
                if annotation.text.trim().is_empty() {
                    return Err(Error::new("MissingText", "annotation needs text"));
                }
                for a in &annotation.evidence {
                    if !annotation.depends_on.contains(&a.file_revision) {
                        annotation.depends_on.push(a.file_revision.clone());
                    }
                }
                for id in &annotation.entities {
                    if let Some(e) = snapshot.entities.iter().find(|e| &e.id == id)
                        && let Some(a) = e.extent.as_ref().or(e.source.as_ref())
                        && !annotation.depends_on.contains(&a.file_revision)
                    {
                        annotation.depends_on.push(a.file_revision.clone());
                    }
                }
                self.store.save_annotation(&annotation)?;
                Ok(json!(annotation))
            }
            "cypher" => {
                let q = query::request("cypher", args.clone())?;
                let state = if let Some(cursor) = cursor {
                    cursor
                } else {
                    let result = self
                        .store
                        .cypher(&snapshot, &q.text, 100000, q.timeout_ms)?;
                    if result["truncated"] == true {
                        return Err(Error::new(
                            "QueryWorkBudget",
                            "Cypher result exceeds 100000 rows; add predicates or aggregation before paging",
                        ));
                    }
                    let rows = result["rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| json!({"columns":result["columns"],"values":r}))
                        .collect::<Vec<_>>();
                    cq_core::query::QueryContinuation {
                        request: q.clone(),
                        revision: snapshot.revision.clone(),
                        query_digest: cq_core::query::request_digest(&q),
                        state: cq_core::query::ContinuationState::Rows {
                            total: Some(rows.len()),
                            rows,
                            offset: q.offset,
                            complete: true,
                            after: None,
                        },
                    }
                };
                let result =
                    QueryEngine::new(&snapshot, &self.store.content).execute(&q, Some(state))?;
                let mut page = result.page;
                if let Some(state) = result.continuation {
                    page.next_cursor = Some(self.store.save_cursor(&state)?);
                }
                let mut value = json!(page);
                value["rows"] = json!(
                    page.items
                        .iter()
                        .map(|r| r["values"].clone())
                        .collect::<Vec<_>>()
                );
                value["columns"] = page
                    .items
                    .first()
                    .map(|r| r["columns"].clone())
                    .unwrap_or(json!([]));
                Ok(value)
            }
            "traces" => self.traces(&snapshot, args),
            _ => {
                let q = query::request(operation, args)?;
                let search = Arc::new(cq_core::query::search::SearchIndex::with_model(
                    if ["search", "semantic", "entrypoints", "tour"].contains(&q.operation.as_str())
                    {
                        &snapshot.entities
                    } else {
                        &[]
                    },
                    self.model.clone(),
                ));
                let result = QueryEngine::with_search(&snapshot, &self.store.content, search)
                    .execute(&q, cursor)?;
                let mut page = result.page;
                if let Some(c) = result.continuation {
                    page.next_cursor = Some(self.store.save_cursor(&c)?);
                    for hidden in &mut page.hidden {
                        if hidden.category == "remaining_query_results" {
                            hidden.continue_with =
                                json!({"operation":"next","cursor":page.next_cursor});
                        }
                    }
                }
                let mut value = json!(page);
                if self.store.paged_enabled(project) {
                    value["execution_path"] = json!("explicit_compatibility_export_job");
                    value["full_snapshot_decodes"] =
                        json!(cq_store::snapshot_decode_count() - decodes_before);
                } else {
                    value["execution_path"] = json!("legacy_full_snapshot_compatibility");
                    value["full_snapshot_decodes"] =
                        json!(cq_store::snapshot_decode_count() - decodes_before);
                }
                Ok(value)
            }
        }
    }
    fn dispatch_paged(
        &self,
        project: &str,
        operation: &str,
        args: Value,
        revision: Option<&str>,
        cursor: Option<cq_core::query::QueryContinuation>,
        execution: ReadExecution<'_>,
    ) -> Result<Option<Value>> {
        use cq_core::ports::facts::FactReader;
        let ReadExecution { control, guard } = execution;
        let request = guard.map(|r| r.request_view()).transpose()?.flatten();
        let guard = request.as_deref().or(guard);
        let owned = if guard.is_none() {
            Some(self.store.open_facts(project, revision)?)
        } else {
            None
        };
        let reader: &dyn FactReader = if let Some(reader) = guard {
            if reader.project() != project || revision.is_some_and(|r| r != reader.revision()) {
                return Err(Error::new(
                    "StaleCursor",
                    "request differs from pinned execution view",
                ));
            }
            reader
        } else {
            owned.as_ref().unwrap()
        };
        let metadata = reader.metadata();
        if operation == "cypher"
            && let Some((column, count)) = cq_core::query::architecture::cypher_count(
                metadata,
                args["text"].as_str().unwrap_or(""),
            )
        {
            return Ok(Some(
                json!({"revision":reader.revision(),"context":null,"items":[{"columns":[column],"values":[count]}],"total":1,"total_kind":"exact","truncated":false,"next_cursor":null,"state":"results","coverage":[],"gaps":[],"hidden":[],"work":0,"elapsed_ms":0,"read_stats":reader.stats(),"execution_path":"precomputed_revision_statistics"}),
            ));
        }
        if operation == "annotations" {
            return Ok(Some(json!(self.store.paged_annotations(project, reader)?)));
        }
        if operation == "annotate" {
            let mut value = args;
            value["id"] = json!(digest(cq_capture_protocol::new_id()?.as_bytes()));
            value["project"] = json!(project);
            value["revision"] = json!(reader.revision());
            value["created_ms"] = json!(cq_capture_protocol::now_ms());
            for (k, v) in [
                ("parent", Value::Null),
                ("kind", json!("Hypothesis")),
                ("author", json!("unattributed")),
                ("entities", json!([])),
                ("evidence", json!([])),
                ("depends_on", json!([])),
                ("status", json!("Proposed")),
            ] {
                if value.get(k).is_none() {
                    value[k] = v;
                }
            }
            let mut a: Annotation = serde_json::from_value(value)?;
            if a.text.trim().is_empty() {
                return Err(Error::new("MissingText", "annotation needs text"));
            }
            for e in &a.evidence {
                if !a.depends_on.contains(&e.file_revision) {
                    a.depends_on.push(e.file_revision.clone());
                }
            }
            for id in &a.entities {
                let row = reader
                    .get("entity", id)?
                    .ok_or_else(|| Error::new("MissingEntity", id))?;
                let e: Entity = serde_json::from_value(row.value)?;
                if let Some(anchor) = e.extent.or(e.source)
                    && !a.depends_on.contains(&anchor.file_revision)
                {
                    a.depends_on.push(anchor.file_revision);
                }
            }
            self.store.save_annotation(&a)?;
            return Ok(Some(json!(a)));
        }
        if operation == "status" {
            return Ok(Some(
                json!({"project":project,"revision":reader.revision(),"created_ms":metadata["created_ms"],"counts":metadata["counts"],"database":"Ladybug","format":2,"read_stats":reader.stats()}),
            ));
        }
        if operation == "generation" {
            let r = self.store.paged_manifest(project, reader.revision())?;
            let mut value = r.metadata["semantic_generation"].clone();
            if value["state"] == "explicit_generation_job_required" && args["explicit_job"] == true
            {
                let cost = metadata["counts"]["entities"].as_u64().unwrap_or(0)
                    + metadata["counts"]["relations"].as_u64().unwrap_or(0)
                    + metadata["counts"]["occurrences"].as_u64().unwrap_or(0);
                if args["max_work"].as_u64().unwrap_or(0) < cost {
                    return Err(Error::new(
                        "QueryWorkBudget",
                        "explicit generation export requires max_work >= entity+relation+occurrence count",
                    ));
                }
                let snapshot = self.store.export_paged(project, Some(reader.revision()))?;
                let generation = cq_core::semantic::generation::project(&snapshot, r.sequence)?;
                value = json!({"generation":generation.generation,"universe":generation.universe,"manifest":generation.manifest,"state":"explicit_full_generation_job","upserts":Value::Null,"deletes":Value::Null});
            } else if value["state"] == "explicit_generation_job_required" {
                value["continue_with"] = json!({"operation":"generation","explicit_job":true,"max_work":metadata["counts"]["entities"].as_u64().unwrap_or(0)+metadata["counts"]["relations"].as_u64().unwrap_or(0)+metadata["counts"]["occurrences"].as_u64().unwrap_or(0)});
            }
            value["revision"] = json!(r.revision);
            value["sequence"] = json!(r.sequence);
            value["roots"] = json!(r.roots);
            value["physical_changes"] = json!(r.changes);
            return Ok(Some(value));
        }
        if operation == "changes" {
            let root: NativeValue = serde_json::from_value(metadata["root"].clone())?;
            let current = cq_core::source_tree::manifest(&root.path())?;
            let mut old = BTreeMap::new();
            let mut after = None;
            loop {
                let b = reader.scan("all:working_tree", after.as_deref(), 256)?;
                for row in b.rows {
                    old.insert(
                        row.key.strip_prefix("working_tree:").unwrap().to_owned(),
                        row.value,
                    );
                }
                if b.complete {
                    break;
                }
                after = b.after;
            }
            let modified = current
                .iter()
                .filter(|(p, v)| old.get(*p).is_some_and(|old| old.as_str() != Some(v)))
                .map(|(p, _)| p)
                .collect::<Vec<_>>();
            let added = current
                .keys()
                .filter(|p| !old.contains_key(*p))
                .collect::<Vec<_>>();
            let removed = old
                .keys()
                .filter(|p| !current.contains_key(*p))
                .collect::<Vec<_>>();
            return Ok(Some(
                json!({"revision":reader.revision(),"stale":!modified.is_empty()||!added.is_empty()||!removed.is_empty(),"modified":modified,"added":added,"removed":removed,"sampled_ms":cq_capture_protocol::now_ms(),"scope":"live input manifest compared with fixed facts"}),
            ));
        }
        if operation == "compare" {
            let base = args["base"]
                .as_str()
                .ok_or_else(|| Error::new("MissingBase", "compare needs base"))?;
            let old = self.store.open_facts(project, Some(base))?;
            let files = |r: &dyn FactReader| -> Result<BTreeMap<String, BlobId>> {
                let mut files = BTreeMap::new();
                let mut after = None;
                loop {
                    let b = r.scan("all:file", after.as_deref(), 256)?;
                    for row in b.rows {
                        let f: FileRevision = serde_json::from_value(row.value)?;
                        files.insert(f.logical_path.display(), f.blob);
                    }
                    if b.complete {
                        break;
                    }
                    after = b.after;
                }
                Ok(files)
            };
            let a = files(&old)?;
            let b = files(reader)?;
            return Ok(Some(
                json!({"base":base,"target":reader.revision(),"added_files":b.keys().filter(|p|!a.contains_key(*p)).collect::<Vec<_>>(),"removed_files":a.keys().filter(|p|!b.contains_key(*p)).collect::<Vec<_>>(),"modified_files":b.iter().filter(|(p,v)|a.get(*p).is_some_and(|x|x!=*v)).map(|(p,_)|p).collect::<Vec<_>>(),"entity_delta":metadata["counts"]["entities"].as_i64().unwrap_or(0)-old.metadata()["counts"]["entities"].as_i64().unwrap_or(0),"relation_delta":metadata["counts"]["relations"].as_i64().unwrap_or(0)-old.metadata()["counts"]["relations"].as_i64().unwrap_or(0)}),
            ));
        }
        if [
            "search_code",
            "at",
            "overview",
            "schema",
            "definition",
            "type_definition",
            "details",
            "members",
            "bodies",
            "references",
            "occurrences",
            "callers",
            "callees",
            "relations",
            "implementations",
            "search",
            "semantic",
            "entrypoints",
            "outline",
            "docs",
            "rustdoc",
            "doc_search",
            "document",
            "source",
            "coverage",
            "capabilities",
            "gaps",
            "contexts",
            "scopes",
            "observations",
            "provenance_observations",
            "runtime_traces",
            "topology",
            "build_configuration",
            "trace",
            "path",
            "impact",
        ]
        .contains(&operation)
        {
            let q = query::request(operation, args)?;
            let result = cq_core::query::paged::Engine {
                reader,
                model: self.model.clone(),
                control: Some(control),
            }
            .execute(&q, cursor)?;
            let mut page = result.page;
            if let Some(state) = result.continuation {
                page.next_cursor = Some(self.store.save_cursor(&state)?);
                for h in &mut page.hidden {
                    if h.category == "remaining_query_results" {
                        h.continue_with = json!({"operation":"next","cursor":page.next_cursor});
                    }
                }
            }
            let mut value = json!(page);
            value["read_stats"] = json!(reader.stats());
            value["read_stats_scope"] = json!("request");
            value["execution_path"] = json!("fixed_view_fact_reader");
            return Ok(Some(value));
        }
        // Remaining global/compatibility operations are explicit, bounded jobs;
        // they never silently masquerade as the local FactReader fast path.
        Ok(None)
    }
    pub fn start_refresh(self: &Arc<Self>, project: &str) -> Result<Value> {
        self.start(self.load_request(project)?)
    }
    fn traces(&self, s: &SemanticSnapshot, args: Value) -> Result<Value> {
        if args.get("events").is_none() {
            let q = query::request("runtime_traces", args)?;
            let result = QueryEngine::new(s, &self.store.content).execute(&q, None)?;
            let mut page = result.page;
            if let Some(state) = result.continuation {
                page.next_cursor = Some(self.store.save_cursor(&state)?);
            }
            return Ok(json!(page));
        }
        let events = args["events"]
            .as_array()
            .ok_or_else(|| Error::new("InvalidTrace", "events must be an array"))?;
        let ids: std::collections::HashSet<_> = s.entities.iter().map(|e| e.id.as_str()).collect();
        let source = args["source"].as_str().unwrap_or("caller-supplied");
        let raw = serde_json::to_vec(&args)?;
        let raw = self.store.content.put(&raw, 1024 * 1024 * 1024)?;
        let mut next = s.clone();
        next.previous = Some(s.revision.clone());
        next.created_ms = cq_capture_protocol::now_ms();
        for event in events {
            let from = event["from"]
                .as_str()
                .ok_or_else(|| Error::new("InvalidTrace", "missing from"))?;
            let to = event["to"]
                .as_str()
                .ok_or_else(|| Error::new("InvalidTrace", "missing to"))?;
            if !ids.contains(from) || !ids.contains(to) {
                return Err(Error::new(
                    "InvalidTrace",
                    "endpoints must be exact entity ids in the selected revision",
                ));
            }
            let count = event
                .get("count")
                .map(|v| {
                    v.as_u64()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| Error::new("InvalidTrace", "count must be positive"))
                })
                .transpose()?
                .unwrap_or(1);
            let id = digest(&serde_json::to_vec(&(event, source))?);
            if next.runtime_traces.iter().any(|t| t.id == id) {
                continue;
            }
            next.runtime_traces.push(RuntimeTrace {
                id,
                from: from.into(),
                to: to.into(),
                count,
                source: source.into(),
                raw: raw.clone(),
                observed_revision: s.revision.clone(),
                recorded_ms: next.created_ms,
            });
            next.add_relation(
                from,
                to,
                RelationKind::ObservedCall,
                None,
                None,
                Provenance {
                    provider: "runtime-trace-import".into(),
                    version: "1".into(),
                    authority: Authority::RuntimeObservation,
                    build_run: None,
                    note: format!(
                        "caller-supplied observation; source={source}; count={count}; raw={}",
                        raw.digest
                    ),
                },
            );
        }
        cq_core::semantic::production::refresh_descriptors(&mut next)?;
        next.seal()?;
        if self.store.paged_enabled(&s.project) {
            self.store
                .publish_paged_semantics(&next, Some(&s.revision))?;
        } else {
            self.store.publish_semantics(&next, Some(&s.revision))?;
        }
        Ok(
            json!({"revision":next.revision,"previous":s.revision,"runtime_traces":next.runtime_traces.len(),"static_calls":next.relations.iter().filter(|r|r.kind==RelationKind::Calls).count(),"state":"runtime_layer_published","meaning":"OBSERVED_CALL retains caller-supplied provenance and never overwrites static CALLS"}),
        )
    }
}
pub fn index_summary(s: &SemanticSnapshot) -> Value {
    let mut value = json!({"project":s.project,"revision":s.revision,"previous":s.previous,"entities":s.entities.len(),"relations":s.relations.len(),"occurrences":s.occurrences.len(),"bodies":s.bodies.len(),"documents":s.documents.len(),"contexts":s.contexts.len(),"unresolved_calls":s.occurrences.iter().filter(|o|o.kind==RelationKind::Calls&&o.executable.resolved().is_none()).count(),"database":"Ladybug","native_reused":s.providers.iter().filter(|p|p.reused_from.is_some()).count(),"analysis":s.providers.iter().filter_map(|p|p.analysis.as_ref()).collect::<Vec<_>>(),"build_outcomes":s.builds.iter().map(|b|&b.outcome).collect::<Vec<_>>(),"coverage_gaps":s.gaps.len()});
    if let Some(p) = &s.production {
        value["production"] = json!({"partial":p.partial,"selection_complete":p.selection_complete,"scopes":p.scopes.len(),"identity_unmatched":p.identity_unmatched,"stage_ms":p.stage_ms,"native_boundary":p.native_boundary,"transport":p.transport});
    }
    value
}
fn changes(s: &SemanticSnapshot) -> Value {
    match cq_core::source_tree::manifest(&s.root.path()) {
        Ok(current) => {
            let modified: Vec<_> = current
                .iter()
                .filter(|(p, v)| s.working_tree_manifest.get(*p).is_some_and(|old| old != *v))
                .map(|(p, _)| p)
                .collect();
            let added: Vec<_> = current
                .keys()
                .filter(|p| !s.working_tree_manifest.contains_key(*p))
                .collect();
            let removed: Vec<_> = s
                .working_tree_manifest
                .keys()
                .filter(|p| !current.contains_key(*p))
                .collect();
            json!({"modified":modified,"added":added,"removed":removed,"stale":!modified.is_empty()||!added.is_empty()||!removed.is_empty(),"revision":s.revision,"scope":"source/document/config file manifest; source reads are still served from the indexed revision"})
        }
        Err(e) => {
            json!({"state":"working_tree_unavailable","error":e.to_string(),"stale":"unknown","revision":s.revision})
        }
    }
}

fn compare(a: &SemanticSnapshot, b: &SemanticSnapshot) -> Value {
    let old: BTreeMap<_, _> = a
        .sources
        .files
        .iter()
        .map(|f| (f.logical_path.display(), &f.blob))
        .collect();
    let new: BTreeMap<_, _> = b
        .sources
        .files
        .iter()
        .map(|f| (f.logical_path.display(), &f.blob))
        .collect();
    let added: Vec<_> = new.keys().filter(|p| !old.contains_key(*p)).collect();
    let removed: Vec<_> = old.keys().filter(|p| !new.contains_key(*p)).collect();
    let modified: Vec<_> = new
        .iter()
        .filter(|(p, v)| old.get(*p).is_some_and(|x| x != *v))
        .map(|(p, _)| p)
        .collect();
    json!({"base":a.revision,"target":b.revision,"added_files":added,"removed_files":removed,"modified_files":modified,"entity_delta":b.entities.len() as i64-a.entities.len() as i64,"relation_delta":b.relations.len() as i64-a.relations.len() as i64,"matching":"content paths; symbol instances remain compilation-context scoped"})
}

pub use super::languages::{index_cpp, index_go};
