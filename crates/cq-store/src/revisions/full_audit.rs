//! Full audits read each payload page once. Compact SHA-256 identities bind
//! directory members to the already decoded facts, not to a trusted producer.
use super::*;
use cq_core::semantic::{
    production::{ScopeDescriptor, family_name},
    scope::{Completion, ContentRef, FactFamily},
    transfer,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
type Key = [u8; 32];
fn hash(bytes: &[u8]) -> Key {
    Sha256::digest(bytes).into()
}
fn reference_hash(value: &Value) -> Result<Key> {
    Ok(hash(&serde_json::to_vec(value)?))
}
#[derive(Clone, Copy)]
enum Owner {
    Unit(u32),
    Entity(Key),
    Project,
}
struct Fact {
    reference: Key,
    groups: Vec<u32>,
    group_count: usize,
    family: Option<FactFamily>,
    owner: Owner,
    scoped: bool,
}
#[derive(Default)]
struct Group {
    expected: u64,
    seen: bool,
}
fn family(s: &str) -> Option<FactFamily> {
    match s {
        "entity" => Some(FactFamily::Entity),
        "body" => Some(FactFamily::Body),
        "occurrence" => Some(FactFamily::Occurrence),
        "relation" => Some(FactFamily::Relation),
        "document" => Some(FactFamily::Document),
        _ => None,
    }
}
#[derive(Default)]
struct Proof {
    indexed: bool,
    facts: HashMap<Key, Fact>,
    group_ids: HashMap<Key, u32>,
    groups: Vec<Group>,
    units: BTreeMap<String, u32>,
    unit_names: Vec<String>,
    selected_units: BTreeSet<String>,
    entities: HashMap<Key, u32>,
    required: HashMap<Key, (String, &'static str)>,
    scopes: Vec<(String, Value)>,
}
impl Proof {
    fn unit(&mut self, name: &str) -> u32 {
        if let Some(id) = self.units.get(name) {
            return *id;
        }
        let id = self.unit_names.len() as u32;
        self.units.insert(name.into(), id);
        self.unit_names.push(name.into());
        id
    }
    fn require(&mut self, family: &str, id: &str, code: &'static str) {
        let key = format!("{family}:{id}");
        self.required
            .entry(hash(key.as_bytes()))
            .or_insert((key, code));
    }
    fn record(&mut self, key: String, fingerprint: Key, value: Value) -> Result<()> {
        let name = key
            .split(':')
            .next()
            .ok_or_else(|| Error::new("InvalidFactKey", &key))?;
        let f = family(name);
        let kh = hash(key.as_bytes());
        let names = cq_core::ports::facts::groups_indexed(name, &value, self.indexed);
        let mut group_ids = Vec::with_capacity(names.len());
        for name in &names {
            let gh = hash(name.as_bytes());
            let id = if let Some(id) = self.group_ids.get(&gh) {
                *id
            } else {
                let id = self.groups.len() as u32;
                self.group_ids.insert(gh, id);
                self.groups.push(Group::default());
                id
            };
            self.groups[id as usize].expected += 1;
            group_ids.push(id);
        }
        let owner = match f {
            Some(FactFamily::Entity | FactFamily::Body | FactFamily::Occurrence) => {
                Owner::Unit(self.unit(value["unit"].as_str().unwrap_or("$project")))
            }
            Some(FactFamily::Relation) => Owner::Entity(hash(
                format!("entity:{}", value["from"].as_str().unwrap_or("")).as_bytes(),
            )),
            Some(FactFamily::Document) => value["owner"].as_str().map_or(Owner::Project, |id| {
                Owner::Entity(hash(format!("entity:{id}").as_bytes()))
            }),
            _ => Owner::Project,
        };
        if f == Some(FactFamily::Entity)
            && let Owner::Unit(id) = owner
        {
            self.entities.insert(kh, id);
        }
        if name == "unit" {
            self.selected_units
                .insert(key.split_once(':').unwrap().1.into());
        }
        if name == "scope" {
            self.scopes
                .push((key.split_once(':').unwrap().1.into(), value.clone()));
        }
        if name == "relation" {
            for field in ["from", "to"] {
                let id = value[field]
                    .as_str()
                    .ok_or_else(|| Error::new("InvalidRelation", &key))?;
                self.require("entity", id, "DanglingEndpoint");
            }
        }
        if let Some(id) = value["provenance"].as_str() {
            self.require("provenance", id, "MissingProvenance");
        }
        if self
            .facts
            .insert(
                kh,
                Fact {
                    reference: fingerprint,
                    groups: group_ids,
                    group_count: names.len(),
                    family: f,
                    owner,
                    scoped: false,
                },
            )
            .is_some()
        {
            return Err(Error::new("InvalidFactKey", "duplicate fact identity"));
        }
        Ok(())
    }
    fn scopes(&mut self, o: &Objects, partial: bool) -> Result<()> {
        for (id, value) in std::mem::take(&mut self.scopes) {
            let descriptor: ScopeDescriptor = serde_json::from_value(value["descriptor"].clone())?;
            if descriptor.id()? != id
                || descriptor.key.context != cq_core::model::digest(descriptor.key.unit.as_bytes())
            {
                return Err(Error::new(
                    "InvalidScopeMembership",
                    "scope identity mismatch",
                ));
            }
            let root: tree::Root = serde_json::from_value(value["members"].clone())?;
            let mut refs = BTreeMap::new();
            tree::visit(o, &root, |key, v| {
                let f: FactRef = serde_json::from_value(v.clone())?;
                let entry = self
                    .facts
                    .get_mut(&hash(key.as_bytes()))
                    .ok_or_else(|| Error::new("InvalidScopeMembership", "foreign scope member"))?;
                if entry.scoped
                    || entry
                        .family
                        .as_ref()
                        .is_none_or(|f| family_name(f) != family_name(&descriptor.key.family))
                    || entry.reference != reference_hash(v)?
                {
                    return Err(Error::new(
                        "InvalidScopeMembership",
                        "foreign/duplicate scope member",
                    ));
                }
                let owner = match entry.owner {
                    Owner::Unit(id) => Some(id),
                    Owner::Entity(k) => self.entities.get(&k).copied(),
                    Owner::Project => None,
                };
                let name = owner
                    .map(|id| self.unit_names[id as usize].as_str())
                    .unwrap_or("$project");
                let name = if name == "external" || self.selected_units.contains(name) {
                    name
                } else {
                    "$project"
                };
                if name != descriptor.key.unit {
                    return Err(Error::new(
                        "InvalidScopeMembership",
                        "owner/context mismatch",
                    ));
                }
                entry.scoped = true;
                refs.insert(
                    key.into(),
                    ContentRef {
                        hash: f.digest,
                        bytes: f.length,
                    },
                );
                Ok(())
            })?;
            if value.get("state_base").is_some()
                && value["state_base"] != transfer::base_root(&descriptor.key, &refs)?
            {
                return Err(Error::new("InvalidScopeMembership", "base token mismatch"));
            }
            if descriptor.digest != transfer::state_digest(&refs)?
                || descriptor.rows != refs.len() as u64
                || descriptor.completion != Completion::Complete && (!refs.is_empty() || !partial)
            {
                return Err(Error::new(
                    "InvalidScopeMembership",
                    "completion/count/digest mismatch",
                ));
            }
        }
        if self.facts.values().any(|f| f.family.is_some() && !f.scoped) {
            return Err(Error::new(
                "InvalidScopeMembership",
                "semantic fact lacks scope completion proof",
            ));
        }
        Ok(())
    }
}
impl StoreHost {
    fn audit_full_pages(&self, r: &Revision) -> Result<Value> {
        r.verify()?;
        let mut pages = BTreeMap::<String, Vec<(String, FactRef, Key)>>::new();
        tree::visit(&self.objects, &r.roots.facts, |key, v| {
            let f: FactRef = serde_json::from_value(v.clone())?;
            pages.entry(f.object.hash.clone()).or_default().push((
                key.into(),
                f,
                reference_hash(v)?,
            ));
            Ok(())
        })?;
        let page_count = pages.len();
        let mut proof = Proof {
            indexed: r.metadata["search_index"]["version"] == 1,
            ..Proof::default()
        };
        for (_, rows) in pages {
            let bytes = self.objects.audit_bytes(&rows[0].1.object)?;
            // Rows sharing one physical page stay adjacent; hash/IO are not
            // repeated after an unrelated page has evicted it from the cache.
            for (key, f, fingerprint) in rows {
                let value = writer::decode_bytes(&bytes, &f)?;
                proof.record(key, fingerprint, value)?;
            }
        }
        for (key, (id, code)) in std::mem::take(&mut proof.required) {
            if !proof.facts.contains_key(&key) {
                return Err(Error::new(code, id));
            }
        }
        tree::visit(&self.objects, &r.roots.groups, |name, v| {
            let group: writer::Group = serde_json::from_value(v.clone())?;
            let id = *proof
                .group_ids
                .get(&hash(name.as_bytes()))
                .ok_or_else(|| Error::new("IndexIncomplete", "foreign directory"))?;
            if proof.groups[id as usize].seen {
                return Err(Error::new("IndexIncomplete", "duplicate directory"));
            }
            proof.groups[id as usize].seen = true;
            let mut count = 0;
            tree::visit(&self.objects, &group.root, |key, v| {
                let entry = proof
                    .facts
                    .get(&hash(key.as_bytes()))
                    .ok_or_else(|| Error::new("IndexIncomplete", "foreign directory member"))?;
                if entry.reference != reference_hash(v)?
                    || !entry.groups[..entry.group_count].contains(&id)
                {
                    return Err(Error::new(
                        "IndexIncomplete",
                        "directory owner/reference mismatch",
                    ));
                }
                count += 1;
                Ok(())
            })?;
            if count != group.count || count != proof.groups[id as usize].expected {
                return Err(Error::new("IndexIncomplete", "directory count mismatch"));
            }
            Ok(())
        })?;
        if proof.groups.iter().any(|g| !g.seen) {
            return Err(Error::new("IndexIncomplete", "missing canonical directory"));
        }
        tree::visit(&self.objects, &r.roots.sources, |_, v| {
            let src: crate::facts::source::Source = serde_json::from_value(v.clone())?;
            src.audit(&self.objects)
        })?;
        if r.metadata["scope_protocol"] == 2 {
            proof.scopes(&self.objects, r.metadata["production"]["partial"] == true)?;
        }
        Ok(
            json!({"payload_pages":page_count,"facts":proof.facts.len(),"directories":proof.groups.len(),"payload_order":"physical_page","directory_checks":"exact membership through SHA-256 fact/reference identities"}),
        )
    }
    pub(super) fn audit(&self, r: &Revision) -> Result<Value> {
        let result = self.audit_full_pages(r);
        // Full audit indexes are temporary; return their freed arenas before
        // opening a large native database. No live allocations are changed.
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        unsafe {
            libc::malloc_trim(0);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Arc<StoreHost>, Revision) {
        let t = tempfile::tempdir().unwrap();
        let h = StoreHost::open(t.path(), "full-proof").unwrap();
        let o = Objects::buffered(h.objects.root.clone()).unwrap();
        let mut w = writer::Writer::new(&o, writer::Roots::default());
        w.append("provenance", "p", &json!({"id":"p"})).unwrap();
        for id in ["a", "b"] {
            w.append("entity",id,&json!({"id":id,"kind":"Function","name":id,"unit":"u","external":false,"provenance":"p"})).unwrap();
        }
        w.append(
            "relation",
            "r",
            &json!({"id":"r","kind":"Calls","from":"a","to":"b","provenance":"p"}),
        )
        .unwrap();
        let (roots, changes) = w.finish_inventory().unwrap();
        o.sync_pending().unwrap();
        let mut r = Revision {
            format: 2,
            project: "full-proof".into(),
            revision: cq_core::model::digest(b"proof"),
            parent: None,
            sequence: 1,
            roots,
            metadata: json!({"project":"full-proof","revision":cq_core::model::digest(b"proof")}),
            changes,
            seal: String::new(),
        };
        r.seal().unwrap();
        (t, h, r)
    }
    #[test]
    fn grouped_full_audit_checks_payload_once_and_preserves_exact_reference_comparison() {
        let (_t, h, mut r) = fixture();
        let report = h.audit(&r).unwrap();
        assert_eq!(report["payload_pages"], 1);
        assert_eq!(report["facts"], 4);
        h.audit_delta(&r).unwrap();
        let mut group: writer::Group = serde_json::from_value(
            tree::get(
                &h.objects,
                &r.roots.groups,
                "out:a",
                &mut ReadStats::default(),
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        let mut value = tree::get(
            &h.objects,
            &group.root,
            "relation:r",
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap();
        value["foreign_extra_field"] = json!(true);
        group.root = tree::set(
            &h.objects,
            &group.root,
            "relation:r",
            Some(value),
            &mut ReadStats::default(),
        )
        .unwrap();
        r.roots.groups = tree::set(
            &h.objects,
            &r.roots.groups,
            "out:a",
            Some(json!(group)),
            &mut ReadStats::default(),
        )
        .unwrap();
        r.seal().unwrap();
        assert_eq!(h.audit(&r).unwrap_err().code(), "IndexIncomplete");
    }
    #[test]
    fn full_audit_rejects_missing_endpoints_and_initial_delta_count_lies() {
        let (_t, h, mut r) = fixture();
        r.changes.facts_changed += 1;
        r.seal().unwrap();
        assert_eq!(h.audit_delta(&r).unwrap_err().code(), "InvalidDelta");
        let mut w = writer::Writer::new(&h.objects, r.roots.clone());
        w.delete("entity:b").unwrap();
        w.flush().unwrap();
        r.roots = w.roots;
        r.seal().unwrap();
        assert_eq!(h.audit(&r).unwrap_err().code(), "DanglingEndpoint");
    }
    #[test]
    fn same_hash_with_conflicting_page_lengths_is_not_trusted() {
        let (_t, h, mut r) = fixture();
        let mut value = tree::get(
            &h.objects,
            &r.roots.facts,
            "entity:a",
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap();
        value["object"]["bytes"] = json!(value["object"]["bytes"].as_u64().unwrap() + 1);
        r.roots.facts = tree::set(
            &h.objects,
            &r.roots.facts,
            "entity:a",
            Some(value),
            &mut ReadStats::default(),
        )
        .unwrap();
        r.seal().unwrap();
        assert_eq!(h.audit(&r).unwrap_err().code(), "DigestMismatch");
    }
}
