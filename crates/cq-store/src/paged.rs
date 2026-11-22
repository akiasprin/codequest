//! Application-facing format-2 publication, explicit export and migration.
use crate::{
    LocalStore,
    facts::{
        objects::Objects,
        writer::{self, Revision},
    },
    runtime::Client,
};
use cq_core::ports::facts::FactReader;
use cq_core::{Error, Result, model::*, ports::facts::CommitOutcome};
use std::collections::BTreeSet;
impl LocalStore {
    /// Resolve a possibly completed publication and restore its application
    /// registration. The native HEAD and admitted immutable view are authoritative;
    /// no compilation or fact publication is repeated here.
    pub fn resolve_publication(
        &self,
        project: &str,
        revision: &str,
    ) -> Result<cq_core::ports::facts::CommitResult> {
        let client = Client::new(&self.root)?;
        let result: cq_core::ports::facts::CommitResult = serde_json::from_value(
            client
                .call(serde_json::json!({"op":"resolve","project":project,"revision":revision}))?,
        )?;
        if result.outcome != CommitOutcome::Committed {
            return Ok(result);
        }
        // Open through the host directly: the local format marker may be the
        // exact write that the interrupted frontend never reached.
        let view = client.open(project, None)?;
        let active = self.paged_manifest(project, view.revision())?;
        let root: NativeValue = serde_json::from_value(active.metadata["root"].clone())?;
        let dir = self.project_path(project);
        if dir.join("project.json").is_file() && self.project_info(project)?.root != root {
            return Err(Error::new(
                "ProjectRootMismatch",
                "registered root differs from admitted HEAD",
            ));
        }
        let mut languages = vec![];
        let mut after = None;
        loop {
            let batch = view.scan("all:unit", after.as_deref(), 128)?;
            for row in batch.rows {
                languages.push(serde_json::from_value::<Language>(
                    row.value["language"].clone(),
                )?);
            }
            if batch.complete {
                break;
            }
            after = batch.after;
        }
        languages.sort_by_key(|l| format!("{l:?}"));
        languages.dedup();
        crate::revisions::save(
            &dir.join("project.json"),
            &crate::graph::ProjectInfo {
                name: project.into(),
                root,
                head: Some(active.revision),
                created_ms: active.metadata["created_ms"].as_u64().ok_or_else(|| {
                    Error::new(
                        "InvalidManifest",
                        "created_ms missing from admitted revision",
                    )
                })?,
                languages,
            },
        )?;
        crate::atomic::immutable_write(&dir.join("format-2"), b"2")?;
        let staging = self
            .root
            .join("format-2/staging")
            .join(format!("{revision}.json"));
        let _guard = crate::revisions::retention::ObjectGate::lock(&self.root.join("format-2"))?;
        if staging.exists() {
            std::fs::remove_file(&staging)?;
            std::fs::File::open(staging.parent().unwrap())?.sync_all()?;
        }
        Ok(result)
    }
    pub fn ensure_paged_checkpoint(&self, r: &Revision) -> Result<bool> {
        let coordinates = &r.metadata["semantic_generation"];
        let path = self.revision_path(&r.revision)?.join("checkpoint.json");
        let expected = serde_json::json!({"format":2,"coordinates":coordinates,"digest":digest(&serde_json::to_vec(&(2,coordinates))?)});
        if path.is_file()
            && serde_json::from_reader::<_, serde_json::Value>(std::fs::File::open(&path)?)
                .ok()
                .as_ref()
                == Some(&expected)
        {
            return Ok(false);
        }
        crate::revisions::save(&path, &expected)?;
        Ok(true)
    }
    pub fn paged_annotations(
        &self,
        project: &str,
        current: &dyn cq_core::ports::facts::FactReader,
    ) -> Result<Vec<Annotation>> {
        let path = self.project_path(project).join("annotations");
        if !path.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for file in std::fs::read_dir(path)? {
            let path = file?.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let mut a: Annotation = serde_json::from_reader(std::fs::File::open(path)?)?;
            if a.revision != current.revision() {
                let old = self.open_facts(project, Some(&a.revision))?;
                let mut stale = a.depends_on.is_empty();
                for id in &a.depends_on {
                    if old.get("file", id)?.is_none() || current.get("file", id)?.is_none() {
                        stale = true;
                    }
                }
                // Set-valued hypotheses need a collection witness. Until they
                // have one, conservatively require review on any fact change.
                if a.kind == AnnotationKind::Responsibility || a.kind == AnnotationKind::Hypothesis
                {
                    stale = true;
                }
                if stale {
                    a.status = ReviewStatus::Stale;
                }
            }
            out.push(a);
        }
        out.sort_by_key(|a| a.created_ms);
        Ok(out)
    }
    pub fn paged_enabled(&self, project: &str) -> bool {
        self.project_path(project).join("format-2").is_file()
    }
    pub fn paged_manifest(&self, project: &str, revision: &str) -> Result<Revision> {
        if !crate::facts::objects::valid_hash(revision) {
            return Err(Error::new("InvalidRevision", revision));
        }
        let path = self
            .root
            .join("format-2/projects")
            .join(digest(project.as_bytes()))
            .join("commits")
            .join(revision);
        let r: Revision = serde_json::from_reader(std::fs::File::open(path)?)?;
        r.verify()?;
        if r.project != project || r.revision != revision {
            return Err(Error::new("WrongProject", project));
        }
        Ok(r)
    }
    pub fn open_facts(
        &self,
        project: &str,
        revision: Option<&str>,
    ) -> Result<crate::runtime::RemoteView> {
        if !self.paged_enabled(project) {
            return Err(Error::new(
                "MigrationRequired",
                "run migrate-store --project NAME to enable fact pages",
            ));
        }
        Client::new(&self.root)?.open(project, revision)
    }
    pub fn open_session_facts(
        &self,
        project: &str,
        revision: Option<&str>,
    ) -> Result<crate::runtime::RemoteView> {
        if !self.paged_enabled(project) {
            return Err(Error::new(
                "MigrationRequired",
                "project uses the legacy format",
            ));
        }
        Client::new(&self.root)?.open_session(project, revision)
    }
    pub fn publish_paged_semantics(
        &self,
        s: &SemanticSnapshot,
        expected: Option<&str>,
    ) -> Result<Revision> {
        self.publish_impl(s, expected, None)
    }
    pub fn publish_analysis(&self, prepared: &crate::analysis::Prepared) -> Result<Revision> {
        self.publish_impl(
            &prepared.header,
            prepared.header.previous.as_deref(),
            Some(prepared),
        )
    }
    pub fn resume_candidate_directories(
        &self,
        candidate: &crate::execution::Candidate,
    ) -> Result<Revision> {
        let execution = self
            .execution
            .as_ref()
            .ok_or_else(|| Error::new("MissingExecution", "directory resume"))?;
        let _gate = crate::revisions::retention::ObjectGate::lock(&self.root.join("format-2"))?;
        let objects = Objects::buffered(self.root.join("format-2/objects"))?;
        let revision = writer::resume_directories(
            &objects,
            execution,
            &candidate.header,
            &candidate.revision,
        )?;
        execution.save_candidate(&candidate.header, &revision, candidate.transport.clone())?;
        Ok(revision)
    }
    pub fn encode_analysis(&self, prepared: &crate::analysis::Prepared) -> Result<Revision> {
        self.encode_impl(
            &prepared.header,
            prepared.header.previous.as_deref(),
            Some(prepared),
        )
    }
    fn publish_impl(
        &self,
        s: &SemanticSnapshot,
        expected: Option<&str>,
        prepared: Option<&crate::analysis::Prepared>,
    ) -> Result<Revision> {
        let revision = self.encode_impl(s, expected, prepared)?;
        self.publish_candidate(&revision)
    }
    pub fn publish_candidate(&self, revision: &Revision) -> Result<Revision> {
        revision.verify()?;
        let client = Client::new(&self.root)?;
        let result = client.publish(revision, revision.parent.as_deref())?;
        if result.outcome != CommitOutcome::Committed {
            return Err(if result.outcome == CommitOutcome::Unknown {
                Error::CommitUnknown {
                    revision: revision.revision.clone(),
                    message: result.detail.unwrap_or_default(),
                }
            } else {
                Error::new(
                    "PublishRejected",
                    result
                        .detail
                        .unwrap_or_else(|| format!("{:?}", result.outcome)),
                )
            });
        }
        let result = self.resolve_publication(&revision.project, &revision.revision)?;
        if result.outcome != CommitOutcome::Committed {
            return Err(Error::new(
                "PublishRejected",
                "publication registration incomplete",
            ));
        }
        Ok(revision.clone())
    }
    fn encode_impl(
        &self,
        s: &SemanticSnapshot,
        expected: Option<&str>,
        prepared: Option<&crate::analysis::Prepared>,
    ) -> Result<Revision> {
        if prepared.is_some() {
            cq_core::semantic::validate::validate_metadata(s, &self.content)?;
        } else {
            cq_core::semantic::validate::validate(s, &self.content)?;
        }
        let partial = s.production.as_ref().is_some_and(|p| p.partial)
            && s.analysis_request
                .as_ref()
                .is_some_and(|r| r["allow_partial"] == true);
        if s.builds
            .iter()
            .any(|b| b.outcome != BuildOutcome::Succeeded)
            && !partial
        {
            return Err(Error::new(
                "IncompleteAnalysis",
                "failed/cancelled builds cannot publish a complete revision",
            ));
        }
        if self.execution.is_none()
            && self.paged_enabled(&s.project)
            && let Ok(r) = self.paged_manifest(&s.project, &s.revision)
        {
            let outcome = Client::new(&self.root)?.call(serde_json::json!({
                "op":"resolve","project":s.project,"revision":s.revision
            }))?;
            if serde_json::from_value::<cq_core::ports::facts::CommitResult>(outcome)?.outcome
                == CommitOutcome::Committed
            {
                return Ok(r);
            }
        }
        if self.head(&s.project)?.as_deref() != expected
            && self.head(&s.project)?.as_deref() != Some(&s.revision)
        {
            return Err(Error::new("PublishConflict", "HEAD changed"));
        }
        if expected.is_some() && !self.paged_enabled(&s.project) {
            self.migrate_paged(&s.project)?;
        }
        let prior = expected
            .map(|id| self.paged_manifest(&s.project, id))
            .transpose()?;
        if let Some(prior) = &prior
            && prior.metadata["root"] != serde_json::to_value(&s.root)?
        {
            return Err(Error::new(
                "ProjectRootMismatch",
                "project namespace cannot change repository root",
            ));
        }
        let objects = Objects::buffered(self.root.join("format-2/objects"))?;
        let staging = self
            .root
            .join("format-2/staging")
            .join(format!("{}.json", s.revision));
        let revision = {
            let _guard = crate::revisions::retention::ObjectGate::lock(
                self.root.join("format-2").as_path(),
            )?;
            let prepare_started = std::time::Instant::now();
            let revision = if let Some(p) = prepared {
                if let Some(e) = &self.execution {
                    writer::scoped_tracked(&objects, &self.content, p, e)?
                } else {
                    writer::scoped(&objects, &self.content, p)?
                }
            } else {
                writer::snapshot(&objects, &self.content, s, prior.as_ref())?
            };
            objects.sync_pending()?;
            cq_core::progress::Progress::stage(
                &self.root,
                &s.project,
                "audit",
                "核验发布内容",
                None,
                "事实",
            );
            let prepared_ms = prepare_started.elapsed().as_millis() as u64;
            let counts = objects.write_counts();
            let attempts = self
                .root
                .join("format-2/publication-attempts")
                .join(format!(
                    "{}-{}.json",
                    s.revision,
                    cq_capture_protocol::new_id()?
                ));
            crate::revisions::save(
                &attempts,
                &serde_json::json!({"revision":s.revision,"objects_written":counts.0,"object_bytes_written":counts.1,"prepare_ms":prepared_ms,"changes":revision.changes,"resources":objects.resource_stats(),"phase":"canonical objects and durability barrier; native index writes are separate"}),
            )?;
            self.ensure_paged_checkpoint(&revision)?;
            let admitted = if self.head(&s.project)?.as_deref() == Some(&revision.revision) {
                let old = self.paged_manifest(&s.project, &revision.revision)?;
                if old.seal != revision.seal {
                    return Err(Error::new(
                        "RevisionConflict",
                        "recomputed content differs from the admitted immutable revision",
                    ));
                }
                true
            } else {
                false
            };
            if !admitted {
                crate::atomic::immutable_write(&staging, &serde_json::to_vec(&revision)?)?;
            }

            revision
        };
        Ok(revision)
    }
    /// Migration is explicit and retains every legacy artifact. The active
    /// format marker is written only after the original HEAD is fully imported.
    pub fn migrate_paged(&self, project: &str) -> Result<serde_json::Value> {
        let legacy = self.project_path(project);
        std::fs::create_dir_all(&legacy)?;
        let _migration = crate::graph::Lock::open(&legacy.join("publish.lock"))?;
        if self.paged_enabled(project) {
            return Ok(serde_json::json!({"state":"already_migrated","project":project}));
        }
        let original = self
            .head(project)?
            .ok_or_else(|| Error::new("MissingRevision", "index first"))?;
        let mut ids = vec![];
        let mut next = Some(original.clone());
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            if !seen.insert(id.clone()) {
                return Err(Error::new("CorruptRevision", "history cycle"));
            }
            let s = self.load_semantics(project, Some(&id))?;
            next = s.previous;
            ids.push(id);
        }
        ids.reverse();
        #[cfg(debug_assertions)]
        {
            let hooks = legacy.join("test-hooks");
            if hooks.join("migration-snapshot").is_file() {
                std::fs::write(hooks.join("ready-migration-snapshot"), b"ready")?;
                while !hooks.join("release-migration-snapshot").is_file() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        let client = Client::new(&self.root)?;
        let objects = Objects::buffered(self.root.join("format-2/objects"))?;
        let mut prior = None;
        for id in &ids {
            let s = self.load_semantics(project, Some(id))?;
            let staging = self
                .root
                .join("format-2/staging")
                .join(format!("{}.json", s.revision));
            let r = {
                let _objects =
                    crate::revisions::retention::ObjectGate::lock(&self.root.join("format-2"))?;
                let r = writer::snapshot(&objects, &self.content, &s, prior.as_ref())?;
                objects.sync_pending()?;
                crate::atomic::immutable_write(&staging, &serde_json::to_vec(&r)?)?;
                r
            };
            // Independently check lossless round-trip before switching reads.
            let exported = writer::export(&objects, &r)?;
            let canonical = |s: &SemanticSnapshot| -> Result<serde_json::Value> {
                let mut v = serde_json::to_value(s)?;
                for key in [
                    "entities",
                    "bodies",
                    "occurrences",
                    "relations",
                    "documents",
                    "units",
                    "contexts",
                    "builds",
                    "runtime_traces",
                ] {
                    if let Some(a) = v[key].as_array_mut() {
                        a.sort_by_key(|x| x["id"].as_str().unwrap_or("").to_owned());
                    }
                }
                Ok(v)
            };
            if canonical(&s)? != canonical(&exported)? {
                return Err(Error::new("MigrationMismatch", id));
            }
            let outcome =
                client.publish(&r, prior.as_ref().map(|r: &Revision| r.revision.as_str()))?;
            if outcome.outcome != CommitOutcome::Committed {
                return Err(Error::new(
                    "MigrationPublication",
                    serde_json::to_string(&outcome)?,
                ));
            }
            {
                let _objects =
                    crate::revisions::retention::ObjectGate::lock(&self.root.join("format-2"))?;
                std::fs::remove_file(&staging)?;
                std::fs::File::open(staging.parent().unwrap())?.sync_all()?;
            }
            prior = Some(r);
        }
        if self.head(project)?.as_deref() != Some(&original) {
            return Err(Error::new(
                "PublishConflict",
                "legacy HEAD changed during migration",
            ));
        }
        crate::atomic::immutable_write(&self.project_path(project).join("format-2"), b"2")?;
        Ok(
            serde_json::json!({"state":"migrated","project":project,"head":original,"revisions":ids,"legacy_deleted":false}),
        )
    }
    pub fn export_paged(&self, project: &str, revision: Option<&str>) -> Result<SemanticSnapshot> {
        let response = Client::new(&self.root)?
            .call(serde_json::json!({"op":"export","project":project,"revision":revision}))?;
        let path = response["path"]
            .as_str()
            .ok_or_else(|| Error::new("IpcProtocol", "export path"))?;
        crate::record_snapshot_decode();
        let result = serde_json::from_reader(std::fs::File::open(path)?).map_err(Into::into);
        let _ = std::fs::remove_file(path);
        result
    }
}
