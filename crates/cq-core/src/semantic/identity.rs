//! Persistent declaration handles are distinct from compilation/run instances.
use crate::{Result, model::*};
use std::collections::{HashMap, HashSet};
const OBSERVATION_SCOPED: &str =
    "repeated compiler observations share a configuration; context identity is observation-scoped";

/// A nonpersistent declaration remains addressable in its immutable semantic
/// instance. Deriving this alias at read time preserves reusable payload bytes;
/// it never upgrades an anonymous/positional declaration to a persistent id.
pub fn query_handle(entity: &Entity) -> Option<String> {
    entity.stable_handle.clone().or_else(|| {
        (entity.id.len() == 64
            && entity
                .id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && !matches!(
                entity.kind,
                EntityKind::Project | EntityKind::CompilationUnit
            ))
        .then(|| format!("cqg1:{}", entity.id))
    })
}

// A nested item can have a moniker while still being scoped to a callable.
// Use compiler body ranges, never a same-name heuristic, to fence these items.
type RangePrefixes = (Vec<(u64, u64, String)>, Vec<Vec<(u64, String)>>);
struct LocalRanges {
    lookup: HashMap<(String, String), RangePrefixes>,
}
impl LocalRanges {
    fn new<'a>(bodies: impl Iterator<Item = &'a Body>) -> Self {
        let mut files = HashMap::<(String, String), Vec<(u64, u64, String)>>::new();
        for b in bodies {
            files
                .entry((b.unit.clone(), b.source.file_revision.clone()))
                .or_default()
                .push((b.source.start, b.source.end, b.owner.clone()));
        }
        let mut lookup = HashMap::new();
        for (key, mut spans) in files {
            spans.sort_by_key(|s| s.0);
            let mut best = Vec::<(u64, String)>::new();
            let mut prefixes = vec![];
            for (_, end, owner) in &spans {
                if let Some(v) = best.iter_mut().find(|(_, id)| id == owner) {
                    v.0 = v.0.max(*end);
                } else {
                    best.push((*end, owner.clone()));
                }
                best.sort_by_key(|a| std::cmp::Reverse(a.0));
                best.truncate(2);
                prefixes.push(best.clone());
            }
            lookup.insert(key, (spans, prefixes));
        }
        Self { lookup }
    }
    fn local(&self, e: &Entity) -> bool {
        if matches!(
            e.kind,
            EntityKind::File | EntityKind::Project | EntityKind::CompilationUnit
        ) {
            return false;
        }
        e.source
            .as_ref()
            .and_then(|a| {
                self.lookup
                    .get(&(e.unit.clone(), a.file_revision.clone()))
                    .map(|(spans, prefixes)| {
                        let at = spans.partition_point(|s| s.0 <= a.start);
                        at > 0
                            && prefixes[at - 1]
                                .iter()
                                .any(|(end, owner)| owner != &e.id && *end >= a.end)
                    })
            })
            .unwrap_or(false)
    }
}
fn local_items(snapshot: &SemanticSnapshot) -> HashSet<String> {
    let ranges = LocalRanges::new(snapshot.bodies.iter());
    snapshot
        .entities
        .iter()
        .filter(|e| ranges.local(e))
        .map(|e| e.id.clone())
        .collect()
}

