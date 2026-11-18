//! HEAD is the sole commit decision. The index is immutable/reconstructible and
//! cannot grant access to a candidate, even after its database transaction commits.
use crate::facts::{
    objects::{Objects, valid_hash},
    tree,
    writer::{self, Revision},
};
use cq_core::{
    Error, Result,
    ports::facts::{
        CommitOutcome, CommitResult, FactBatch, FactReader, FactRef, FactRow, ReadStats,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
const LEASE_MS: u64 = 24 * 60 * 60 * 1000;
mod audit;
mod full_audit;
mod resources;
pub mod retention;
mod retention_plan;
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Error::new("CatalogBudget", "metadata exceeds 16 MiB"));
    }
    crate::atomic::create_directory(path.parent().unwrap())?;
    let tmp = path.with_extension(cq_capture_protocol::new_id()?);
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    if std::fs::metadata(path)?.len() > 16 * 1024 * 1024 {
        return Err(Error::new("CatalogBudget", "metadata exceeds 16 MiB"));
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    revision: String,
    epoch: String,
    expires: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    generation: u64,
    committed: BTreeMap<String, String>,
    /// Commit decisions survive retirement; these entries are not GC roots.
    #[serde(default)]
    published: BTreeMap<String, String>,
    leases: BTreeMap<String, Lease>,
    retiring: BTreeSet<String>,
    #[serde(default)]
    maintenance: BTreeMap<String, BTreeSet<String>>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Route {
    epoch: String,
    generation: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    manifest: Revision,
    expected: Option<String>,
}
pub struct StoreHost {
    pub root: PathBuf,
    pub objects: Arc<Objects>,
    project: String,
    publication: Mutex<()>,
    catalog: Mutex<Catalog>,
    epoch_active: Mutex<BTreeMap<String, usize>>,
    route: Mutex<Route>,
    active: Mutex<BTreeMap<String, usize>>,
    blocked: AtomicBool,
    _owner: File,
}
pub struct View {
    deadline: Mutex<Option<std::time::Instant>>,
    host: Arc<StoreHost>,
    revision: Arc<Revision>,
    epoch: String,
    lease: String,
    stats: Mutex<ReadStats>,
    active: bool,
    durable: bool,
}
struct Maintenance<'a> {
    host: &'a StoreHost,
    epoch: String,
}
impl Drop for Maintenance<'_> {
    fn drop(&mut self) {
        if let Ok(mut c) = self.host.catalog.lock()
            && c.maintenance.remove(&self.epoch).is_some()
        {
            c.generation += 1;
            let _ = save(&self.host.root.join("catalog.json"), &*c);
        }
    }
}
impl StoreHost {
    fn point(&self, stage: &str) -> Result<()> {
        #[cfg(debug_assertions)]
        {
            let hooks = self.root.join("test-hooks");
            if hooks.join(stage).is_file() {
                std::fs::write(hooks.join(format!("ready-{stage}")), b"ready")?;
                while !hooks.join(format!("release-{stage}")).is_file() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        let _ = stage;
        Ok(())
    }
    pub fn open(store: &Path, project: &str) -> Result<Arc<Self>> {
        let root = store
            .join("format-2/projects")
            .join(cq_core::model::digest(project.as_bytes()));
        crate::atomic::create_directory(&root)?;
        crate::atomic::sync_directory_chain(&root)?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("host.lock"))?;
        if unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::new(
                "HostOwnershipConflict",
                "connect to the existing store host",
            ));
        }
        let objects = Arc::new(Objects::new(store.join("format-2/objects"))?);
        let mut catalog: Catalog = if root.join("catalog.json").exists() {
            load(&root.join("catalog.json"))?
        } else {
            Catalog::default()
        };
        // Exclusive OS ownership proves no maintenance execution from the old
        // host remains alive. Committed roots and durable cursor leases remain.
        catalog.maintenance.clear();
        for (id, seal) in &catalog.committed {
            catalog
                .published
                .entry(id.clone())
                .or_insert_with(|| seal.clone());
        }
        let mut route: Route = if root.join("route.json").exists() {
            load(&root.join("route.json"))?
        } else {
            Route {
                epoch: cq_capture_protocol::new_id()?,
                generation: 0,
            }
        };
        let path = root.join("indexes").join(&route.epoch).join("graph.lbdb");
        let create = !root.join("route.json").exists();
        let index = match crate::ladybug::residency::acquire(
            &path,
            create,
            false,
            std::time::Duration::from_secs(5),
        ) {
            Ok(p) => p,
            Err(e) if !create && e.code() != "StoreBusy" => {
                // A missing/broken projection never changes the fact HEAD.
                let epoch = cq_capture_protocol::new_id()?;
                let projection = crate::ladybug::residency::acquire(
                    &root.join("indexes").join(&epoch).join("graph.lbdb"),
                    true,
                    false,
                    std::time::Duration::from_secs(5),
                )?;
                let catalog: &Catalog = &catalog;
                for id in catalog.committed.keys() {
                    let r: Revision = load(&root.join("commits").join(id))?;
                    r.verify()?;
                    projection.import(&objects, &r)?;
                }
                let pending_path = root.join("pending.json");
                if pending_path.exists() {
                    let p: Pending = load(&pending_path)?;
                    let head: Option<String> = if root.join("HEAD").exists() {
                        Some(load(&root.join("HEAD"))?)
                    } else {
                        None
                    };
                    if head.as_deref() == Some(&p.manifest.revision) {
                        projection.import(&objects, &p.manifest)?;
                    }
                }
                route = Route {
                    epoch,
                    generation: route.generation + 1,
                };
                projection
            }
            Err(e) => return Err(e),
        };
        drop(index);
        let host = Arc::new(Self {
            root,
            objects,
            project: project.into(),
            publication: Mutex::new(()),
            catalog: Mutex::new(catalog),
            epoch_active: Mutex::new(BTreeMap::new()),
            route: Mutex::new(route),
            active: Mutex::new(BTreeMap::new()),
            blocked: AtomicBool::new(true),
            _owner: owner,
        });
        save(&host.root.join("route.json"), &*host.route.lock().unwrap())?;
        host.recover()?;
        Ok(host)
    }
    pub fn head(&self) -> Result<Option<String>> {
        let p = self.root.join("HEAD");
        if p.exists() {
            Ok(Some(load(&p)?))
        } else {
            Ok(None)
        }
    }
    pub fn validate_lease(&self, id: &str, revision: &str) -> Result<()> {
        let c = self.catalog.lock().unwrap();
        let l = c
            .leases
            .get(id)
            .ok_or_else(|| Error::new("CursorExpired", "missing view lease"))?;
        if l.revision != revision || l.expires < cq_capture_protocol::now_ms() {
            return Err(Error::new("CursorExpired", "lease revision or expiration"));
        }
        Ok(())
    }
    pub fn release_lease(&self, id: &str) -> Result<()> {
        let mut c = self.catalog.lock().unwrap();
        c.leases.remove(id);
        c.generation += 1;
        save(&self.root.join("catalog.json"), &*c)
    }
    pub fn diagnostics(self: &Arc<Self>, revision: Option<&str>) -> Result<Value> {
        let view = self.open_view(revision)?;
        let row = view.scan("all:entity", None, 1)?.rows.into_iter().next();
        let profile = row
            .map(|r| {
                view.host
                    .index(view.epoch())?
                    .profile(&r.reference.object.hash)
            })
            .transpose()?;
        Ok(
            serde_json::json!({"revision":view.revision(),"epoch":view.epoch(),"physical_counts":view.host.index(view.epoch())?.physical_counts()?,"profile":profile,"residency":crate::ladybug::residency::diagnostics(),"scope":"physical shared projection; ordinary queries additionally require canonical revision membership"}),
        )
    }
    pub fn manifest(&self, id: &str) -> Result<Revision> {
        if !valid_hash(id) {
            return Err(Error::new("InvalidRevision", id));
        }
        let path = self.root.join("commits").join(id);
        if !path.is_file() {
            return Err(Error::new("RevisionUnavailable", id));
        }
        let r: Revision = load(&path)?;
        r.verify()?;
        if r.project != self.project || r.revision != id {
            return Err(Error::new("WrongProject", id));
        }
        Ok(r)
    }
    fn index(&self, epoch: &str) -> Result<crate::ladybug::residency::Lease> {
        crate::ladybug::residency::acquire(
            &self.root.join("indexes").join(epoch).join("graph.lbdb"),
            false,
            false,
            std::time::Duration::from_secs(5),
        )
    }
    fn audit_delta(&self, r: &Revision) -> Result<()> {
        let prior = r
            .parent
            .as_deref()
            .map(|id| self.manifest(id))
            .transpose()?;
        if r.sequence != prior.as_ref().map_or(1, |r| r.sequence + 1)
            || r.metadata["revision"] != r.revision
            || r.metadata["project"] != r.project
        {
            return Err(Error::new(
                "InvalidRevision",
                "sequence or metadata coordinates",
            ));
        }
        let base = prior.map(|r| r.roots.facts);
        if base.is_none() && r.changes.upserts == r.roots.facts && r.changes.deletes.is_none() {
            let mut count = 0;
            tree::visit(&self.objects, &r.roots.facts, |_, _| {
                count += 1;
                Ok(())
            })?;
            return if r.changes.facts_changed == count && r.changes.facts_deleted == 0 {
                Ok(())
            } else {
                Err(Error::new("InvalidDelta", "initial counts"))
            };
        }
        let mut changed = 0;
        let mut deleted = 0;
        tree::diff(
            &self.objects,
            &base.flatten(),
            &r.roots.facts,
            &mut ReadStats::default(),
            |key, a, b| {
                let (root, expected) = if let Some(value) = b {
                    changed += 1;
                    (&r.changes.upserts, value)
                } else {
                    deleted += 1;
                    (&r.changes.deletes, a.unwrap())
                };
                if tree::get(&self.objects, root, key, &mut ReadStats::default())?.as_ref()
                    != Some(expected)
                {
                    return Err(Error::new("InvalidDelta", "missing or mismatched change"));
                }
                Ok(())
            },
        )?;
        let mut upserts = 0;
        let mut deletes = 0;
        tree::visit(&self.objects, &r.changes.upserts, |_, _| {
            upserts += 1;
            Ok(())
        })?;
        tree::visit(&self.objects, &r.changes.deletes, |_, _| {
            deletes += 1;
            Ok(())
        })?;
        if changed != r.changes.facts_changed
            || deleted != r.changes.facts_deleted
            || changed != upserts
            || deleted != deletes
        {
            return Err(Error::new("InvalidDelta", "counts or extra rows"));
        }
        Ok(())
    }
    fn recover(&self) -> Result<()> {
        let head = self.head()?;
        {
            let c = self.catalog.lock().unwrap();
            if head.is_none() && !c.committed.is_empty() {
                self.blocked.store(true, Ordering::Release);
                return Err(Error::new(
                    "RecoveryRequired",
                    "HEAD missing for published history",
                ));
            }
            if let Some(id) = &head
                && !c.committed.contains_key(id)
                && !self.root.join("pending.json").exists()
            {
                self.blocked.store(true, Ordering::Release);
                return Err(Error::new(
                    "RecoveryRequired",
                    "HEAD is not admitted or pending",
                ));
            }
        }
        if self.blocked.load(Ordering::Acquire) {
            let disk: Route = load(&self.root.join("route.json"))?;
            let mut route = self.route.lock().unwrap();
            if disk.epoch != route.epoch {
                *route = disk;
            }
            save(&self.root.join("route.json"), &*route)?;
            let epoch = route.epoch.clone();
            drop(route);
            let ids = self
                .catalog
                .lock()
                .unwrap()
                .committed
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            for id in ids {
                let r = self.manifest(&id)?;
                if !self.index(&epoch)?.receipt(&r)? {
                    return Err(Error::new(
                        "RecoveryRequired",
                        "route lacks a retained version receipt",
                    ));
                }
            }
        }
        let p = self.root.join("pending.json");
        if p.exists() {
            let pending: Pending = load(&p)?;
            let r = &pending.manifest;
            r.verify()?;
            if r.parent != pending.expected || r.project != self.project {
                return Err(Error::new(
                    "RecoveryRequired",
                    "pending commit binding mismatch",
                ));
            }
            if self.head()?.as_deref() == Some(&r.revision) {
                self.audit(r)?;
                let epoch = self.route.lock().unwrap().epoch.clone();
                if !self.index(&epoch)?.receipt(r)? {
                    return Err(Error::new(
                        "RecoveryRequired",
                        "committed manifest lacks index receipt",
                    ));
                }
                save(&self.root.join("HEAD"), &r.revision)?;
                let mut c = self.catalog.lock().unwrap();
                c.committed.insert(r.revision.clone(), r.seal.clone());
                c.published.insert(r.revision.clone(), r.seal.clone());
                c.generation += 1;
                save(&self.root.join("catalog.json"), &*c)?;
            } else if self.head()? != pending.expected {
                return Err(Error::new(
                    "RecoveryRequired",
                    "pending commit does not match HEAD",
                ));
            }
            std::fs::remove_file(&p)?;
            File::open(&self.root)?.sync_all()?;
        }
        self.recover_retirement()?;
        self.blocked.store(false, Ordering::Release);
        Ok(())
    }
    pub fn publish(&self, r: &Revision, expected: Option<&str>) -> CommitResult {
        let mut state = CommitOutcome::NotCommitted;
        let started = std::time::Instant::now();
        let mut timing = serde_json::Map::new();
        let result = (|| {
            let _g = self.publication.lock().unwrap();
            self.recover()?;
            let _objects = retention::ObjectGate::lock(self.objects.root.parent().unwrap())?;
            if r.project != self.project {
                return Err(Error::new("WrongProject", &r.project));
            }
            if self.catalog.lock().unwrap().published.get(&r.revision) == Some(&r.seal) {
                state = CommitOutcome::Committed;
                return Ok(());
            }
            if self.head()?.as_deref() != expected || r.parent.as_deref() != expected {
                return Err(Error::new(
                    "PublishConflict",
                    "expected HEAD or parent differs",
                ));
            }
            if self
                .catalog
                .lock()
                .unwrap()
                .published
                .contains_key(&r.revision)
            {
                return Err(Error::new(
                    "RevisionConflict",
                    "same revision with different manifest",
                ));
            }
            let audit = std::time::Instant::now();
            let audit_stats = self.audit_publication(r)?;
            timing.insert("audit".into(), audit_stats);
            timing.insert("audit_ms".into(), json!(audit.elapsed().as_millis() as u64));
            crate::atomic::immutable_write(
                &self.root.join("commits").join(&r.revision),
                &serde_json::to_vec(r)?,
            )?;
            save(
                &self.root.join("pending.json"),
                &Pending {
                    manifest: r.clone(),
                    expected: expected.map(str::to_owned),
                },
            )?;
            let epoch = self.route.lock().unwrap().epoch.clone();
            let native = std::time::Instant::now();
            let projection = self.index(&epoch)?;
            let imported = projection.import_with(&self.objects, r, &|stage| self.point(stage));
            timing.insert(
                "native_import".into(),
                serde_json::to_value(projection.import_stats())?,
            );
            imported?;
            drop(projection);
            timing.insert(
                "native_import_ms".into(),
                json!(native.elapsed().as_millis() as u64),
            );
            self.point("after-index")?;
            let mut cat = self.catalog.lock().unwrap();
            self.blocked.store(true, Ordering::Release);
            state = CommitOutcome::Unknown;
            let decision = std::time::Instant::now();
            save(&self.root.join("HEAD"), &r.revision)?;
            self.point("after-head")?;
            cat.committed.insert(r.revision.clone(), r.seal.clone());
            cat.published.insert(r.revision.clone(), r.seal.clone());
            cat.generation += 1;
            save(&self.root.join("catalog.json"), &*cat)?;
            std::fs::remove_file(self.root.join("pending.json"))?;
            File::open(&self.root)?.sync_all()?;
            self.blocked.store(false, Ordering::Release);
            state = CommitOutcome::Committed;
            timing.insert(
                "head_and_activation_ms".into(),
                json!(decision.elapsed().as_millis() as u64),
            );
            Ok(())
        })();
        timing.insert(
            "total_ms".into(),
            json!(started.elapsed().as_millis() as u64),
        );
        timing.insert("outcome".into(), json!(state));
        // Diagnostics are outside the commit decision and manifest seal. A
        // missing diagnostic file must not turn a committed result into failure.
        if let Ok(id) = cq_capture_protocol::new_id() {
            let _ = save(
                &self
                    .root
                    .join("publication-traces")
                    .join(format!("{}-{id}.json", r.revision)),
                &timing,
            );
        }
        CommitResult {
            revision: r.revision.clone(),
            outcome: state,
            detail: result.err().map(|e: Error| e.to_string()),
        }
    }
    pub fn resolve(&self, revision: &str) -> Result<CommitResult> {
        let _g = self.publication.lock().unwrap();
        if let Err(e) = self.recover() {
            return Ok(CommitResult {
                revision: revision.into(),
                outcome: CommitOutcome::Unknown,
                detail: Some(e.to_string()),
            });
        }
        let committed = self
            .catalog
            .lock()
            .unwrap()
            .published
            .contains_key(revision);
        Ok(CommitResult {
            revision: revision.into(),
            outcome: if committed {
                CommitOutcome::Committed
            } else {
                CommitOutcome::NotCommitted
            },
            detail: None,
        })
    }
    pub fn open_view(self: &Arc<Self>, revision: Option<&str>) -> Result<View> {
        self.open_kind(revision, false)
    }
    pub fn open_session(self: &Arc<Self>, revision: Option<&str>) -> Result<View> {
        self.open_kind(revision, true)
    }
    fn open_kind(self: &Arc<Self>, revision: Option<&str>, session: bool) -> Result<View> {
        let mut c = self.catalog.lock().unwrap();
        if self.blocked.load(Ordering::Acquire) {
            return Err(Error::new(
                "RecoveryRequired",
                "publication outcome unknown",
            ));
        }
        let id = revision
            .map(str::to_owned)
            .or(self.head()?)
            .ok_or_else(|| Error::new("MissingRevision", "index first"))?;
        let r = self.manifest(&id)?;
        if c.retiring.contains(&id) || c.committed.get(&id) != Some(&r.seal) {
            return Err(Error::new("RevisionUnavailable", "uncommitted or retired"));
        }
        let epoch = self.route.lock().unwrap().epoch.clone();
        if !self.index(&epoch)?.receipt(&r)? {
            return Err(Error::new("IndexIncomplete", "missing version receipt"));
        }
        let lease = cq_capture_protocol::new_id()?;
        c.leases.insert(
            lease.clone(),
            Lease {
                revision: id.clone(),
                epoch: epoch.clone(),
                expires: cq_capture_protocol::now_ms() + LEASE_MS,
            },
        );
        c.generation += 1;
        save(&self.root.join("catalog.json"), &*c)?;
        if !session {
            *self.active.lock().unwrap().entry(id).or_default() += 1;
            *self
                .epoch_active
                .lock()
                .unwrap()
                .entry(epoch.clone())
                .or_default() += 1;
        }
        Ok(View {
            deadline: Mutex::new(None),
            host: self.clone(),
            revision: Arc::new(r),
            epoch: epoch.clone(),
            lease,
            stats: Mutex::new(ReadStats::default()),
            active: !session,
            durable: true,
        })
    }
    fn request_from(self: &Arc<Self>, session: &View) -> Result<View> {
        let c = self.catalog.lock().unwrap();
        let lease = c
            .leases
            .get(&session.lease)
            .ok_or_else(|| Error::new("CursorExpired", "session lease missing"))?;
        if lease.expires < cq_capture_protocol::now_ms()
            || lease.revision != session.revision.revision
        {
            return Err(Error::new("CursorExpired", "session lease expired"));
        }
        *self
            .active
            .lock()
            .unwrap()
            .entry(lease.revision.clone())
            .or_default() += 1;
        *self
            .epoch_active
            .lock()
            .unwrap()
            .entry(session.epoch.clone())
            .or_default() += 1;
        Ok(View {
            deadline: Mutex::new(None),
            host: self.clone(),
            revision: session.revision.clone(),
            epoch: session.epoch.clone(),
            lease: session.lease.clone(),
            stats: Mutex::new(ReadStats::default()),
            active: true,
            durable: false,
        })
    }
    pub fn rebuild(&self) -> Result<String> {
        self.collect_epochs()?;
        let publication = self.publication.lock().unwrap();
        self.recover()?;
        if !self.catalog.lock().unwrap().maintenance.is_empty() {
            return Err(Error::new(
                "StoreBusy",
                "epoch budget exhausted; finish or release old maintenance/views",
            ));
        }
        let epoch = cq_capture_protocol::new_id()?;
        let current_epoch = self.route.lock().unwrap().epoch.clone();
        let reservation = resources::reserve(&self.root, &current_epoch)?;
        let projection = crate::ladybug::residency::acquire_limited(
            &self.root.join("indexes").join(&epoch).join("graph.lbdb"),
            true,
            true,
            std::time::Duration::from_secs(5),
            reservation.bytes,
        )?;
        let mut ids = self
            .catalog
            .lock()
            .unwrap()
            .committed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort_by_key(|id| self.manifest(id).map(|r| r.sequence).unwrap_or(u64::MAX));
        {
            let mut c = self.catalog.lock().unwrap();
            c.maintenance
                .insert(epoch.clone(), ids.iter().cloned().collect());
            c.generation += 1;
            save(&self.root.join("catalog.json"), &*c)?;
        }
        let _maintenance = Maintenance {
            host: self,
            epoch: epoch.clone(),
        };
        let generation = self.route.lock().unwrap().generation;
        drop(publication);
        self.point("rebuild-snapshot")?;
        for id in ids {
            let r = self.manifest(&id)?;
            if r.parent
                .as_ref()
                .is_some_and(|id| self.catalog.lock().unwrap().committed.contains_key(id))
            {
                self.audit_publication(&r)?;
            } else {
                self.audit(&r)?;
            }
            projection.import_with(&self.objects, &r, &|stage| self.point(stage))?;
            if !projection.receipt(&r)? {
                return Err(Error::new("IndexIncomplete", id));
            }
        }
        let _publication = self.publication.lock().unwrap();
        self.recover()?;
        if self.route.lock().unwrap().generation != generation {
            return Err(Error::new(
                "EpochConflict",
                "another maintenance operation switched first",
            ));
        }
        let mut current = self
            .catalog
            .lock()
            .unwrap()
            .committed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        current.sort_by_key(|id| self.manifest(id).map(|r| r.sequence).unwrap_or(u64::MAX));
        for id in current {
            let r = self.manifest(&id)?;
            if !projection.receipt(&r)? {
                if r.parent
                    .as_ref()
                    .is_some_and(|id| self.catalog.lock().unwrap().committed.contains_key(id))
                {
                    self.audit_publication(&r)?;
                } else {
                    self.audit(&r)?;
                }
                projection.import_with(&self.objects, &r, &|stage| self.point(stage))?;
            }
        }
        let mut cat = self.catalog.lock().unwrap();
        let mut route = self.route.lock().unwrap();
        self.point("rebuild-before-route")?;
        let updated = Route {
            epoch: epoch.clone(),
            generation: route.generation + 1,
        };
        self.blocked.store(true, Ordering::Release);
        save(&self.root.join("route.json"), &updated)?;
        *route = updated;
        self.point("rebuild-after-route")?;
        cat.generation += 1;
        cat.maintenance.remove(&epoch);
        save(&self.root.join("catalog.json"), &*cat)?;
        self.blocked.store(false, Ordering::Release);
        Ok(epoch)
    }
    pub fn compact(self: &Arc<Self>) -> Result<Value> {
        let head_before = self.head()?;
        let before = self
            .index(&self.route.lock().unwrap().epoch.clone())?
            .physical_counts()?;
        let epoch = self.rebuild()?;
        let after = self.index(&epoch)?.physical_counts()?;
        let removed = self.collect_epochs()?;
        Ok(
            json!({"epoch":epoch,"head_before":head_before,"head_after":self.head()?,"physical_before":before,"physical_after":after,"collected_epochs":removed,"canonical_facts_deleted":false,"raw_evidence_deleted":false,"residency":crate::ladybug::residency::diagnostics()}),
        )
    }
}
impl Drop for View {
    fn drop(&mut self) {
        if self.durable
            && let Ok(mut c) = self.host.catalog.lock()
        {
            c.leases.remove(&self.lease);
            c.generation += 1;
            let _ = save(&self.host.root.join("catalog.json"), &*c);
        }
        let mut a = self.host.active.lock().unwrap();
        if self.active
            && let Some(n) = a.get_mut(&self.revision.revision)
        {
            *n = n.saturating_sub(1);
        }
        if self.active
            && let Some(n) = self.host.epoch_active.lock().unwrap().get_mut(&self.epoch)
        {
            *n = n.saturating_sub(1);
        }
    }
}
impl View {
    pub(crate) fn renew_session_lease(&self) -> Result<()> {
        if !self.is_session() {
            return Ok(());
        }
        let mut c = self.host.catalog.lock().unwrap();
        let lease = c
            .leases
            .get_mut(&self.lease)
            .ok_or_else(|| Error::new("CursorExpired", "missing session lease"))?;
        let now = cq_capture_protocol::now_ms();
        if lease.revision != self.revision.revision
            || lease.epoch != self.epoch
            || lease.expires < now
        {
            return Err(Error::new(
                "CursorExpired",
                "session lease expired or revoked",
            ));
        }
        if lease.expires.saturating_sub(now) <= LEASE_MS / 2 {
            lease.expires = now + LEASE_MS;
            c.generation += 1;
            save(&self.host.root.join("catalog.json"), &*c)?;
        }
        Ok(())
    }
    fn remaining(&self) -> Result<std::time::Duration> {
        match *self.deadline.lock().unwrap() {
            Some(d) => {
                let left = d.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    Err(Error::new("QueryTimeout", "request budget exhausted"))
                } else {
                    Ok(left)
                }
            }
            None => Ok(std::time::Duration::from_secs(5)),
        }
    }
    fn index(&self) -> Result<crate::ladybug::residency::Lease> {
        crate::ladybug::residency::acquire(
            &self
                .host
                .root
                .join("indexes")
                .join(&self.epoch)
                .join("graph.lbdb"),
            false,
            false,
            self.remaining()?,
        )
    }
    pub fn is_session(&self) -> bool {
        !self.active
    }
    pub fn request(&self) -> Result<View> {
        self.host.request_from(self)
    }
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    pub fn manifest(&self) -> &Revision {
        &self.revision
    }
    pub fn lease(&self) -> &str {
        &self.lease
    }
    fn row(&self, key: String, r: FactRef) -> Result<FactRow> {
        let mut io = ReadStats::default();
        let (bytes, queried) = self.index()?.page(
            &self.host.objects,
            &r.object,
            self.remaining()?.as_millis().max(1) as u64,
            &mut io,
        )?;
        {
            let mut stats = self.stats.lock().unwrap();
            stats.page_reads += io.page_reads;
            stats.bytes_decoded += io.bytes_decoded;
        }
        if queried > 0 {
            let mut stats = self.stats.lock().unwrap();
            stats.database_queries += queried;
        }
        let end = r
            .offset
            .checked_add(r.length)
            .ok_or_else(|| Error::new("InvalidFactRef", "overflow"))?;
        let b = bytes
            .get(r.offset as usize..end as usize)
            .ok_or_else(|| Error::new("IndexIncomplete", "record bounds"))?;
        if cq_core::model::digest(b) != r.digest {
            return Err(Error::new("IndexIncomplete", "record digest"));
        }
        let mut s = self.stats.lock().unwrap();
        s.provider_rows += 1;
        s.bytes_decoded += r.length;
        Ok(FactRow {
            key,
            reference: r,
            value: serde_json::from_slice(b)?,
        })
    }
}

