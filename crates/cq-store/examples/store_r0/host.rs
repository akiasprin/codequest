//! Minimal publication/catalog coordinator for R0. No product API is changed.
use crate::{
    model::*,
    projection::{Projection, integer, quote, string},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub lease: String,
    pub revision: String,
    pub sequence: i64,
    pub context: String,
    pub epoch: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Lease {
    view: View,
    expires: u64,
    active: usize,
}
#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    generation: u64,
    now: u64,
    serial: u64,
    leases: BTreeMap<String, Lease>,
    retiring: BTreeSet<String>,
    reclaimed: BTreeSet<String>,
    staging: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    maintenance: BTreeMap<String, BTreeSet<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcPlan {
    pub generation: u64,
    pub revisions: Vec<String>,
    pub objects: Vec<String>,
}
pub struct Host {
    pub root: PathBuf,
    publication: Mutex<()>,
    catalog: Mutex<Catalog>,
    databases: Mutex<BTreeMap<String, Arc<Projection>>>,
    recovery_required: AtomicBool,
    // Kept until all database objects have been dropped.
    _ownership: File,
}
pub struct Read {
    host: Arc<Host>,
    view: View,
    pub projection: Arc<Projection>,
}
impl Drop for Read {
    fn drop(&mut self) {
        if let Ok(mut cat) = self.host.catalog.lock() {
            if let Some(lease) = cat.leases.get_mut(&self.view.lease) {
                lease.active = lease.active.saturating_sub(1);
            }
            cat.generation += 1;
            // Execution pins are process-local. Durable cursor leases survive
            // restart; executions cannot survive acquisition of a new host lock.
        }
    }
}
pub fn pause(stage: &str, asked: Option<&str>) {
    if asked == Some(stage) {
        println!("R0_READY {stage}");
        std::io::stdout().flush().expect("flush crash barrier");
        loop {
            std::thread::park();
        }
    }
}
impl Host {
    pub fn create(root: &Path) -> Result<Arc<Self>> {
        require(!root.exists(), "R0 requires a fresh owned directory")?;
        fs::create_dir_all(root)?;
        for d in ["facts", "evidence", "revisions", "candidates", "epochs"] {
            fs::create_dir_all(root.join(d))?;
        }
        save(
            &root.join("route.json"),
            &Route {
                epoch: "e1".into(),
                generation: 0,
            },
        )?;
        save(&root.join("catalog.json"), &Catalog::default())?;
        Self::open_inner(root, true)
    }
    pub fn open(root: &Path) -> Result<Arc<Self>> {
        let host = Self::open_inner(root, false)?;
        host.recover(None)?;
        Ok(host)
    }
    pub fn open_unrecovered(root: &Path) -> Result<Arc<Self>> {
        Self::open_inner(root, false)
    }
    fn open_inner(root: &Path, create: bool) -> Result<Arc<Self>> {
        let ownership = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("host.lock"))?;
        require(
            unsafe { libc::flock(ownership.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "HostOwnershipConflict",
        )?;
        let mut catalog: Catalog = load(&root.join("catalog.json"))?;
        // Holding the OS ownership lock proves that executions in the old host ended.
        // Persistent cursor leases and their expirations are retained.
        for lease in catalog.leases.values_mut() {
            lease.active = 0;
        }
        catalog.generation += 1;
        save(&root.join("catalog.json"), &catalog)?;
        let host = Arc::new(Self {
            root: root.into(),
            publication: Mutex::new(()),
            catalog: Mutex::new(catalog),
            databases: Mutex::new(BTreeMap::new()),
            recovery_required: AtomicBool::new(true),
            _ownership: ownership,
        });
        let route = host.route()?;
        let p = Arc::new(Projection::open(&host.epoch_path(&route.epoch), create)?);
        host.databases
            .lock()
            .map_err(|_| "database map poisoned")?
            .insert(route.epoch, p);
        Ok(host)
    }
    pub fn epoch_path(&self, epoch: &str) -> PathBuf {
        self.root.join("epochs").join(epoch).join("graph.lbdb")
    }
    pub fn route(&self) -> Result<Route> {
        load(&self.root.join("route.json"))
    }
    pub fn projection(&self, epoch: &str) -> Result<Arc<Projection>> {
        let mut map = self.databases.lock().map_err(|_| "database map poisoned")?;
        if let Some(p) = map.get(epoch) {
            return Ok(p.clone());
        }
        let p = Arc::new(Projection::open(&self.epoch_path(epoch), false)?);
        map.insert(epoch.into(), p.clone());
        Ok(p)
    }
    pub fn current_projection(&self) -> Result<Arc<Projection>> {
        self.projection(&self.route()?.epoch)
    }
    fn save_catalog(&self, cat: &Catalog) -> Result<()> {
        save(&self.root.join("catalog.json"), cat)
    }
    pub fn head(&self) -> Result<Option<String>> {
        let path = self.root.join("HEAD");
        if path.exists() {
            Ok(Some(load(&path)?))
        } else {
            Ok(None)
        }
    }
    pub fn manifest(&self, revision: &str) -> Result<Manifest> {
        let manifest: Manifest =
            load(&self.root.join("revisions").join(format!("{revision}.json")))?;
        require(manifest.revision()? == revision, "CorruptManifest")?;
        Ok(manifest)
    }
    fn history(&self) -> Result<Vec<(String, Manifest)>> {
        let mut next = self.head()?;
        let mut out = vec![];
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            require(seen.insert(id.clone()), "revision cycle")?;
            let m = self.manifest(&id)?;
            next = m.parent.clone();
            out.push((id, m));
        }
        out.reverse();
        Ok(out)
    }
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = self.root.join(key);
        if path.exists() {
            require(fs::read(path)? == bytes, "ContentCollision")
        } else {
            durable(&path, bytes)
        }
    }
    pub fn payload(&self, reference: &PayloadRef) -> Result<Payload> {
        let bytes = fs::read(self.root.join("facts").join(&reference.hash))?;
        require(digest(&bytes) == reference.hash, "CorruptFactPage")?;
        let p: Payload = serde_json::from_slice(&bytes)?;
        require(
            p.source == reference.source
                && p.context == reference.context
                && p.nodes.len() == reference.nodes
                && p.edges.len() == reference.edges,
            "InvalidPayloadRef",
        )?;
        require(
            bytes.len() == reference.fact_bytes
                && adjacency_roots(&reference.hash, &p)? == reference.adjacency_roots,
            "InvalidPayloadSeal",
        )?;
        Ok(p)
    }
    pub fn audit_manifest(&self, manifest: &Manifest) -> Result<()> {
        for (shard, reference) in &manifest.shards {
            let payload = self.payload(reference)?;
            require(payload.shard == *shard, "ShardOwnershipMismatch")?;
            let source = fs::read(self.root.join("evidence").join(&reference.source))?;
            require(
                source.len() == reference.source_bytes && digest(&source) == reference.source,
                "CorruptEvidence",
            )?;
        }
        Ok(())
    }
    pub fn prepare(
        &self,
        job: &str,
        parent: Option<String>,
        payloads: &[Payload],
        sources: &[Vec<u8>],
    ) -> Result<Candidate> {
        let mut refs = BTreeMap::new();
        let mut blobs = BTreeMap::new();
        for source in sources {
            blobs.insert(format!("evidence/{}", digest(source)), source.clone());
        }
        for p in payloads {
            let bytes = serde_json::to_vec(p)?;
            let hash = digest(&bytes);
            require(!refs.contains_key(&p.shard), "duplicate shard")?;
            let key = format!("evidence/{}", p.source);
            if let std::collections::btree_map::Entry::Vacant(entry) = blobs.entry(key.clone()) {
                let source = fs::read(self.root.join("evidence").join(&p.source))?;
                require(digest(&source) == p.source, "CorruptEvidence")?;
                entry.insert(source);
            }
            refs.insert(
                p.shard.clone(),
                PayloadRef {
                    hash: hash.clone(),
                    source: p.source.clone(),
                    context: p.context.clone(),
                    nodes: p.nodes.len(),
                    edges: p.edges.len(),
                    fact_bytes: bytes.len(),
                    source_bytes: blobs[&key].len(),
                    adjacency_roots: adjacency_roots(&hash, p)?,
                },
            );
            blobs.insert(format!("facts/{hash}"), bytes);
        }
        {
            let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
            require(!cat.staging.contains_key(job), "duplicate active job")?;
            cat.staging
                .insert(job.into(), blobs.keys().cloned().collect());
            cat.generation += 1;
            self.save_catalog(&cat)?;
        }
        for (key, bytes) in blobs {
            self.put(&key, &bytes)?;
        }
        let sequence = parent
            .as_ref()
            .map(|id| self.manifest(id).map(|m| m.sequence + 1))
            .transpose()?
            .unwrap_or(1);
        let manifest = Manifest {
            sequence,
            parent,
            shards: refs,
        };
        let revision = manifest.revision()?;
        let mut candidate = Candidate {
            job: job.into(),
            revision: revision.clone(),
            manifest,
            seal: String::new(),
        };
        candidate.seal = candidate.seal()?;
        save(
            &self.root.join("revisions").join(format!("{revision}.json")),
            &candidate.manifest,
        )?;
        save(
            &self.root.join("candidates").join(format!("{job}.json")),
            &candidate,
        )?;
        self.audit_manifest(&candidate.manifest)?;
        Ok(candidate)
    }
    fn changes(
        &self,
        p: &Projection,
        c: &lbug::Connection<'_>,
        candidate: &Candidate,
    ) -> Result<Vec<Change>> {
        let old = candidate
            .manifest
            .parent
            .as_ref()
            .map(|id| self.manifest(id))
            .transpose()?;
        self.changes_against(p, c, candidate, old.as_ref())
    }
    fn changes_against(
        &self,
        p: &Projection,
        c: &lbug::Connection<'_>,
        candidate: &Candidate,
        old: Option<&Manifest>,
    ) -> Result<Vec<Change>> {
        let previous = old.as_ref().map(|m| m.shards.clone()).unwrap_or_default();
        let keys: BTreeSet<_> = previous
            .keys()
            .chain(candidate.manifest.shards.keys())
            .cloned()
            .collect();
        let mut changes = vec![];
        for shard in keys {
            let before = previous.get(&shard);
            let after = candidate.manifest.shards.get(&shard);
            if before == after {
                continue;
            }
            let old_use = if before.is_some() {
                let seq = old.ok_or("missing old manifest")?.sequence;
                let rows=p.rows(c,&format!("MATCH (u:ShardUse) WHERE u.shard={} AND u.born <= {seq} AND u.retired > {seq} RETURN u.id",quote(&shard)))?;
                require(rows.len() == 1, "old shard interval missing or duplicated")?;
                Some(string(&rows[0][0])?)
            } else {
                None
            };
            changes.push(Change {
                shard,
                old_use,
                new_payload: after.map(|r| r.hash.clone()),
            });
        }
        Ok(changes)
    }
    fn apply(
        &self,
        p: &Projection,
        c: &lbug::Connection<'_>,
        candidate: &Candidate,
        changes: &[Change],
    ) -> Result<ImportStats> {
        let mut stats = ImportStats::default();
        let seq = candidate.manifest.sequence;
        let declarations = changes
            .iter()
            .filter(|change| change.new_payload.is_some())
            .map(|change| self.payload(&candidate.manifest.shards[&change.shard]))
            .collect::<Result<Vec<_>>>()?;
        p.ensure_slots(c, &declarations)?;
        for change in changes {
            if let Some(old) = &change.old_use {
                p.exec(
                    c,
                    &format!(
                        "MATCH (u:ShardUse {{id:{}}}) SET u.retired={seq}",
                        quote(old)
                    ),
                )?;
                stats.uses_closed += 1;
            }
            if let Some(hash) = &change.new_payload {
                let r = candidate
                    .manifest
                    .shards
                    .get(&change.shard)
                    .ok_or("missing new shard")?;
                p.import(c, hash, &self.payload(r)?, &mut stats)?;
                let id = format!("{}:{}", candidate.revision, change.shard);
                p.exec(
                    c,
                    &format!(
                        "CREATE (:ShardUse {{id:{},shard:{},born:{seq},retired:{INFINITY}}})",
                        quote(&id),
                        quote(&change.shard)
                    ),
                )?;
                p.exec(c,&format!("MATCH (u:ShardUse {{id:{}}}),(p:Payload {{id:{}}}) CREATE (u)-[:USES]->(p)",quote(&id),quote(hash)))?;
                stats.uses_started += 1;
            }
        }
        p.check_endpoints(c, seq)?;
        p.exec(
            c,
            &format!(
                "CREATE (:Receipt {{id:{},sequence:{seq},manifest:{}}})",
                quote(&candidate.revision),
                quote(&candidate.revision)
            ),
        )?;
        Ok(stats)
    }
    pub fn publish(
        &self,
        candidate: &Candidate,
        stop: Option<&str>,
    ) -> std::result::Result<ImportStats, PublicationFailure> {
        let outcome = Cell::new(CommitState::NotCommitted);
        self.publish_inner(candidate, stop, &outcome).map_err(|e| {
            if outcome.get() == CommitState::Unknown {
                self.recovery_required.store(true, Ordering::SeqCst);
            }
            PublicationFailure {
                outcome: outcome.get(),
                revision: candidate.revision.clone(),
                message: e.to_string(),
            }
        })
    }
    fn publish_inner(
        &self,
        candidate: &Candidate,
        stop: Option<&str>,
        outcome: &Cell<CommitState>,
    ) -> Result<ImportStats> {
        let _write = self
            .publication
            .lock()
            .map_err(|_| "publication poisoned")?;
        self.recover_inner(None)?;
        // Validate the content binding before the committed/idempotent shortcut.
        require(
            candidate.manifest.revision()? == candidate.revision,
            "CandidateMismatch",
        )?;
        require(candidate.seal == candidate.seal()?, "CandidateSealMismatch")?;
        if self
            .history()?
            .iter()
            .any(|(id, _)| id == &candidate.revision)
        {
            outcome.set(CommitState::Committed);
            self.release_staging(&candidate.job)?;
            return Ok(ImportStats::default());
        }
        require(self.head()? == candidate.manifest.parent, "PublishConflict")?;
        self.audit_manifest(&candidate.manifest)?;
        pause("after-files", stop);
        let p = self.current_projection()?;
        let c = p.connection()?;
        let changes = self.changes(&p, &c, candidate)?;
        let pending = Pending {
            candidate: candidate.clone(),
            changes,
        };
        p.exec(&c, "BEGIN TRANSACTION")?;
        let result = (|| {
            let stats = self.apply(&p, &c, candidate, &pending.changes)?;
            p.exec(
                &c,
                &format!(
                    "CREATE (:Pending {{id:{},json:{}}})",
                    quote(&candidate.revision),
                    quote(&serde_json::to_string(&pending)?)
                ),
            )?;
            pause("before-db-commit", stop);
            outcome.set(CommitState::Unknown);
            p.exec(&c, "COMMIT")?;
            Ok(stats)
        })();
        let stats: ImportStats = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = p.exec(&c, "ROLLBACK");
                return Err(e);
            }
        };
        pause("after-db-commit", stop);
        {
            let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
            let activation = (|| -> Result<()> {
                self.write_head(&candidate.revision, stop)?;
                outcome.set(CommitState::Committed);
                cat.staging.remove(&candidate.job);
                cat.generation += 1;
                self.save_catalog(&cat)
            })();
            if let Err(e) = activation {
                // Set this while the catalog gate is still held: no reader may
                // enter between failed durability confirmation and blocking.
                self.recovery_required.store(true, Ordering::SeqCst);
                return Err(e);
            }
        }
        pause("after-head", stop);
        p.exec(
            &c,
            &format!(
                "MATCH (n:Pending {{id:{}}}) DELETE n",
                quote(&candidate.revision)
            ),
        )?;
        Ok(stats)
    }
    fn write_head(&self, id: &str, stop: Option<&str>) -> Result<()> {
        let tmp = self.root.join("HEAD.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(id)?)?;
        f.sync_all()?;
        fs::rename(tmp, self.root.join("HEAD"))?;
        pause("after-head-rename", stop);
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn recover(&self, stop: Option<&str>) -> Result<()> {
        let _write = self
            .publication
            .lock()
            .map_err(|_| "publication poisoned")?;
        self.recover_inner(stop)
    }
    fn recover_inner(&self, stop: Option<&str>) -> Result<()> {
        let needs_confirmation = self.recovery_required.swap(true, Ordering::SeqCst);
        let p = self.current_projection()?;
        let c = p.connection()?;
        for row in p.rows(&c, "MATCH (n:Pending) RETURN n.json")? {
            let pending: Pending = serde_json::from_str(&string(&row[0])?)?;
            require(
                pending.candidate.manifest.revision()? == pending.candidate.revision
                    && pending.candidate.seal == pending.candidate.seal()?,
                "CorruptRecoveryRecord",
            )?;
            let committed = self
                .history()?
                .iter()
                .any(|(id, _)| id == &pending.candidate.revision);
            if committed {
                self.audit_manifest(&pending.candidate.manifest)?;
                File::open(self.root.join("HEAD"))?.sync_all()?;
                File::open(&self.root)?.sync_all()?;
            }
            p.exec(&c, "BEGIN TRANSACTION")?;
            if !committed {
                for change in &pending.changes {
                    let new = format!("{}:{}", pending.candidate.revision, change.shard);
                    p.exec(
                        &c,
                        &format!("MATCH (u:ShardUse {{id:{}}}) DETACH DELETE u", quote(&new)),
                    )?;
                    if let Some(old) = &change.old_use {
                        p.exec(
                            &c,
                            &format!(
                                "MATCH (u:ShardUse {{id:{}}}) SET u.retired={INFINITY}",
                                quote(old)
                            ),
                        )?;
                    }
                }
                p.exec(
                    &c,
                    &format!(
                        "MATCH (r:Receipt {{id:{}}}) DELETE r",
                        quote(&pending.candidate.revision)
                    ),
                )?;
            }
            p.exec(
                &c,
                &format!(
                    "MATCH (n:Pending {{id:{}}}) DELETE n",
                    quote(&pending.candidate.revision)
                ),
            )?;
            pause("before-recovery-commit", stop);
            p.exec(&c, "COMMIT")?;
            self.release_staging(&pending.candidate.job)?;
        }
        // Also confirm an epoch route whose previous response was uncertain.
        if needs_confirmation {
            File::open(self.root.join("route.json"))?.sync_all()?;
            File::open(&self.root)?.sync_all()?;
        }
        let _cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        self.recovery_required.store(false, Ordering::SeqCst);
        Ok(())
    }
    pub fn resolve_commit(&self, candidate: &Candidate) -> CommitResolution {
        let resolved = (|| -> Result<CommitState> {
            require(
                candidate.manifest.revision()? == candidate.revision
                    && candidate.seal == candidate.seal()?,
                "CandidateMismatch",
            )?;
            self.recover(None)?;
            Ok(
                if self
                    .history()?
                    .iter()
                    .any(|(id, _)| id == &candidate.revision)
                {
                    CommitState::Committed
                } else {
                    CommitState::NotCommitted
                },
            )
        })();
        match resolved {
            Ok(outcome) => CommitResolution {
                outcome,
                revision: candidate.revision.clone(),
                error: None,
            },
            Err(e) => CommitResolution {
                outcome: CommitState::Unknown,
                revision: candidate.revision.clone(),
                error: Some(e.to_string()),
            },
        }
    }
    fn release_staging(&self, job: &str) -> Result<()> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        cat.staging.remove(job);
        cat.generation += 1;
        self.save_catalog(&cat)
    }
    pub fn open_view(&self, revision: Option<&str>, context: &str, ttl: u64) -> Result<View> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        require(
            !self.recovery_required.load(Ordering::SeqCst),
            "RecoveryRequired",
        )?;
        let id = revision
            .map(str::to_owned)
            .or(self.head()?)
            .ok_or("MissingRevision")?;
        require(
            !cat.retiring.contains(&id) && !cat.reclaimed.contains(&id),
            "RetiringOrReclaimed",
        )?;
        require(
            self.history()?.iter().any(|(r, _)| r == &id),
            "UncommittedRevision",
        )?;
        let manifest = self.manifest(&id)?;
        require(
            manifest.shards.values().any(|r| r.context == context),
            "UnknownContext",
        )?;
        let route = self.route()?;
        let p = self.projection(&route.epoch)?;
        let c = p.connection()?;
        require(
            p.count(
                &c,
                &format!(
                    "MATCH (r:Receipt {{id:{}}}) WHERE r.manifest={} RETURN count(*)",
                    quote(&id),
                    quote(&id)
                ),
            )? == 1,
            "IndexCoverageMissing",
        )?;
        cat.serial += 1;
        let view = View {
            lease: format!("lease-{}", cat.serial),
            revision: id,
            sequence: manifest.sequence,
            context: context.into(),
            epoch: route.epoch,
        };
        let expires = cat.now + ttl;
        cat.leases.insert(
            view.lease.clone(),
            Lease {
                view: view.clone(),
                expires,
                active: 0,
            },
        );
        cat.generation += 1;
        self.save_catalog(&cat)?;
        Ok(view)
    }
    pub fn start_read(self: &Arc<Self>, view: &View) -> Result<Read> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        let now = cat.now;
        let lease = cat.leases.get_mut(&view.lease).ok_or("MissingLease")?;
        require(lease.expires > now, "LeaseExpired")?;
        require(
            lease.view.revision == view.revision
                && lease.view.sequence == view.sequence
                && lease.view.epoch == view.epoch
                && lease.view.context == view.context,
            "LeaseScopeMismatch",
        )?;
        lease.active += 1;
        cat.generation += 1;
        let projection = self.projection(&view.epoch)?;
        Ok(Read {
            host: self.clone(),
            view: view.clone(),
            projection,
        })
    }
    pub fn release(&self, view: &View) -> Result<()> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        if let Some(l) = cat.leases.get_mut(&view.lease) {
            require(l.active == 0, "ActiveRead")?;
            l.expires = 0;
        }
        cat.generation += 1;
        self.save_catalog(&cat)
    }
    pub fn advance_clock(&self, by: u64) -> Result<()> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        cat.now += by;
        cat.generation += 1;
        self.save_catalog(&cat)
    }
    pub fn build_epoch(&self, epoch: &str) -> Result<Route> {
        let _write = self
            .publication
            .lock()
            .map_err(|_| "publication poisoned")?;
        self.recover_inner(None)?;
        let route = self.route()?;
        self.protect_maintenance(epoch)?;
        let p = Arc::new(Projection::open(&self.epoch_path(epoch), true)?);
        self.databases
            .lock()
            .map_err(|_| "database map poisoned")?
            .insert(epoch.into(), p.clone());
        if let Err(e) = self.catch_up(&p, epoch) {
            self.release_maintenance(epoch)?;
            return Err(e);
        }
        Ok(route)
    }
    fn protect_maintenance(&self, epoch: &str) -> Result<Vec<(String, Manifest)>> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        let retained = self
            .history()?
            .into_iter()
            .filter(|(id, _)| !cat.retiring.contains(id) && !cat.reclaimed.contains(id))
            .collect::<Vec<_>>();
        cat.maintenance
            .entry(epoch.into())
            .or_default()
            .extend(retained.iter().map(|(id, _)| id.clone()));
        cat.generation += 1;
        self.save_catalog(&cat)?;
        Ok(retained)
    }
    fn release_maintenance(&self, epoch: &str) -> Result<()> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        cat.maintenance.remove(epoch);
        cat.generation += 1;
        self.save_catalog(&cat)
    }
    fn catch_up(&self, p: &Projection, epoch: &str) -> Result<()> {
        let c = p.connection()?;
        let retained = self.protect_maintenance(epoch)?;
        let mut previous = None;
        for (id, manifest) in retained {
            if p.count(
                &c,
                &format!("MATCH (r:Receipt {{id:{}}}) RETURN count(*)", quote(&id)),
            )? != 0
            {
                previous = Some(manifest);
                continue;
            }
            self.audit_manifest(&manifest)?;
            let mut candidate = Candidate {
                job: "rebuild".into(),
                revision: id,
                manifest,
                seal: String::new(),
            };
            candidate.seal = candidate.seal()?;
            let changes = self.changes_against(p, &c, &candidate, previous.as_ref())?;
            p.exec(&c, "BEGIN TRANSACTION")?;
            if let Err(e) = self.apply(p, &c, &candidate, &changes) {
                let _ = p.exec(&c, "ROLLBACK");
                return Err(e);
            }
            p.exec(&c, "COMMIT")?;
            previous = Some(candidate.manifest);
        }
        Ok(())
    }
    pub fn switch_epoch(&self, epoch: &str, expected: &Route, stop: Option<&str>) -> Result<()> {
        let _write = self
            .publication
            .lock()
            .map_err(|_| "publication poisoned")?;
        self.recover_inner(None)?;
        let p = self.projection(epoch)?;
        self.catch_up(&p, epoch)?;
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        let route = self.route()?;
        if route.epoch == epoch {
            cat.maintenance.remove(epoch);
            cat.generation += 1;
            self.save_catalog(&cat)?;
            return Ok(());
        }
        require(
            route.epoch == expected.epoch && route.generation == expected.generation,
            "RouteConflict",
        )?;
        let c = p.connection()?;
        for (id, _) in self.history()? {
            if !cat.retiring.contains(&id) && !cat.reclaimed.contains(&id) {
                require(
                    p.count(
                        &c,
                        &format!(
                            "MATCH (r:Receipt {{id:{}}}) WHERE r.manifest={} RETURN count(*)",
                            quote(&id),
                            quote(&id)
                        ),
                    )? == 1,
                    "IncompleteEpoch",
                )?;
            }
        }
        pause("before-route", stop);
        let switched = save(
            &self.root.join("route.json"),
            &Route {
                epoch: epoch.into(),
                generation: route.generation + 1,
            },
        );
        if let Err(e) = switched {
            self.recovery_required.store(true, Ordering::SeqCst);
            return Err(e);
        }
        cat.maintenance.remove(epoch);
        cat.generation += 1;
        self.save_catalog(&cat)?;
        pause("after-route", stop);
        Ok(())
    }
    pub fn plan_gc(&self, revisions: &[String]) -> Result<GcPlan> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        require(
            !self.recovery_required.load(Ordering::SeqCst),
            "RecoveryRequired",
        )?;
        let head = self.head()?;
        let mut retired = vec![];
        for id in revisions {
            let protected = cat.maintenance.values().any(|ids| ids.contains(id))
                || cat
                    .leases
                    .values()
                    .any(|l| l.view.revision == *id && (l.expires > cat.now || l.active > 0));
            if !protected && head.as_ref() != Some(id) {
                cat.retiring.insert(id.clone());
                retired.push(id.clone());
            }
        }
        cat.generation += 1;
        let mut reachable = BTreeSet::new();
        for (id, m) in self.history()? {
            if cat.retiring.contains(&id) || cat.reclaimed.contains(&id) {
                continue;
            }
            for r in m.shards.values() {
                reachable.insert(format!("facts/{}", r.hash));
                reachable.insert(format!("evidence/{}", r.source));
            }
        }
        for refs in cat.staging.values() {
            reachable.extend(refs.iter().cloned());
        }
        let mut objects = vec![];
        for kind in ["facts", "evidence"] {
            for f in fs::read_dir(self.root.join(kind))? {
                let f = f?;
                if f.file_type()?.is_file() {
                    let key = format!("{kind}/{}", f.file_name().to_string_lossy());
                    if !reachable.contains(&key) {
                        objects.push(key);
                    }
                }
            }
        }
        self.save_catalog(&cat)?;
        Ok(GcPlan {
            generation: cat.generation,
            revisions: retired,
            objects,
        })
    }
    pub fn sweep(&self, plan: &GcPlan) -> Result<usize> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        require(
            !self.recovery_required.load(Ordering::SeqCst),
            "RecoveryRequired",
        )?;
        require(cat.generation == plan.generation, "GcPlanStale")?;
        for object in &plan.objects {
            let path = self.root.join(object);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        for id in &plan.revisions {
            cat.retiring.remove(id);
            cat.reclaimed.insert(id.clone());
        }
        cat.generation += 1;
        self.save_catalog(&cat)?;
        Ok(plan.objects.len())
    }
    pub fn collect_epoch(&self, epoch: &str) -> Result<()> {
        let mut cat = self.catalog.lock().map_err(|_| "catalog poisoned")?;
        require(
            !self.recovery_required.load(Ordering::SeqCst) && !cat.maintenance.contains_key(epoch),
            "EpochMaintenanceOrRecoveryRequired",
        )?;
        require(self.route()?.epoch != epoch, "CurrentEpochProtected")?;
        require(
            !cat.leases
                .values()
                .any(|l| l.view.epoch == epoch && (l.expires > cat.now || l.active > 0)),
            "EpochLeased",
        )?;
        let mut map = self.databases.lock().map_err(|_| "database map poisoned")?;
        if let Some(p) = map.get(epoch) {
            require(Arc::strong_count(p) == 1, "EpochBusy")?;
        }
        map.remove(epoch);
        fs::remove_dir_all(
            self.epoch_path(epoch)
                .parent()
                .ok_or("missing epoch directory")?,
        )?;
        cat.generation += 1;
        self.save_catalog(&cat)
    }
    pub fn native_payload_count(&self) -> Result<i64> {
        let p = self.current_projection()?;
        let c = p.connection()?;
        p.count(&c, "MATCH (p:Payload) RETURN count(*)")
    }
    pub fn native_pending_count(&self) -> Result<i64> {
        let p = self.current_projection()?;
        let c = p.connection()?;
        p.count(&c, "MATCH (p:Pending) RETURN count(*)")
    }
    pub fn interval_count(&self, hash: &str) -> Result<i64> {
        let p = self.current_projection()?;
        let c = p.connection()?;
        p.count(
            &c,
            &format!(
                "MATCH (:ShardUse)-[:USES]->(p:Payload {{id:{}}}) RETURN count(*)",
                quote(hash)
            ),
        )
    }
}
impl Read {
    pub fn view(&self) -> &View {
        &self.view
    }
    pub fn adjacency_page(
        &self,
        payload: &str,
        subject: &str,
        ordinal: usize,
    ) -> Result<Vec<(String, String)>> {
        let m = self.host.manifest(&self.view.revision)?;
        let reference = m
            .shards
            .values()
            .find(|r| r.hash == payload && r.context == self.view.context)
            .ok_or("InactiveFactRef")?;
        let root = reference
            .adjacency_roots
            .get(subject)
            .ok_or("UnknownAdjacencySubject")?;
        let c = self.projection.connection()?;
        self.projection
            .adjacency_page(&c, payload, &self.view.context, subject, ordinal, root)
    }
    pub fn edges(&self, subject: &str, after: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let c = self.projection.connection()?;
        self.projection
            .edges(
                &c,
                self.view.sequence,
                &self.view.context,
                subject,
                after,
                limit,
            )?
            .iter()
            .map(|r| Ok((string(&r[0])?, string(&r[1])?)))
            .collect()
    }
    pub fn entities(&self, text: &str) -> Result<Vec<Vec<lbug::Value>>> {
        let c = self.projection.connection()?;
        self.projection
            .entities(&c, self.view.sequence, &self.view.context, text)
    }
    pub fn source(&self, shard: &str) -> Result<Vec<u8>> {
        let m = self.host.manifest(&self.view.revision)?;
        let r = m.shards.get(shard).ok_or("missing source shard")?;
        require(r.context == self.view.context, "SourceContextMismatch")?;
        let source = fs::read(self.host.root.join("evidence").join(&r.source))?;
        require(digest(&source) == r.source, "CorruptEvidence")?;
        Ok(source)
    }
    pub fn visible_entity_count(&self) -> Result<i64> {
        Ok(self.entities("")?.len() as i64)
    }
    pub fn sequence_from_receipt(&self) -> Result<i64> {
        let c = self.projection.connection()?;
        let rows = self.projection.rows(
            &c,
            &format!(
                "MATCH (r:Receipt {{id:{}}}) RETURN r.sequence",
                quote(&self.view.revision)
            ),
        )?;
        integer(&rows[0][0])
    }
}