fn logical(s: &str, root: &str) -> String {
    s.replace(root, "$PROJECT")
}
fn strings(v: &[NativeValue], root: &str) -> Vec<String> {
    v.iter().map(|v| logical(&v.display(), root)).collect()
}
pub fn unit_key(unit: &BuildUnit, root: &str) -> Result<String> {
    let configuration = match &unit.configuration {
        ConfigurationObservation::Rust(c) => {
            serde_json::json!({"cfg":c.explicit_cfg,"compiler_cfg":c.compiler_cfg,"test":c.test,"edition":c.edition,"options":rust_options(&c.options,root)})
        }
        ConfigurationObservation::Native(c) => {
            // Go compiler args contain temporary export/importcfg paths. Selection flags
            // and environment are recorded separately by its analyst below.
            let args = if unit.language == Language::Go {
                c.environment
                    .get("CQ_SELECTION")
                    .cloned()
                    .unwrap_or_default()
            } else {
                serde_json::to_string(&strings(&c.arguments, root))?
            };
            serde_json::json!({"args":logical(&args,root),"working_directory":logical(&c.working_directory.display(),root),"environment":c.environment.iter().filter(|(k,_)|k.as_str()!="GOMOD"&&k.as_str()!="GOWORK").collect::<std::collections::BTreeMap<_,_>>()})
        }
    };
    Ok(digest(&serde_json::to_vec(&(
        unit.language,
        unit.name.value().map(|n| logical(n, root)),
        if unit.language == Language::Go {
            None
        } else {
            unit.root.value().map(|p| logical(&p.display(), root))
        },
        unit.role,
        &unit.host,
        &unit.target,
        configuration,
    ))?))
}
pub struct CompilationIdentities {
    pub units: HashMap<String, String>,
    pub observation_scoped: HashSet<String>,
}
impl CompilationIdentities {
    pub(super) fn report(&self, snapshot: &mut SemanticSnapshot) {
        if !self.observation_scoped.is_empty()
            && !snapshot
                .gaps
                .iter()
                .any(|g| g.code == "ObservationScopedCompilationIdentity")
        {
            snapshot.gaps.push(Gap::new("ObservationScopedCompilationIdentity", "compilation contexts", format!("{} repeated compiler observations retained as separate observation-scoped contexts; cross-revision identity is not claimed for these instances", self.observation_scoped.len())));
        }
    }
    pub(super) fn mark(&self, entity: &mut Entity) {
        if self.observation_scoped.contains(&entity.unit) {
            entity.stable_handle = None;
            entity
                .attributes
                .insert("identity_scope_reason".into(), OBSERVATION_SCOPED.into());
        }
    }
}
/// Configuration identity and execution observation are distinct. Repeated
/// observations (such as build-script probes) must neither collapse nor abort
/// the entire repository. Keep ambiguous instances generation-scoped.
pub fn compilation_identities(units: &[BuildUnit], root: &str) -> Result<CompilationIdentities> {
    let mut result = CompilationIdentities {
        units: HashMap::new(),
        observation_scoped: HashSet::new(),
    };
    let mut groups = HashMap::<String, Vec<&str>>::new();
    for unit in units {
        let key = unit_key(unit, root)?;
        if result.units.insert(unit.id.clone(), key.clone()).is_some() {
            return Err(crate::Error::new(
                "DuplicateCompilationObservation",
                &unit.id,
            ));
        }
        groups.entry(key).or_default().push(&unit.id);
    }
    for (key, observations) in groups.into_iter().filter(|(_, ids)| ids.len() > 1) {
        for id in observations {
            let observation = id
                .strip_prefix("cqo1:")
                .and_then(|rest| rest.split_once(':'))
                .filter(|(config, observed)| {
                    config.len() == 64
                        && observed.len() == 64
                        && config
                            .bytes()
                            .chain(observed.bytes())
                            .all(|b| b.is_ascii_hexdigit())
                })
                .map(|(_, observed)| observed.to_owned())
                .unwrap_or_else(|| digest(id.as_bytes()));
            result
                .units
                .insert(id.into(), format!("cqo1:{key}:{observation}"));
            result.observation_scoped.insert(id.into());
        }
    }
    Ok(result)
}
fn rust_options(options: &[NativeValue], root: &str) -> Vec<String> {
    let options = strings(options, root);
    let mut out = vec![];
    let mut skip = false;
    for option in options {
        if skip {
            skip = false;
            continue;
        }
        if matches!(option.as_str(), "--out-dir" | "--emit" | "--extern" | "-L") {
            skip = true;
            continue;
        }
        if option.starts_with("metadata=")
            || option.starts_with("extra-filename=")
            || option.starts_with("incremental=")
        {
            if out.last().is_some_and(|s| s == "-C") {
                out.pop();
            }
            continue;
        }
        out.push(option);
    }
    out
}
pub(super) fn rust_symbol(key: &str) -> String {
    // The producer's length-prefixed crate-context digest is intentionally
    // generation-dependent for our immutable replay views. All following native
    // structural identity fields remain intact, including impl/trait owners.
    if let Some(rest) = key.strip_prefix("rust-hir-v1|13:crate-context=64:") {
        return format!("rust-hir-v1{}", rest.get(64..).unwrap_or(rest));
    }
    key.into()
}
pub fn assign(snapshot: &mut SemanticSnapshot) -> Result<()> {
    let root = snapshot.root.display();
    let contexts = compilation_identities(&snapshot.units, &root)?;
    contexts.report(snapshot);
    let units = &contexts.units;
    let files: HashMap<_, _> = snapshot
        .sources
        .files
        .iter()
        .map(|f| (f.id.clone(), logical(&f.logical_path.display(), &root)))
        .collect();
    let locals = local_items(snapshot);
    for entity in &mut snapshot.entities {
        contexts.mark(entity);
        let local = locals.contains(&entity.id);
        assign_one(entity, &root, units, &files, local)?;
    }
    // Native monikers can omit lexical owners or use anonymous placeholders.
    // Conflicting observations cannot acquire a persistent declaration handle.
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in snapshot.entities.iter().enumerate() {
        if let Some(h) = e.stable_handle.as_ref().filter(|h| h.starts_with("cq1:")) {
            groups.entry(h.clone()).or_default().push(i);
        }
    }
    let mut conflicts = 0;
    for (handle, indices) in groups.into_iter().filter(|(_, v)| v.len() > 1) {
        let mut facts = HashSet::new();
        for i in &indices {
            let mut e = snapshot.entities[*i].clone();
            e.id.clear();
            e.stable_handle = None;
            e.attributes.remove("native_token");
            facts.insert(serde_json::to_vec(&e)?);
        }
        if facts.len() > 1 {
            conflicts += indices.len();
            for i in indices {
                let e = &mut snapshot.entities[i];
                e.stable_handle = Some(format!(
                    "cqg1:{}",
                    digest(&serde_json::to_vec(&(&handle, &e.id))?)
                ));
                e.attributes.insert(
                    "identity_scope_reason".into(),
                    "conflicting native declaration observations".into(),
                );
            }
        }
    }
    if conflicts > 0 {
        snapshot.gaps.push(Gap::new("GenerationScopedIdentity","native declaration handles",format!("{conflicts} conflicting native observations keep separate generation-scoped identities")));
    }
    Ok(())
}