impl FactReader for View {
    fn set_timeout(&self, timeout_ms: u64) {
        *self.deadline.lock().unwrap() =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms));
    }
    fn request_view(&self) -> Result<Option<Box<dyn FactReader>>> {
        if self.is_session() {
            Ok(Some(Box::new(self.request()?)))
        } else {
            Ok(None)
        }
    }
    fn revision(&self) -> &str {
        &self.revision.revision
    }
    fn project(&self) -> &str {
        &self.revision.project
    }
    fn metadata(&self) -> &Value {
        &self.revision.metadata
    }
    fn get(&self, family: &str, id: &str) -> Result<Option<FactRow>> {
        if self.is_session() {
            return self.request()?.get(family, id);
        }
        let r = writer::reference(
            &self.host.objects,
            &self.revision.roots,
            family,
            id,
            &mut self.stats.lock().unwrap(),
        )?;
        r.map(|r| self.row(cq_core::ports::facts::fact_key(family, id), r))
            .transpose()
    }
    fn scan(&self, group: &str, after: Option<&str>, limit: usize) -> Result<FactBatch> {
        if self.is_session() {
            return self.request()?.scan(group, after, limit);
        }
        let group_name = group;
        let group: writer::Group = tree::get(
            &self.host.objects,
            &self.revision.roots.groups,
            group,
            &mut self.stats.lock().unwrap(),
        )?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
        let p = tree::scan(
            &self.host.objects,
            &group.root,
            after,
            limit,
            &mut self.stats.lock().unwrap(),
        )?;
        let mut rows = vec![];
        for (_, key, value) in p.rows {
            let r: FactRef = serde_json::from_value(value)?;
            // A corrupted secondary directory cannot attach a foreign fact.
            let current = tree::get(
                &self.host.objects,
                &self.revision.roots.facts,
                &key,
                &mut self.stats.lock().unwrap(),
            )?;
            if current != Some(serde_json::to_value(&r)?) {
                return Err(Error::new(
                    "IndexIncomplete",
                    "secondary membership mismatch",
                ));
            }
            let row = self.row(key, r)?;
            let family = row
                .key
                .split(':')
                .next()
                .ok_or_else(|| Error::new("InvalidFactKey", &row.key))?;
            if !cq_core::ports::facts::groups(family, &row.value)
                .iter()
                .any(|g| g == group_name)
            {
                return Err(Error::new(
                    "IndexIncomplete",
                    "directory scope does not match record",
                ));
            }
            rows.push(row);
        }
        Ok(FactBatch {
            rows,
            after: p.after,
            complete: p.complete,
        })
    }
    fn count(&self, group: &str) -> Result<u64> {
        if self.is_session() {
            return self.request()?.count(group);
        }
        let g: writer::Group = tree::get(
            &self.host.objects,
            &self.revision.roots.groups,
            group,
            &mut self.stats.lock().unwrap(),
        )?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
        Ok(g.count)
    }
    fn source_range(&self, file: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        if self.is_session() {
            return self.request()?.source_range(file, start, end);
        }
        let v = tree::get(
            &self.host.objects,
            &self.revision.roots.sources,
            file,
            &mut self.stats.lock().unwrap(),
        )?
        .ok_or_else(|| Error::new("MissingEvidence", file))?;
        let src: crate::facts::source::Source = serde_json::from_value(v)?;
        src.range(
            &self.host.objects,
            start,
            end,
            &mut self.stats.lock().unwrap(),
        )
    }
    fn stats(&self) -> ReadStats {
        self.stats.lock().unwrap().clone()
    }
}

