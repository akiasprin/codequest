//! Incremental publication inherits the audited immutable parent. It verifies
//! changed Merkle paths, exact directory deltas and affected completion scopes.
//! Recovery/rebuild and conservative global changes retain the complete audit.
use super::*;
use cq_core::semantic::{
    production::{ScopeDescriptor, family_name},
    scope::{Completion, ContentRef, FactFamily, ScopeKey},
    transfer,
};
type Edits = BTreeMap<String, (Option<Value>, Option<Value>)>;
fn semantic(family: &str) -> bool {
    matches!(
        family,
        "entity" | "body" | "occurrence" | "relation" | "document"
    )
}
impl StoreHost {
    fn value(&self, r: &Revision, key: &str) -> Result<Option<Value>> {
        tree::get(
            &self.objects,
            &r.roots.facts,
            key,
            &mut ReadStats::default(),
        )?
        .map(|v| {
            writer::decode(
                &self.objects,
                &serde_json::from_value(v)?,
                &mut ReadStats::default(),
            )
        })
        .transpose()
    }
    fn scope_key(&self, r: &Revision, family: &str, v: &Value) -> Result<ScopeKey> {
        let unit = match family {
            "entity" | "body" | "occurrence" => v["unit"].as_str().map(str::to_owned),
            "relation" | "document" => {
                let field = if family == "relation" {
                    "from"
                } else {
                    "owner"
                };
                v[field]
                    .as_str()
                    .map(|id| self.value(r, &format!("entity:{id}")))
                    .transpose()?
                    .flatten()
                    .and_then(|v| v["unit"].as_str().map(str::to_owned))
            }
            _ => None,
        }
        .unwrap_or_else(|| "$project".into());
        let unit = if unit == "external"
            || writer::reference(
                &self.objects,
                &r.roots,
                "unit",
                &unit,
                &mut ReadStats::default(),
            )?
            .is_some()
        {
            unit
        } else {
            "$project".into()
        };
        Ok(ScopeKey {
            context: cq_core::model::digest(unit.as_bytes()),
            unit,
            family: match family {
                "entity" => FactFamily::Entity,
                "body" => FactFamily::Body,
                "occurrence" => FactFamily::Occurrence,
                "relation" => FactFamily::Relation,
                _ => FactFamily::Document,
            },
        })
    }
    fn scope_member(&self, r: &Revision, key: &ScopeKey, id: &str) -> Result<Option<Value>> {
        let sid = cq_core::model::digest(&serde_json::to_vec(key)?);
        let Some(scope) = self.value(r, &format!("scope:{sid}"))? else {
            return Ok(None);
        };
        let root = serde_json::from_value(scope["members"].clone())?;
        tree::get(&self.objects, &root, id, &mut ReadStats::default())
    }
    pub(super) fn audit_publication(&self, r: &Revision) -> Result<Value> {
        if let Some(proof) = self.cached_audit(r)? {
            self.audit_delta(r)?;
            return Ok(json!({"mode":"resumed_full_audit","proof":proof}));
        }
        let prior = r
            .parent
            .as_deref()
            .map(|id| self.manifest(id))
            .transpose()?;
        let full = |why: &str| -> Result<Value> {
            let proof = self.audit(r)?;
            self.audit_delta(r)?;
            Ok(json!({"mode":"full","reason":why,"proof":proof}))
        };
        let Some(base) = prior else {
            return full("initial revision");
        };
        if self.catalog.lock().unwrap().committed.get(&base.revision) != Some(&base.seal)
            || r.changes.facts_changed + r.changes.facts_deleted > 4096
            || base.metadata["scope_protocol"] != r.metadata["scope_protocol"]
        {
            return full("untrusted/incompatible parent or large change set");
        }
        r.verify()?;
        if r.sequence != base.sequence + 1
            || r.metadata["revision"] != r.revision
            || r.metadata["project"] != r.project
        {
            return Err(Error::new("InvalidRevision", "coordinates"));
        }
        let mut reads = ReadStats::default();
        let mut facts = Edits::new();
        tree::diff(
            &self.objects,
            &base.roots.facts,
            &r.roots.facts,
            &mut reads,
            |key, a, b| {
                facts.insert(key.into(), (a.cloned(), b.cloned()));
                Ok(())
            },
        )?;
        let mut groups = BTreeMap::<String, Edits>::new();
        let mut payloads = BTreeMap::<String, (Option<Value>, Option<Value>)>::new();
        let mut upserts = 0u64;
        let mut deletes = 0u64;
        for (key, (a, b)) in &facts {
            let family = key.split(':').next().unwrap();
            let decode = |v: &Value| -> Result<Value> {
                writer::decode(
                    &self.objects,
                    &serde_json::from_value(v.clone())?,
                    &mut ReadStats::default(),
                )
            };
            let old = a.as_ref().map(decode).transpose()?;
            let new = b.as_ref().map(decode).transpose()?;
            if family == "entity"
                && (new.is_none()
                    || old
                        .as_ref()
                        .zip(new.as_ref())
                        .is_some_and(|(a, b)| a["unit"] != b["unit"]))
                || family == "unit" && (a.is_none() || b.is_none())
                || family == "provenance" && b.is_none()
            {
                return full("owner, unit inventory or provenance removal");
            }
            if let Some(v) = b {
                upserts += 1;
                if tree::get(&self.objects, &r.changes.upserts, key, &mut reads)?.as_ref()
                    != Some(v)
                {
                    return Err(Error::new("InvalidDelta", "missing or mismatched upsert"));
                }
                let f: FactRef = serde_json::from_value(v.clone())?;
                self.objects.audit(&f.object)?;
                if family == "relation" {
                    for endpoint in ["from", "to"] {
                        let id = new.as_ref().unwrap()[endpoint]
                            .as_str()
                            .ok_or_else(|| Error::new("InvalidRelation", key))?;
                        if writer::reference(&self.objects, &r.roots, "entity", id, &mut reads)?
                            .is_none()
                        {
                            return Err(Error::new("DanglingEndpoint", id));
                        }
                    }
                }
                if let Some(id) = new.as_ref().unwrap()["provenance"].as_str()
                    && writer::reference(&self.objects, &r.roots, "provenance", id, &mut reads)?
                        .is_none()
                {
                    return Err(Error::new("MissingProvenance", id));
                }
            } else {
                deletes += 1;
                if tree::get(&self.objects, &r.changes.deletes, key, &mut reads)?.as_ref()
                    != a.as_ref()
                {
                    return Err(Error::new("InvalidDelta", "missing delete"));
                }
            }
            for (value, reference, is_old) in [
                (old.as_ref(), a.as_ref(), true),
                (new.as_ref(), b.as_ref(), false),
            ] {
                if let Some(value) = value {
                    for group in cq_core::ports::facts::groups_indexed(
                        family,
                        value,
                        if is_old {
                            base.metadata["search_index"]["version"] == 1
                        } else {
                            r.metadata["search_index"]["version"] == 1
                        },
                    ) {
                        let entry = groups
                            .entry(group)
                            .or_default()
                            .entry(key.clone())
                            .or_default();
                        if is_old {
                            entry.0 = reference.cloned();
                        } else {
                            entry.1 = reference.cloned();
                        }
                    }
                }
            }
            payloads.insert(key.clone(), (old, new));
        }
        if upserts != r.changes.facts_changed || deletes != r.changes.facts_deleted {
            return Err(Error::new("InvalidDelta", "counts"));
        }
        for (root, expected, is_upsert) in [
            (&r.changes.upserts, upserts, true),
            (&r.changes.deletes, deletes, false),
        ] {
            let mut count = 0;
            tree::visit(&self.objects, root, |key, v| {
                count += 1;
                let actual = facts.get(key).and_then(|(a, b)| {
                    if is_upsert {
                        b.as_ref()
                    } else if b.is_none() {
                        a.as_ref()
                    } else {
                        None
                    }
                });
                if actual != Some(v) {
                    return Err(Error::new("InvalidDelta", "foreign delta row"));
                }
                Ok(())
            })?;
            if count != expected {
                return Err(Error::new("InvalidDelta", "extra rows"));
            }
        }
        groups.retain(|_, edits| {
            edits.retain(|_, (a, b)| a != b);
            !edits.is_empty()
        });
        let group_count = groups.len();
        tree::diff(
            &self.objects,
            &base.roots.groups,
            &r.roots.groups,
            &mut reads,
            |name, a, b| {
                let mut expected = groups
                    .remove(name)
                    .ok_or_else(|| Error::new("IndexIncomplete", "unexplained directory change"))?;
                let old = a
                    .cloned()
                    .map(serde_json::from_value::<writer::Group>)
                    .transpose()?
                    .unwrap_or_default();
                let new = b
                    .cloned()
                    .map(serde_json::from_value::<writer::Group>)
                    .transpose()?
                    .unwrap_or_default();
                let mut count = old.count as i64;
                tree::diff(
                    &self.objects,
                    &old.root,
                    &new.root,
                    &mut ReadStats::default(),
                    |key, a, b| {
                        let wanted = expected.remove(key).ok_or_else(|| {
                            Error::new("IndexIncomplete", "foreign directory member")
                        })?;
                        if a != wanted.0.as_ref() || b != wanted.1.as_ref() {
                            return Err(Error::new("IndexIncomplete", "directory member mismatch"));
                        }
                        count += i64::from(b.is_some()) - i64::from(a.is_some());
                        Ok(())
                    },
                )?;
                if !expected.is_empty() || count < 0 || count as u64 != new.count {
                    return Err(Error::new(
                        "IndexIncomplete",
                        "missing directory update/count mismatch",
                    ));
                }
                Ok(())
            },
        )?;
        if !groups.is_empty() {
            return Err(Error::new("IndexIncomplete", "directory update absent"));
        }
        tree::diff(
            &self.objects,
            &base.roots.sources,
            &r.roots.sources,
            &mut reads,
            |_, _, b| {
                if let Some(v) = b {
                    serde_json::from_value::<crate::facts::source::Source>(v.clone())?
                        .audit(&self.objects)?;
                }
                Ok(())
            },
        )?;
        let mut scope_members = 0u64;
        if r.metadata["scope_protocol"] == 2 {
            for (key, (old, new)) in &payloads {
                let (family, _) = key.split_once(':').unwrap();
                if semantic(family) {
                    if let Some(new) = new {
                        let scope = self.scope_key(r, family, new)?;
                        if self.scope_member(r, &scope, key)?.as_ref() != facts[key].1.as_ref() {
                            return Err(Error::new(
                                "InvalidScopeMembership",
                                "changed semantic fact lacks completion proof",
                            ));
                        }
                    } else if let Some(old) = old {
                        let scope = self.scope_key(&base, family, old)?;
                        if self.scope_member(r, &scope, key)?.is_some() {
                            return Err(Error::new(
                                "InvalidScopeMembership",
                                "deleted fact remains a scope member",
                            ));
                        }
                    }
                }
            }
            for (key, (_, new)) in &payloads {
                if !key.starts_with("scope:") {
                    continue;
                }
                let previous_scope = self.value(&base, key)?;
                let previous_root: tree::Root = previous_scope
                    .as_ref()
                    .map(|v| serde_json::from_value(v["members"].clone()))
                    .transpose()?
                    .flatten();
                let next_root: tree::Root = new
                    .as_ref()
                    .map(|v| serde_json::from_value(v["members"].clone()))
                    .transpose()?
                    .flatten();
                tree::diff(
                    &self.objects,
                    &previous_root,
                    &next_root,
                    &mut ReadStats::default(),
                    |member, a, b| {
                        if a.is_some()
                            && b.is_none()
                            && let Some(row) = self.value(r, member)?
                        {
                            let family = member.split(':').next().unwrap();
                            let scope = self.scope_key(r, family, &row)?;
                            if format!(
                                "scope:{}",
                                cq_core::model::digest(&serde_json::to_vec(&scope)?)
                            ) == *key
                            {
                                return Err(Error::new(
                                    "InvalidScopeMembership",
                                    "live semantic fact removed from its completion proof",
                                ));
                            }
                        }
                        Ok(())
                    },
                )?;
                let Some(value) = new else {
                    continue;
                };
                let descriptor: ScopeDescriptor =
                    serde_json::from_value(value["descriptor"].clone())?;
                if format!("scope:{}", descriptor.id()?) != *key
                    || descriptor.key.context
                        != cq_core::model::digest(descriptor.key.unit.as_bytes())
                {
                    return Err(Error::new(
                        "InvalidScopeMembership",
                        "scope identity mismatch",
                    ));
                }
                let root: tree::Root = serde_json::from_value(value["members"].clone())?;
                let old = self.value(&base, key)?;
                if let Some(old) = old
                    && old["members"] == value["members"]
                    && old["state_base"] == value["state_base"]
                {
                    let previous: ScopeDescriptor =
                        serde_json::from_value(old["descriptor"].clone())?;
                    if descriptor.digest != previous.digest || descriptor.rows != previous.rows {
                        return Err(Error::new(
                            "InvalidScopeMembership",
                            "inherited root differs from completion descriptor",
                        ));
                    }
                } else {
                    let mut refs = BTreeMap::new();
                    tree::visit(&self.objects, &root, |member, v| {
                        scope_members += 1;
                        let (family, _) = member
                            .split_once(':')
                            .ok_or_else(|| Error::new("InvalidScopeMembership", "member key"))?;
                        if !semantic(family)
                            || family != family_name(&descriptor.key.family)
                            || tree::get(
                                &self.objects,
                                &r.roots.facts,
                                member,
                                &mut ReadStats::default(),
                            )?
                            .as_ref()
                                != Some(v)
                        {
                            return Err(Error::new("InvalidScopeMembership", "foreign member"));
                        }
                        let f: FactRef = serde_json::from_value(v.clone())?;
                        let row = writer::decode(&self.objects, &f, &mut ReadStats::default())?;
                        if self.scope_key(r, family, &row)? != descriptor.key {
                            return Err(Error::new(
                                "InvalidScopeMembership",
                                "scope owner mismatch",
                            ));
                        }
                        refs.insert(
                            member.into(),
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
                    if descriptor.rows != refs.len() as u64
                        || descriptor.digest != transfer::state_digest(&refs)?
                    {
                        return Err(Error::new(
                            "InvalidScopeMembership",
                            "completion digest/count mismatch",
                        ));
                    }
                }
                if descriptor.completion != Completion::Complete
                    && (root.is_some() || r.metadata["production"]["partial"] != true)
                {
                    return Err(Error::new(
                        "InvalidScopeMembership",
                        "failed scope carries facts",
                    ));
                }
            }
        }
        Ok(
            json!({"mode":"incremental_merkle","facts_checked":facts.len(),"groups_checked":group_count,"scope_members_checked":scope_members,"manifest_reads":reads,"parent":base.revision,"inherited_immutable_roots":true}),
        )
    }
}