pub(super) fn assign_one(
    entity: &mut Entity,
    root: &str,
    units: &HashMap<String, String>,
    files: &HashMap<String, String>,
    local: bool,
) -> Result<()> {
    let persistent = !entity.name.is_empty()
        && !entity.attributes.contains_key("identity_scope_reason")
        && !entity.name.starts_with('<')
        && !local
        && !entity.attributes.contains_key("native_local")
        && !matches!(
            entity.kind,
            EntityKind::Variable
                | EntityKind::Parameter
                | EntityKind::Body
                | EntityKind::Document
                | EntityKind::DocSection
                | EntityKind::Impl
        )
        && entity.name != "init";
    let symbol = if matches!(entity.kind, EntityKind::File) {
        String::new()
    } else {
        rust_symbol(&logical(&entity.provider_id, root))
    };
    let context = units
        .get(&entity.unit)
        .cloned()
        .unwrap_or_else(|| entity.unit.clone());
    let file = entity
        .source
        .as_ref()
        .and_then(|a| files.get(&a.file_revision));
    let file = if persistent && entity.kind != EntityKind::File {
        None
    } else {
        file
    };
    let symbol = if entity.kind == EntityKind::CompilationUnit {
        context.clone()
    } else {
        symbol
    };
    let key = serde_json::to_vec(&(
        1,
        &context,
        entity.kind,
        symbol,
        file,
        entity.attributes.get("definition_kind"),
    ))?;
    entity.stable_handle = Some(if persistent {
        format!("cq1:{}", digest(&key))
    } else {
        format!("cqg1:{}", digest(&serde_json::to_vec(&(&key, &entity.id))?))
    });
    Ok(())
}