impl Drop for StoreHost {
    fn drop(&mut self) {
        crate::ladybug::residency::evict_below(&self.root.join("indexes"));
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn session_renewal_preserves_version_and_does_not_revive_revoked_or_expired_leases() {
        let dir = tempfile::tempdir().unwrap();
        let host = StoreHost::open(dir.path(), "renewal").unwrap();
        let id = cq_core::model::digest(b"renewal-version");
        let mut revision = Revision {
            format: 2,
            project: "renewal".into(),
            revision: id.clone(),
            parent: None,
            sequence: 1,
            roots: Default::default(),
            metadata: json!({"project":"renewal","revision":id}),
            changes: Default::default(),
            seal: String::new(),
        };
        revision.seal().unwrap();
        assert_eq!(
            host.publish(&revision, None).outcome,
            CommitOutcome::Committed
        );
        let session = host.open_session(None).unwrap();
        {
            let mut c = host.catalog.lock().unwrap();
            c.leases.get_mut(session.lease()).unwrap().expires =
                cq_capture_protocol::now_ms() + 1000;
        }
        session.renew_session_lease().unwrap();
        {
            let c = host.catalog.lock().unwrap();
            let lease = &c.leases[session.lease()];
            assert_eq!(lease.revision, id);
            assert_eq!(lease.epoch, session.epoch());
            assert!(lease.expires > cq_capture_protocol::now_ms() + LEASE_MS / 2);
            let persisted: Catalog = load(&host.root.join("catalog.json")).unwrap();
            assert_eq!(persisted.leases[session.lease()].expires, lease.expires);
        }
        host.catalog
            .lock()
            .unwrap()
            .leases
            .get_mut(session.lease())
            .unwrap()
            .expires = 0;
        assert_eq!(
            session.renew_session_lease().unwrap_err().code(),
            "CursorExpired"
        );
        assert_eq!(
            host.catalog.lock().unwrap().leases[session.lease()].expires,
            0
        );
        host.release_lease(session.lease()).unwrap();
        assert_eq!(
            session.renew_session_lease().unwrap_err().code(),
            "CursorExpired"
        );
        assert!(
            !host
                .catalog
                .lock()
                .unwrap()
                .leases
                .contains_key(session.lease())
        );
    }
}
