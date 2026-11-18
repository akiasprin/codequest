//! Explicit retention only. No legacy data or raw evidence is automatically
//! deleted. All projects and staged roots share the object deletion barrier.
use super::*;
pub(crate) struct ObjectGate(File);
impl ObjectGate {
    pub(crate) fn lock(format: &Path) -> Result<Self> {
        std::fs::create_dir_all(format)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(format.join("object-gate"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(f))
    }
}
impl Drop for ObjectGate {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetentionPlan {
    pub generation: u64,
    pub revisions: Vec<String>,
    pub objects: Vec<String>,
    pub bytes: u64,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TreeKind {
    Facts,
    Groups,
    Sources,
    Blocks,
}
#[derive(Default)]
struct Reachability {
    live: BTreeSet<String>,
    trees: BTreeSet<(String, TreeKind)>,
    scopes: BTreeSet<(String, u64, u64, String)>,
}
fn mark(o: &Objects, value: &Value, state: &mut Reachability) -> Result<()> {
    let r: Revision = serde_json::from_value(value.get("manifest").unwrap_or(value).clone())?;
    r.verify()?;
    let mut pending = Vec::new();
    for (root, kind) in [
        (&r.roots.facts, TreeKind::Facts),
        (&r.changes.upserts, TreeKind::Facts),
        (&r.changes.deletes, TreeKind::Facts),
        (&r.roots.groups, TreeKind::Groups),
        (&r.roots.sources, TreeKind::Sources),
    ] {
        if let Some(root) = root {
            pending.push((root.clone(), kind));
        }
    }
    while let Some((reference, kind)) = pending.pop() {
        if !state.trees.insert((reference.hash.clone(), kind)) {
            continue;
        }
        state.live.insert(reference.hash.clone());
        let bytes = o.audit_bytes(&reference)?;
        match serde_json::from_slice::<tree::Node>(&bytes)? {
            tree::Node::Branch(children) => {
                for child in children.into_values() {
                    pending.push((child, kind));
                }
            }
            tree::Node::Leaf(rows) => {
                for (ordered, value) in rows {
                    match kind {
                        TreeKind::Facts => {
                            let f: FactRef = serde_json::from_value(value)?;
                            if state.live.insert(f.object.hash.clone()) {
                                o.audit(&f.object)?;
                            }
                            let key = ordered
                                .split_once(':')
                                .ok_or_else(|| Error::new("InvalidManifestTree", "ordered key"))?
                                .1;
                            if key.starts_with("scope:")
                                && state.scopes.insert((
                                    f.object.hash.clone(),
                                    f.offset,
                                    f.length,
                                    f.digest.clone(),
                                ))
                            {
                                // Scope control fields are decoded at their FactRef
                                // range even when a page contains several JSONL rows.
                                let scope = writer::decode(o, &f, &mut ReadStats::default())?;
                                if let Some(v) = scope.get("members") {
                                    let root: tree::Root = serde_json::from_value(v.clone())?;
                                    if let Some(root) = root {
                                        pending.push((root, TreeKind::Facts));
                                    }
                                }
                            }
                        }
                        TreeKind::Groups => {
                            let group: writer::Group = serde_json::from_value(value)?;
                            if let Some(root) = group.root {
                                pending.push((root, TreeKind::Facts));
                            }
                        }
                        TreeKind::Sources => {
                            let source: crate::facts::source::Source =
                                serde_json::from_value(value)?;
                            if let Some(root) = source.blocks {
                                pending.push((root, TreeKind::Blocks));
                            }
                        }
                        TreeKind::Blocks => {
                            let block: cq_core::ports::facts::ObjectRef =
                                serde_json::from_value(value)?;
                            if state.live.insert(block.hash.clone()) {
                                o.audit(&block)?;
                            }
                            // Source bytes are opaque evidence, never graph control.
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
fn sweep(o: &Objects, hashes: &[String]) -> Result<()> {
    for batch in hashes.chunks(128) {
        let mut directories = BTreeSet::new();
        for hash in batch {
            let p = o.root.join(&hash[..2]).join(hash);
            if p.exists() {
                std::fs::remove_file(&p)?;
                directories.insert(p.parent().unwrap().to_owned());
            }
        }
        for dir in directories {
            File::open(dir)?.sync_all()?;
        }
    }
    Ok(())
}
impl StoreHost {
    pub fn pin_cursor(&self, id: &str, revision: &str) -> Result<()> {
        let mut c = self.catalog.lock().unwrap();
        if self.blocked.load(Ordering::Acquire)
            || c.retiring.contains(revision)
            || !c.committed.contains_key(revision)
        {
            return Err(Error::new(
                "RevisionUnavailable",
                "cursor cannot pin this revision",
            ));
        }
        c.leases.insert(
            format!("cursor-{id}"),
            Lease {
                revision: revision.into(),
                epoch: self.route.lock().unwrap().epoch.clone(),
                expires: cq_capture_protocol::now_ms() + LEASE_MS,
            },
        );
        c.generation += 1;
        save(&self.root.join("catalog.json"), &*c)
    }
    pub fn preview_retention(&self, keep: usize) -> Result<RetentionPlan> {
        let _p = self.publication.lock().unwrap();
        self.recover()?;
        let _objects = ObjectGate::lock(self.objects.root.parent().unwrap())?;
        let c = self.catalog.lock().unwrap();
        let head = self.head()?;
        let active = self.active.lock().unwrap();
        let protected = c
            .leases
            .values()
            .filter(|l| l.expires >= cq_capture_protocol::now_ms())
            .map(|l| l.revision.clone())
            .chain(
                active
                    .iter()
                    .filter(|(_, n)| **n > 0)
                    .map(|(r, _)| r.clone()),
            )
            .chain(c.maintenance.values().flatten().cloned())
            .collect::<BTreeSet<_>>();
        let mut revisions = c
            .committed
            .keys()
            .map(|id| self.manifest(id).map(|r| (r.sequence, id.clone())))
            .collect::<Result<Vec<_>>>()?;
        revisions.sort();
        revisions.reverse();
        let retire = revisions
            .into_iter()
            .skip(keep)
            .map(|(_, id)| id)
            .filter(|id| head.as_ref() != Some(id) && !protected.contains(id))
            .collect::<Vec<_>>();
        let generation = c.generation;
        drop(active);
        drop(c);
        let (objects, bytes) = self.unreachable(&retire)?;
        Ok(RetentionPlan {
            generation,
            revisions: retire,
            objects,
            bytes,
        })
    }
    fn unreachable(&self, retiring: &[String]) -> Result<(Vec<String>, u64)> {
        let format = self.objects.root.parent().unwrap();
        let mut live = Reachability::default();
        let projects = format.join("projects");
        if projects.exists() {
            for entry in std::fs::read_dir(projects)? {
                let path = entry?.path();
                let catalog = path.join("catalog.json");
                if !catalog.exists() {
                    continue;
                }
                let c: Catalog = load(&catalog)?;
                for (id, seal) in &c.committed {
                    if path == self.root && retiring.contains(id) {
                        continue;
                    }
                    let r: Revision = load(&path.join("commits").join(id))?;
                    if r.revision != *id || r.seal != *seal {
                        return Err(Error::new("CorruptRevision", "retention catalog binding"));
                    }
                    mark(&self.objects, &serde_json::to_value(r)?, &mut live)?;
                }
                let pending = path.join("pending.json");
                if pending.exists() {
                    mark(&self.objects, &load::<Value>(&pending)?, &mut live)?;
                }
                for ids in c.maintenance.values() {
                    for id in ids {
                        let r: Revision = load(&path.join("commits").join(id))?;
                        mark(&self.objects, &serde_json::to_value(r)?, &mut live)?;
                    }
                }
            }
        }
        let staging = format.join("staging");
        if staging.exists() {
            for entry in std::fs::read_dir(staging)? {
                let path = entry?.path();
                if path.extension().is_some_and(|x| x == "json") {
                    mark(&self.objects, &load::<Value>(&path)?, &mut live)?;
                }
            }
        }
        let mut remove = vec![];
        let mut bytes = 0;
        for dir in std::fs::read_dir(self.objects.directory())? {
            let dir = dir?.path();
            if !dir.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(dir)? {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                if valid_hash(&name) && !live.live.contains(&name) {
                    bytes += file.metadata()?.len();
                    remove.push(name);
                }
            }
        }
        remove.sort();
        Ok((remove, bytes))
    }
    pub fn apply_retention(&self, plan: &RetentionPlan) -> Result<RetentionPlan> {
        let _p = self.publication.lock().unwrap();
        self.recover()?;
        let _objects = ObjectGate::lock(self.objects.root.parent().unwrap())?;
        let mut c = self.catalog.lock().unwrap();
        if c.generation != plan.generation {
            return Err(Error::new(
                "StaleRetentionPlan",
                "root generation changed; preview again",
            ));
        }
        let active = self.active.lock().unwrap();
        let head = self.head()?;
        for id in &plan.revisions {
            if !c.committed.contains_key(id) {
                return Err(Error::new("RevisionUnavailable", id));
            }
            if head.as_ref() == Some(id)
                || active.get(id).is_some_and(|n| *n > 0)
                || c.leases
                    .values()
                    .any(|l| &l.revision == id && l.expires >= cq_capture_protocol::now_ms())
                || c.maintenance.values().any(|ids| ids.contains(id))
            {
                return Err(Error::new("RevisionProtected", id));
            }
        }
        let journal = self.root.join("retirement.json");
        self.save_retirement(plan)?;
        for id in &plan.revisions {
            c.retiring.insert(id.clone());
            c.committed.remove(id);
        }
        c.generation += 1;
        save(&self.root.join("catalog.json"), &*c)?;
        drop(active);
        drop(c);
        // Recompute after closing admission. Staging and other projects may keep
        // objects that appeared in the preview, so its deletion list is not trusted.
        let (objects, bytes) = self.unreachable(&[])?;
        let applied = RetentionPlan {
            generation: plan.generation,
            revisions: plan.revisions.clone(),
            objects,
            bytes,
        };
        self.save_retirement(&applied)?;
        sweep(&self.objects, &applied.objects)?;
        self.objects.clear_cache();
        std::fs::remove_file(journal)?;
        File::open(&self.root)?.sync_all()?;
        Ok(applied)
    }
    pub(super) fn recover_retirement(&self) -> Result<()> {
        let path = self.root.join("retirement.json");
        if !path.exists() {
            return Ok(());
        }
        // Metadata from before the crash already closes admission. Retrying a
        // newly computed plan is idempotent and includes any newly staged roots.
        let _objects = ObjectGate::lock(self.objects.root.parent().unwrap())?;
        let plan: RetentionPlan = self.load_retirement()?;
        let mut c = self.catalog.lock().unwrap();
        for id in &plan.revisions {
            if c.leases
                .values()
                .any(|l| &l.revision == id && l.expires >= cq_capture_protocol::now_ms())
            {
                return Err(Error::new(
                    "RecoveryRequired",
                    "retirement journal overlaps a protected lease",
                ));
            }
            c.retiring.insert(id.clone());
            c.committed.remove(id);
        }
        c.generation += 1;
        save(&self.root.join("catalog.json"), &*c)?;
        drop(c);
        let (objects, _) = self.unreachable(&[])?;
        sweep(&self.objects, &objects)?;
        self.objects.clear_cache();
        std::fs::remove_file(path)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn collect_epochs(&self) -> Result<Vec<String>> {
        let _p = self.publication.lock().unwrap();
        self.recover()?;
        let c = self.catalog.lock().unwrap();
        let current = self.route.lock().unwrap().epoch.clone();
        let active = self.epoch_active.lock().unwrap();
        let mut removed = vec![];
        for entry in std::fs::read_dir(self.root.join("indexes"))? {
            let path = entry?.path();
            let Some(id) = path.file_name().and_then(|x| x.to_str()) else {
                continue;
            };
            if id == current
                || c.maintenance.contains_key(id)
                || c.leases
                    .values()
                    .any(|l| l.epoch == id && l.expires >= cq_capture_protocol::now_ms())
                || active.get(id).is_some_and(|n| *n > 0)
            {
                continue;
            }
            if !crate::ladybug::residency::evict(&path.join("graph.lbdb")) {
                continue;
            }
            std::fs::remove_dir_all(&path)?;
            removed.push(id.into());
        }
        Ok(removed)
    }
}