/// Only declaration handles, fingerprints and body intervals are retained.
pub(super) struct Handles {
    values: HashMap<String, (String, Option<&'static str>)>,
}
impl Handles {
    pub fn new(header: &SemanticSnapshot, source: &dyn super::records::Source) -> Result<Self> {
        use super::records::Record;
        let root = header.root.display();
        let contexts = compilation_identities(&header.units, &root)?;
        let units = &contexts.units;
        let files = header
            .sources
            .files
            .iter()
            .map(|f| (f.id.clone(), logical(&f.logical_path.display(), &root)))
            .collect::<HashMap<_, _>>();
        let mut bodies = vec![];
        source.visit_family("body", &mut |r| {
            if let Record::Body(b) = r {
                bodies.push(b);
            }
            Ok(())
        })?;
        let ranges = LocalRanges::new(bodies.iter());
        drop(bodies);
        let mut values = HashMap::new();
        let mut groups = HashMap::<String, Vec<(String, String)>>::new();
        source.visit_family("entity", &mut |r| {
            if let Record::Entity(mut e) = r {
                contexts.mark(&mut e);
                let local = ranges.local(&e);
                assign_one(&mut e, &root, units, &files, local)?;
                let handle = e.stable_handle.clone().unwrap();
                values.insert(
                    e.id.clone(),
                    (
                        handle.clone(),
                        contexts
                            .observation_scoped
                            .contains(&e.unit)
                            .then_some(OBSERVATION_SCOPED),
                    ),
                );
                if handle.starts_with("cq1:") {
                    let id = e.id.clone();
                    e.id.clear();
                    e.stable_handle = None;
                    e.attributes.remove("native_token");
                    groups
                        .entry(handle)
                        .or_default()
                        .push((id, digest(&serde_json::to_vec(&e)?)));
                }
            }
            Ok(())
        })?;
        for (handle, items) in groups {
            if items.iter().map(|(_, h)| h).collect::<HashSet<_>>().len() > 1 {
                for (id, _) in items {
                    values.insert(
                        id.clone(),
                        (
                            format!("cqg1:{}", digest(&serde_json::to_vec(&(&handle, &id))?)),
                            Some("conflicting native declaration observations"),
                        ),
                    );
                }
            }
        }
        Ok(Self { values })
    }
    pub fn apply(&self, e: &mut Entity) {
        if let Some((h, reason)) = self.values.get(&e.id) {
            e.stable_handle = Some(h.clone());
            if let Some(reason) = reason {
                e.attributes
                    .insert("identity_scope_reason".into(), (*reason).into());
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn repeated_units() -> Vec<BuildUnit> {
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../fixtures/native/repeated-rust-observations.json"
        ))
        .unwrap();
        serde_json::from_value(raw["units"].clone()).unwrap()
    }
    #[test]
    fn real_repeated_probes_keep_six_distinct_observation_scoped_contexts() {
        let units = repeated_units();
        let keys = compilation_identities(&units, "/repo").unwrap();
        assert_eq!(keys.units.len(), 6);
        assert_eq!(keys.observation_scoped.len(), 6);
        assert_eq!(keys.units.values().collect::<HashSet<_>>().len(), 6);
        assert_eq!(
            units
                .iter()
                .map(|u| unit_key(u, "/repo").unwrap())
                .collect::<HashSet<_>>()
                .len(),
            3
        );
        let mut reversed = units.clone();
        reversed.reverse();
        assert_eq!(
            keys.units,
            compilation_identities(&reversed, "/repo").unwrap().units
        );
        let mut canonical = units.clone();
        for u in &mut canonical {
            u.id = keys.units[&u.id].clone();
        }
        let again = compilation_identities(&canonical, "/repo").unwrap();
        assert!(canonical.iter().all(|u| again.units[&u.id] == u.id));
        let mut duplicate = units.clone();
        duplicate.push(units[0].clone());
        assert_eq!(
            compilation_identities(&duplicate, "/repo")
                .err()
                .unwrap()
                .code(),
            "DuplicateCompilationObservation"
        );
    }
    #[test]
    fn ambiguous_context_handles_are_scoped_and_unambiguous_handles_stay_stable() {
        let units = repeated_units();
        let mut second = units.clone();
        for u in &mut second {
            u.id.push_str("-next-observation");
        }
        let a = compilation_identities(&units, "/repo").unwrap();
        let b = compilation_identities(&second, "/repo").unwrap();
        assert!(
            a.units
                .values()
                .all(|key| !b.units.values().any(|v| v == key))
        );
        let single = compilation_identities(&units[..1], "/repo").unwrap();
        assert!(single.observation_scoped.is_empty());
        assert_eq!(
            single.units[&units[0].id],
            unit_key(&units[0], "/repo").unwrap()
        );
    }
    #[test]
    fn strips_only_the_native_crate_context_coordinate() {
        let key = format!(
            "rust-hir-v1|13:crate-context=64:{}|7:package=4:demo|4:name=3:run",
            "a".repeat(64)
        );
        assert_eq!(
            rust_symbol(&key),
            "rust-hir-v1|7:package=4:demo|4:name=3:run"
        );
        assert_eq!(rust_symbol("c:@F@run#"), "c:@F@run#");
    }
    fn entity(id: &str, name: &str, start: u64, end: u64) -> Entity {
        Entity {
            id: id.into(),
            stable_handle: None,
            locations: Default::default(),
            unit: "u".into(),
            provider_id: format!("native::{name}"),
            kind: EntityKind::Function,
            name: name.into(),
            qualified_name: name.into(),
            source: Some(SourceAnchor {
                file_revision: "file".into(),
                start,
                end,
            }),
            extent: None,
            signature: "fn()".into(),
            parent: None,
            external: false,
            exported: false,
            attributes: Default::default(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: ProvenanceId("fixture".into()),
        }
    }
    #[test]
    fn callable_scoped_items_and_anonymous_builtins_never_claim_persistent_identity() {
        let mut s = SemanticSnapshot::empty("fixture".into(), NativeValue::from("/root"));
        s.entities = vec![
            entity("outer", "outer", 0, 5),
            entity("nested", "nested", 20, 26),
            entity("global", "global", 101, 107),
            entity("attribute", "", 0, 0),
        ];
        s.entities[3].source = None;
        s.bodies.push(Body {
            id: "body".into(),
            owner: "outer".into(),
            unit: "u".into(),
            parent: None,
            source: SourceAnchor {
                file_revision: "file".into(),
                start: 0,
                end: 100,
            },
            kind: BodyKind::Function,
            evaluation: Evaluation::Immediate,
        });
        assign(&mut s).unwrap();
        for (e, prefix) in s.entities.iter().zip(["cq1:", "cqg1:", "cq1:", "cqg1:"]) {
            assert!(
                e.stable_handle.as_ref().unwrap().starts_with(prefix),
                "{}",
                e.name
            );
        }
    }
    #[test]
    fn conflicting_monikers_are_fenced_but_identical_observations_keep_their_handle() {
        for conflict in [false, true] {
            let mut s = SemanticSnapshot::empty("fixture".into(), NativeValue::from("/root"));
            let a = entity("a", "same", 0, 4);
            let mut b = a.clone();
            b.id = "b".into();
            b.attributes.insert("native_token".into(), "2".into());
            if conflict {
                b.source.as_mut().unwrap().start = 1;
            }
            s.entities = vec![a, b];
            assign(&mut s).unwrap();
            if conflict {
                assert!(
                    s.entities.iter().all(|e| e
                        .stable_handle
                        .as_ref()
                        .unwrap()
                        .starts_with("cqg1:"))
                );
                assert_ne!(s.entities[0].stable_handle, s.entities[1].stable_handle);
            } else {
                assert_eq!(s.entities[0].stable_handle, s.entities[1].stable_handle);
                assert!(
                    s.entities[0]
                        .stable_handle
                        .as_ref()
                        .unwrap()
                        .starts_with("cq1:")
                );
            }
        }
    }
}
