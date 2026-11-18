use super::ranges::RangeIndex;
use crate::{Error, Result, model::*, ports::ContentRepository};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) fn native_relation_target_id(
    unit: &str,
    key: &str,
    source: &Option<SourceAnchor>,
) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(
        unit,
        key,
        source,
        "native-relation-target",
    ))?))
}

fn s(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}
pub(super) fn n(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or_default()
}
pub(super) fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn kind(name: &str) -> EntityKind {
    match name.to_ascii_lowercase().as_str() {
        "function" => EntityKind::Function,
        "method" | "staticmethod" | "traitmethod" => EntityKind::Method,
        "module" | "namespace" => EntityKind::Module,
        "struct" => EntityKind::Struct,
        "enum" => EntityKind::Enum,
        "enummember" | "variant" => EntityKind::Variant,
        "union" => EntityKind::Union,
        "trait" => EntityKind::Trait,
        "interface" => EntityKind::Interface,
        "class" => EntityKind::Class,
        "typealias" => EntityKind::TypeAlias,
        "type" | "typeparameter" | "associatedtype" => EntityKind::Type,
        "field" | "property" => EntityKind::Field,
        "parameter" | "valueparameter" | "selfparameter" => EntityKind::Parameter,
        "variable" | "local" => EntityKind::Variable,
        "constant" | "static" | "staticvariable" => EntityKind::Constant,
        "macro" | "attribute" => EntityKind::Macro,
        "impl" => EntityKind::Impl,
        _ => EntityKind::Symbol,
    }
}
pub(super) fn anchor(
    span: &Value,
    files: &HashMap<u64, (FileRevision, String)>,
) -> Option<SourceAnchor> {
    let (f, _) = files.get(&span["file"].as_u64()?)?;
    let start = span["start"].as_u64()?;
    let end = span["end"].as_u64()?;
    if start > end || end > f.blob.len {
        return None;
    }
    Some(SourceAnchor {
        file_revision: f.id.clone(),
        start,
        end,
    })
}
fn locations(raw: &Value, files: &HashMap<u64, (FileRevision, String)>) -> SourceLocations {
    SourceLocations {
        declaration: anchor(&raw["declaration"], files),
        definition: anchor(&raw["definition"], files),
        spelling: anchor(&raw["spelling"], files),
        expansion: anchor(&raw["expansion"], files),
    }
}
fn containment(a: &SourceAnchor, b: &SourceAnchor) -> bool {
    a.file_revision == b.file_revision && a.start <= b.start && a.end >= b.end
}
pub fn rust_hir(
    snapshot: &mut SemanticSnapshot,
    raw: &Value,
    mapping: &[MaterializedInput],
    content: &dyn ContentRepository,
    version: &str,
) -> Result<()> {
    native_graph(
        snapshot,
        raw,
        mapping,
        content,
        "rust-analyzer-hir",
        Language::Rust,
        version,
    )
}
pub fn native_graph(
    snapshot: &mut SemanticSnapshot,
    raw: &Value,
    mapping: &[MaterializedInput],
    content: &dyn ContentRepository,
    provider: &str,
    language: Language,
    version: &str,
) -> Result<()> {
    super::provider_contract::validate(raw)?;
    let prov=Provenance{provider:provider.into(),version:version.into(),authority:Authority::SemanticResolution,build_run:snapshot.builds.last().map(|b|b.id.clone()),note:"native semantic resolution in captured compilation contexts; executable dynamic targets may remain unknown".into()};
    let prov_id = snapshot.intern_provenance(prov.clone());
    let syntax_id = snapshot.intern_provenance(Provenance {
        authority: Authority::RuleDerived,
        ..prov.clone()
    });
    let documentation_id=snapshot.intern_provenance(Provenance{authority:Authority::AuthorStatement,note:"documentation exported for this native declaration; not a generated explanation of author motive".into(),..prov.clone()});
    let comment_id = snapshot.intern_provenance(Provenance {
        authority: Authority::AuthorStatement,
        note: "native lexer comment; owner indicates lexical scope, not inferred author motivation"
            .into(),
        ..prov.clone()
    });
    let expected: HashMap<_, _> = mapping
        .iter()
        .map(|m| ((m.path.as_str(), m.unit.as_str()), m))
        .collect();
    let by_path: HashMap<_, _> = mapping.iter().map(|m| (m.path.as_str(), m)).collect();
    let mut files = HashMap::new();
    let mut known: HashMap<_, _> = snapshot
        .sources
        .files
        .iter()
        .map(|f| (f.logical_path.clone(), f.clone()))
        .collect();
    for file in list(raw, "files") {
        let path = s(file, "path");
        let text = s(file, "text");
        let blob = content.put(text.as_bytes(), 1024 * 1024 * 1024)?;
        let selected = if let Some(unit) = file["unit"].as_str() {
            Some(*expected.get(&(path.as_str(), unit)).ok_or_else(|| {
                Error::new(
                    "InvalidProviderOutput",
                    "file compilation unit was not selected/captured",
                )
            })?)
        } else {
            by_path.get(path.as_str()).copied()
        };
        let (revision, unit) = if let Some(m) = selected {
            if blob != m.file.blob {
                return Err(Error::new(
                    "ProducerInputMismatch",
                    format!("HIR loaded different bytes: {path}"),
                ));
            }
            (m.file.clone(), m.unit.clone())
        } else {
            let logical_path = NativeValue::from(path.as_str());
            if let Some(old) = known.get(&logical_path) {
                if old.blob != blob {
                    return Err(Error::new(
                        "ProducerInputMismatch",
                        format!("uncaptured live input differed: {path}"),
                    ));
                }
                (old.clone(), "external".into())
            } else {
                let id = digest(&serde_json::to_vec(&(&logical_path, &blob))?);
                let f = FileRevision {
                    id,
                    logical_path,
                    blob,
                    origin: "hir_actual_input_external".into(),
                };
                known.insert(f.logical_path.clone(), f.clone());
                (f, "external".into())
            }
        };
        files.insert(n(file, "id"), (revision, unit));
    }
    for m in mapping {
        known.insert(m.file.logical_path.clone(), m.file.clone());
    }
    snapshot.sources = SourceSnapshot::seal(
        known.into_values().collect(),
        snapshot.sources.binding.clone(),
        snapshot.sources.gaps.clone(),
    );
    let external_files: std::collections::HashSet<_> = snapshot
        .sources
        .files
        .iter()
        .filter(|f| !f.logical_path.path().starts_with(snapshot.root.path()))
        .map(|f| f.id.clone())
        .collect();
    let mut file_entities = HashMap::new();
    for (file, unit) in files.values() {
        let id = digest(&serde_json::to_vec(&(unit, &file.id, "file"))?);
        if file_entities
            .insert((unit.clone(), file.id.clone()), id.clone())
            .is_none()
        {
            snapshot.entities.push(Entity {
                stable_handle: None,
                locations: Default::default(),
                id,
                unit: unit.clone(),
                provider_id: file.id.clone(),
                kind: EntityKind::File,
                name: file
                    .logical_path
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                qualified_name: file.logical_path.display(),
                source: Some(SourceAnchor {
                    file_revision: file.id.clone(),
                    start: 0,
                    end: file.blob.len,
                }),
                extent: None,
                signature: String::new(),
                parent: None,
                external: unit == "external" || external_files.contains(&file.id),
                exported: false,
                attributes: BTreeMap::new(),
                type_definitions: vec![],
                type_navigation: vec![],
                provenance: prov_id.clone(),
            });
        }
    }
    let mut tokens = HashMap::new();
    let mut semantic_keys: HashMap<(String, String), Vec<String>> = HashMap::new();
    for symbol in list(raw, "symbols") {
        let source = anchor(&symbol["definition"], &files);
        let extent = anchor(&symbol["body"], &files);
        let unit = symbol["definition"]["file"]
            .as_u64()
            .and_then(|i| files.get(&i))
            .map(|(_, u)| u.clone())
            .unwrap_or_else(|| raw["unit"].as_str().unwrap_or("external").to_owned());
        let key = s(symbol, "key");
        let id = digest(&serde_json::to_vec(&(
            &unit,
            &key,
            &source,
            n(symbol, "token"),
        ))?);
        tokens.insert(n(symbol, "token"), id.clone());
        semantic_keys
            .entry((unit.clone(), key.clone()))
            .or_default()
            .push(id.clone());
        let mut attributes = BTreeMap::from([
            ("native_token".into(), n(symbol, "token").to_string()),
            ("native_kind".into(), s(symbol, "kind")),
        ]);
        if symbol["local"] == true {
            attributes.insert("native_local".into(), "true".into());
        }
        if let Some(kind) = symbol["definition_kind"].as_str() {
            attributes.insert("definition_kind".into(), kind.into());
        }
        if symbol["is_definition"] == false {
            attributes.insert("declaration_only".into(), "true".into());
        }
        if symbol["trait_member"] == true {
            attributes.insert("trait_member".into(), "true".into());
        }
        if symbol["const_function"] == true {
            attributes.insert("const_function".into(), "true".into());
        }
        let external = symbol["external"] == true
            || source
                .as_ref()
                .is_some_and(|a| external_files.contains(&a.file_revision));
        snapshot.entities.push(Entity {
            stable_handle: None,
            locations: locations(&symbol["locations"], &files),
            id,
            unit,
            provider_id: key,
            kind: kind(&s(symbol, "kind")),
            name: s(symbol, "name"),
            qualified_name: s(symbol, "qualified_name"),
            source,
            extent,
            signature: s(symbol, "signature"),
            parent: None,
            external,
            exported: symbol["exported"] == true,
            attributes,
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: prov_id.clone(),
        });
    }
    let unmapped: std::collections::BTreeSet<_> = list(raw, "symbols")
        .iter()
        .map(|v| s(v, "kind"))
        .filter(|native| {
            kind(native) == EntityKind::Symbol && !native.eq_ignore_ascii_case("symbol")
        })
        .collect();
    for native in unmapped {
        snapshot.gaps.push(Gap::new("NativeSymbolKindUnmapped",provider,format!("native kind {native} retained as Symbol; callable/type interpretation is incomplete")));
    }
    let mut entity_index: HashMap<String, usize> = snapshot
        .entities
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.clone(), i))
        .collect();
    for parent in list(raw, "parents") {
        let Some(child) = tokens.get(&n(parent, "child")).cloned() else {
            continue;
        };
        let Some(&index) = entity_index.get(&child) else {
            continue;
        };
        let unit = snapshot.entities[index].unit.clone();
        let key = s(parent, "key");
        let source = anchor(&parent["source"], &files);
        let candidates = semantic_keys
            .get(&(unit.clone(), key.clone()))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|id| source.is_none() || snapshot.entities[entity_index[id]].source == source)
            .collect::<Vec<_>>();
        let native_parent = parent["parent_token"]
            .as_u64()
            .and_then(|token| tokens.get(&token));
        let target = if let Some(id) = native_parent {
            id.clone()
        } else if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            let id = digest(&serde_json::to_vec(&(&unit, &key, &source))?);
            if !entity_index.contains_key(&id) {
                entity_index.insert(id.clone(), snapshot.entities.len());
                snapshot.entities.push(Entity {
                    stable_handle: None,
                    locations: Default::default(),
                    id: id.clone(),
                    unit: unit.clone(),
                    provider_id: key.clone(),
                    kind: kind(&s(parent, "kind")),
                    name: s(parent, "name"),
                    qualified_name: String::new(),
                    source: source.clone(),
                    extent: None,
                    signature: String::new(),
                    parent: None,
                    external: unit == "external"
                        || source
                            .as_ref()
                            .is_some_and(|a| external_files.contains(&a.file_revision)),
                    exported: false,
                    attributes: BTreeMap::new(),
                    type_definitions: vec![],
                    type_navigation: vec![],
                    provenance: prov_id.clone(),
                });
            }
            id
        };
        if target != child {
            snapshot.entities[index].parent = Some(target.clone());
            snapshot.add_relation(
                &target,
                &child,
                RelationKind::Contains,
                source,
                None,
                prov.clone(),
            );
        }
    }
    // Native HIR impl self types provide a nominal membership projection without
    // discarding the actual declaring impl scope kept in Entity.parent.
    for membership in list(raw, "nominal_members") {
        if let (Some(child), Some(owner)) = (
            tokens.get(&n(membership, "child")),
            tokens.get(&n(membership, "owner")),
        ) {
            snapshot.add_relation(owner,child,RelationKind::HasMember,snapshot.entities[entity_index[child]].source.clone(),None,Provenance{note:"native HIR impl self type resolves to this nominal ADT; declaring impl scope remains separate".into(),..prov.clone()});
        }
    }
    // Actual AST body boundaries are retained. A closure is not attributed to
    // the nearest named function; it owns its occurrences as a separate body.
    let mut function_ends: HashMap<(String, String, u64), Vec<usize>> = HashMap::new();
    for (i, e) in snapshot.entities.iter().enumerate() {
        if matches!(e.kind, EntityKind::Function | EntityKind::Method)
            && let Some(a) = &e.extent
        {
            function_ends
                .entry((e.unit.clone(), a.file_revision.clone(), a.end))
                .or_default()
                .push(i);
        }
    }
    let mut inactive = vec![];
    let mut bodies = vec![];
    for body in list(raw, "bodies")
        .iter()
        .filter(|b| b["kind"] == "function")
    {
        let Some(source) = anchor(&body["source"], &files) else {
            continue;
        };
        let unit = files
            .get(&n(&body["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let owner = function_ends
            .get(&(unit.clone(), source.file_revision.clone(), source.end))
            .into_iter()
            .flatten()
            .map(|i| &snapshot.entities[*i])
            .find(|e| e.source.as_ref().is_some_and(|a| containment(&source, a)))
            .map(|e| e.id.clone());
        if let Some(owner) = owner {
            if body["test"] == true
                && let Some(e) = entity_index.get(&owner).map(|i| &mut snapshot.entities[*i])
            {
                e.attributes.insert("test".into(), "true".into());
            }
            let entity = &snapshot.entities[entity_index[&owner]];
            let kind = if body["generated"] == true {
                BodyKind::Generated
            } else if body["async"] == true {
                BodyKind::AsyncFunction
            } else {
                BodyKind::Function
            };
            bodies.push(Body {
                id: digest(&serde_json::to_vec(&(&entity.unit, &source, "body"))?),
                unit: entity.unit.clone(),
                owner,
                parent: None,
                kind,
                source: anchor(&body["block"], &files).unwrap_or(source),
                evaluation: if kind == BodyKind::Generated {
                    Evaluation::Unknown
                } else if kind == BodyKind::AsyncFunction {
                    Evaluation::Deferred
                } else {
                    Evaluation::Immediate
                },
            });
        } else {
            inactive.push((unit, source));
        }
    }
    for raw_body in list(raw, "bodies")
        .iter()
        .filter(|b| b["kind"] == "const_initializer" || b["kind"] == "static_initializer")
    {
        let (Some(source), Some(owner_source)) = (
            anchor(&raw_body["source"], &files),
            anchor(&raw_body["owner"], &files),
        ) else {
            continue;
        };
        let unit = files
            .get(&n(&raw_body["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        if let Some(owner) = snapshot.entities.iter().find(|e| {
            e.unit == unit
                && e.source.as_ref() == Some(&owner_source)
                && matches!(
                    e.kind,
                    EntityKind::Constant | EntityKind::Variable | EntityKind::Symbol
                )
        }) {
            bodies.push(Body {
                id: digest(&serde_json::to_vec(&(&unit, &source, "body"))?),
                unit,
                owner: owner.id.clone(),
                parent: None,
                kind: if raw_body["kind"] == "static_initializer" {
                    BodyKind::StaticInitializer
                } else {
                    BodyKind::ConstInitializer
                },
                source,
                evaluation: Evaluation::Constant,
            });
        }
    }
    for body in list(raw, "bodies").iter().filter(|b| {
        !["function", "const_initializer", "static_initializer"]
            .contains(&b["kind"].as_str().unwrap_or(""))
    }) {
        let Some(source) = anchor(&body["source"], &files) else {
            continue;
        };
        let unit = files
            .get(&n(&body["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        if inactive
            .iter()
            .any(|(u, i)| u == &unit && containment(i, &source))
        {
            continue;
        }
        let Some(parent) = bodies
            .iter()
            .filter(|b| b.unit == unit && containment(&b.source, &source))
            .min_by_key(|b| b.source.end - b.source.start)
            .cloned()
        else {
            continue;
        };
        let kind = match body["kind"].as_str() {
            Some("closure") => BodyKind::Closure,
            Some("async_block") => BodyKind::AsyncBlock,
            _ => BodyKind::ConstInitializer,
        };
        bodies.push(Body {
            id: digest(&serde_json::to_vec(&(&parent.unit, &source, "body"))?),
            unit: parent.unit,
            owner: parent.owner,
            parent: Some(parent.id),
            kind,
            source,
            evaluation: if kind == BodyKind::ConstInitializer {
                Evaluation::Constant
            } else {
                Evaluation::Deferred
            },
        });
    }
    for body in &bodies {
        snapshot.entities.push(Entity {
            stable_handle: None,
            locations: Default::default(),
            id: body.id.clone(),
            unit: body.unit.clone(),
            provider_id: body.id.clone(),
            kind: EntityKind::Body,
            name: format!("{:?}", body.kind),
            qualified_name: format!("{}::body@{}", body.owner, body.source.start),
            source: Some(body.source.clone()),
            extent: Some(body.source.clone()),
            signature: String::new(),
            parent: Some(body.owner.clone()),
            external: external_files.contains(&body.source.file_revision),
            exported: false,
            attributes: BTreeMap::from([("evaluation".into(), format!("{:?}", body.evaluation))]),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: prov_id.clone(),
        });
        snapshot.add_relation(
            &body.owner,
            &body.id,
            RelationKind::CreatesBody,
            Some(body.source.clone()),
            None,
            prov.clone(),
        );
    }
    entity_index = snapshot
        .entities
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.clone(), i))
        .collect();
    let body_index = RangeIndex::new(
        bodies
            .iter()
            .enumerate()
            .map(|(i, b)| (b.unit.clone(), b.source.clone(), i)),
    );
    let inactive_index = RangeIndex::new(
        inactive
            .iter()
            .enumerate()
            .map(|(i, (u, a))| (u.clone(), a.clone(), i)),
    );
    let call_index = RangeIndex::new(list(raw, "calls").iter().enumerate().filter_map(|(i, c)| {
        Some((
            files.get(&n(&c["callee"], "file"))?.1.clone(),
            anchor(&c["callee"], &files)?,
            i,
        ))
    }));
    let parameter_slots = super::parameters::bind(snapshot, raw, &files)?;
    let flow_provenance=snapshot.intern_provenance(Provenance{authority:Authority::RuleDerived,note:"syntactic argument position mapped to a native bound declaration; no SSA, alias or runtime-value proof".into(),..prov.clone()});
    let mut covered_calls = HashSet::new();
    for reference in list(raw, "references")
        .iter()
        .filter(|r| r["is_definition"] != true)
    {
        let Some(target) = tokens.get(&n(reference, "target")).cloned() else {
            continue;
        };
        let Some(source) = anchor(&reference["source"], &files) else {
            continue;
        };
        let unit = files
            .get(&n(&reference["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        if inactive_index.nearest(&unit, &source).is_some() {
            continue;
        }
        let unit = files
            .get(&n(&reference["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let entity = &snapshot.entities[entity_index[&target]];
        let body = body_index.nearest(&unit, &source).map(|i| &bodies[i]);
        let unit = files
            .get(&n(&reference["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_else(|| "external".into());
        let relation = match reference["kind"].as_str() {
            Some("Call") => RelationKind::Calls,
            Some("Type") => RelationKind::UsesType,
            Some("Access") => RelationKind::Accesses,
            Some("Import") => RelationKind::Imports,
            Some("Export") => RelationKind::Exports,
            Some("Instantiate") => RelationKind::Instantiates,
            Some("Decorate") => RelationKind::Decorates,
            _ => RelationKind::References,
        };
        let bound_call = reference["call_index"]
            .as_u64()
            .map(|i| i as usize)
            .or_else(|| call_index.nearest(&unit, &source));
        let call = bound_call.and_then(|i| list(raw, "calls").get(i));
        let arguments: Vec<SourceAnchor> = call
            .map(|c| {
                list(c, "arguments")
                    .iter()
                    .filter_map(|s| anchor(s, &files))
                    .collect()
            })
            .unwrap_or_default();
        let direct = matches!(
            entity.kind,
            EntityKind::Function | EntityKind::Method | EntityKind::Struct | EntityKind::Variant
        ) && !entity.attributes.contains_key("trait_member");
        let dispatch = if entity.attributes.contains_key("trait_member") {
            Dispatch::Trait
        } else if direct {
            Dispatch::Direct
        } else if entity.kind == EntityKind::Macro {
            Dispatch::Macro
        } else {
            Dispatch::Indirect
        };
        let native_locations = locations(&reference["locations"], &files);
        let id = digest(&serde_json::to_vec(&(
            &unit,
            &source,
            relation,
            &target,
            &arguments,
            &native_locations,
        ))?);
        let owner = body.map(|b| b.id.clone());
        let executable = if relation == RelationKind::Calls {
            if direct && !entity.attributes.contains_key("declaration_only") {
                Resolution::Resolved(target.clone())
            } else {
                Resolution::Unknown(
                    "declaration is bound; concrete executable target is not determined".into(),
                )
            }
        } else {
            Resolution::NotApplicable
        };
        if relation == RelationKind::Calls
            && let Some(i) = bound_call
        {
            covered_calls.insert(i);
        }
        let argument_bindings = if relation == RelationKind::Calls {
            super::parameters::arguments(
                &arguments,
                &target,
                call.is_some_and(|c| c["method"] == true),
                &parameter_slots,
            )
        } else {
            vec![]
        };
        snapshot.occurrences.push(Occurrence {
            locations: native_locations,
            id: id.clone(),
            unit: unit.clone(),
            owner: owner.clone(),
            source: source.clone(),
            kind: relation,
            declaration: Resolution::Resolved(target.clone()),
            executable: executable.clone(),
            dispatch,
            evaluation: body.map(|b| b.evaluation).unwrap_or(Evaluation::Unknown),
            arguments,
            argument_bindings,
            provenance: prov_id.clone(),
        });
        if let Some(body) = body {
            let from = if matches!(body.kind, BodyKind::Function | BodyKind::AsyncFunction) {
                body.owner.as_str()
            } else {
                body.id.as_str()
            };
            let edge_kind = if relation == RelationKind::Calls
                && executable.resolved().is_none()
                && dispatch != Dispatch::Direct
            {
                RelationKind::Dispatches
            } else {
                relation
            };
            snapshot.add_relation(
                from,
                &target,
                edge_kind,
                Some(source),
                Some(id),
                prov.clone(),
            );
        } else if let Some(from) = file_entities.get(&(unit.clone(), source.file_revision.clone()))
        {
            snapshot.add_relation(
                from,
                &target,
                relation,
                Some(source),
                Some(id),
                prov.clone(),
            );
        }
    }
    for (call_number, call) in list(raw, "calls").iter().enumerate() {
        let Some(source) = anchor(&call["callee"], &files) else {
            continue;
        };
        let unit = files
            .get(&n(&call["callee"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        if inactive_index.nearest(&unit, &source).is_some() || covered_calls.contains(&call_number)
        {
            continue;
        }
        let Some(body) = body_index.nearest(&unit, &source).map(|i| &bodies[i]) else {
            continue;
        };
        let arguments: Vec<_> = list(call, "arguments")
            .iter()
            .filter_map(|s| anchor(s, &files))
            .collect();
        let id = digest(&serde_json::to_vec(&(
            &body.unit,
            &source,
            "unresolved-call",
            &arguments,
            &call["source"],
        ))?);
        snapshot.occurrences.push(Occurrence {
            locations: Default::default(),
            id,
            unit: body.unit.clone(),
            owner: Some(body.id.clone()),
            source,
            kind: RelationKind::Calls,
            declaration: Resolution::Unknown(
                "native syntax call has no resolved HIR target; cfg/expression coverage is partial"
                    .into(),
            ),
            executable: Resolution::Unknown("indirect or unresolved call".into()),
            dispatch: Dispatch::Unknown,
            evaluation: body.evaluation,
            argument_bindings: vec![],
            arguments,
            provenance: syntax_id.clone(),
        });
    }
    let argument_ranges =
        RangeIndex::new(snapshot.occurrences.iter().enumerate().flat_map(|(i, o)| {
            o.argument_bindings
                .iter()
                .map(move |binding| (o.unit.clone(), binding.source.clone(), i))
        }));
    let mut transfers = vec![];
    for occurrence in &snapshot.occurrences {
        if !matches!(
            occurrence.kind,
            RelationKind::References | RelationKind::Accesses | RelationKind::UsesType
        ) {
            continue;
        }
        let Some(source) = occurrence.declaration.resolved() else {
            continue;
        };
        let Some(call) = argument_ranges
            .nearest(&occurrence.unit, &occurrence.source)
            .map(|i| &snapshot.occurrences[i])
        else {
            continue;
        };
        for binding in &call.argument_bindings {
            if containment(&binding.source, &occurrence.source)
                && let Some(parameter) = binding.parameter.resolved()
            {
                transfers.push((
                    source.to_owned(),
                    parameter.to_owned(),
                    occurrence.source.clone(),
                    occurrence.id.clone(),
                ));
            }
        }
    }
    for (from, to, source, occurrence) in transfers {
        snapshot.add_relation(
            &from,
            &to,
            RelationKind::PassesValue,
            Some(source),
            Some(occurrence),
            flow_provenance.clone(),
        );
    }
    let mut all_keys: HashMap<&str, Vec<String>> = HashMap::new();
    for ((_, key), ids) in &semantic_keys {
        all_keys.entry(key).or_default().extend(ids.clone());
    }
    for relation in list(raw, "relations") {
        let unit = files
            .get(&n(relation, "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let from_candidates = semantic_keys
            .get(&(unit.clone(), s(relation, "from")))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|id| {
                relation["from_definition_kind"].as_str().is_none_or(|tag| {
                    snapshot.entities[entity_index[id]]
                        .attributes
                        .get("definition_kind")
                        .is_some_and(|actual| actual == tag)
                })
            })
            .collect::<Vec<_>>();
        let from = if from_candidates.len() == 1 {
            Some(from_candidates[0].clone())
        } else {
            None
        };
        let target_unit = files.get(&n(relation, "to_file")).map(|(_, u)| u);
        let key = s(relation, "to");
        let mut to = if let Some(unit) = target_unit {
            semantic_keys
                .get(&(unit.clone(), key))
                .cloned()
                .unwrap_or_default()
        } else {
            all_keys.get(key.as_str()).cloned().unwrap_or_default()
        };
        to.retain(|id| {
            relation["to_definition_kind"].as_str().is_none_or(|tag| {
                snapshot.entities[entity_index[id]]
                    .attributes
                    .get("definition_kind")
                    .is_some_and(|actual| actual == tag)
            })
        });
        to.sort();
        to.dedup();
        if to.is_empty() {
            let key = s(relation, "to");
            if !key.is_empty() {
                let source = anchor(&relation["to_source"], &files);
                let target_unit = target_unit.cloned().unwrap_or_else(|| "external".into());
                let id = native_relation_target_id(&target_unit, &key, &source)?;
                if !entity_index.contains_key(&id) {
                    entity_index.insert(id.clone(), snapshot.entities.len());
                    snapshot.entities.push(Entity {
                        stable_handle: None,
                        locations: Default::default(),
                        id: id.clone(),
                        unit: target_unit,
                        provider_id: key,
                        name: s(relation, "to_name"),
                        qualified_name: s(relation, "to_qn"),
                        kind: kind(&s(relation, "to_kind")),
                        source,
                        extent: None,
                        signature: s(relation, "to_signature"),
                        parent: None,
                        external: relation["to_external"].as_bool().unwrap_or(true),
                        exported: false,
                        attributes: BTreeMap::new(),
                        type_definitions: vec![],
                        type_navigation: vec![],
                        provenance: prov_id.clone(),
                    });
                }
                to.push(id);
            }
        }
        if from.is_none() || to.len() != 1 {
            snapshot.gaps.push(Gap::new(
                "StaticRelationBindingAmbiguous",
                format!("{provider}:{}", s(relation, "from")),
                format!(
                    "native relationship {} has {} source and {} target interpretations",
                    s(relation, "kind"),
                    from_candidates.len(),
                    to.len()
                ),
            ));
        }
        if let (Some(from), Some(target)) = (
            from,
            if to.len() == 1 {
                Some(to[0].clone())
            } else {
                None
            },
        ) {
            let kind = match relation["kind"].as_str() {
                Some("Implements") => RelationKind::Implements,
                Some("Overrides") => RelationKind::Overrides,
                Some("Dispatches") => RelationKind::Dispatches,
                Some("Extends") => RelationKind::Extends,
                _ => {
                    snapshot.gaps.push(Gap::new(
                        "NativeRelationKindUnmapped",
                        provider,
                        format!(
                            "unknown native relation {} was not interpreted",
                            s(relation, "kind")
                        ),
                    ));
                    continue;
                }
            };
            snapshot.add_relation(&from, &target, kind, None, None, prov.clone());
        }
    }
    let mut definitions: HashMap<(String, SourceKey), Vec<String>> = HashMap::new();
    for e in &snapshot.entities {
        if e.kind != EntityKind::Body
            && let Some(a) = &e.source
        {
            definitions
                .entry((
                    e.unit.clone(),
                    SourceKey(a.file_revision.clone(), a.start, a.end),
                ))
                .or_default()
                .push(e.id.clone());
        }
    }
    // Resolve type navigation by exact producer source locations, not names.
    for lexical in list(raw, "lexical_parents") {
        let lookup = |span: &Value| -> Option<String> {
            let source = anchor(span, &files)?;
            let (_, unit) = files.get(&n(span, "file"))?;
            let ids = definitions.get(&(
                unit.clone(),
                SourceKey(source.file_revision, source.start, source.end),
            ))?;
            let matches = ids
                .iter()
                .filter(|id| snapshot.entities[entity_index[*id]].kind == EntityKind::Function)
                .collect::<Vec<_>>();
            (matches.len() == 1).then(|| matches[0].clone())
        };
        if let (Some(child), Some(owner)) = (lookup(&lexical["child"]), lookup(&lexical["owner"])) {
            let source = snapshot.entities[entity_index[&child]].source.clone();
            snapshot.add_relation(
                &owner,
                &child,
                RelationKind::LexicallyContains,
                source,
                None,
                Provenance {
                    authority: Authority::RuleDerived,
                    note: "native AST lexical function nesting, anchored at both declarations"
                        .into(),
                    ..prov.clone()
                },
            );
        }
    }
    for symbol in list(raw, "symbols") {
        let Some(id) = tokens.get(&n(symbol, "token")).cloned() else {
            continue;
        };
        let mut type_ids = vec![];
        let mut navigation = vec![];
        for nav in list(symbol, "type_definitions") {
            let Some(source) = anchor(&nav["source"], &files) else {
                continue;
            };
            let target_unit = files
                .get(&n(&nav["source"], "file"))
                .map(|(_, u)| u.clone())
                .unwrap_or_default();
            let matches = definitions
                .get(&(
                    target_unit,
                    SourceKey(source.file_revision.clone(), source.start, source.end),
                ))
                .cloned()
                .unwrap_or_default();
            navigation.push(TypeNavigation {
                source,
                extent: anchor(&nav["body"], &files),
                candidates: matches.clone(),
            });
            type_ids.extend(matches);
        }
        if let Some(e) = entity_index.get(&id).map(|i| &mut snapshot.entities[*i]) {
            e.type_definitions = type_ids;
            e.type_navigation = navigation;
        }
        let text = s(symbol, "documentation");
        if !text.is_empty()
            && let Some(entity) = entity_index.get(&id).map(|i| snapshot.entities[*i].clone())
            && let Some(source) = anchor(&symbol["documentation_source"], &files)
                .or(entity.extent)
                .or(entity.source)
        {
            let docid = digest(&serde_json::to_vec(&(&id, &text, "rustdoc"))?);
            snapshot.documents.push(DocumentBlock {
                id: docid,
                file_revision: source.file_revision.clone(),
                kind: if language == Language::Go {
                    DocumentKind::GoDoc
                } else if language == Language::Rust {
                    DocumentKind::Rustdoc
                } else {
                    DocumentKind::CodeComment
                },
                title: entity.qualified_name,
                text,
                source,
                owner: Some(id),
                parent: None,
                links: vec![],
                provenance: documentation_id.clone(),
            });
        }
    }
    for document in list(raw, "scoped_documentation") {
        let Some(source) = anchor(&document["source"], &files) else {
            continue;
        };
        let Some((_, unit)) = files.get(&n(&document["source"], "file")) else {
            continue;
        };
        let owners = anchor(&document["owner"], &files).and_then(|a| {
            definitions.get(&(unit.clone(), SourceKey(a.file_revision, a.start, a.end)))
        });
        let owner = owners
            .filter(|ids| ids.len() == 1)
            .map(|ids| ids[0].clone())
            .or_else(|| {
                file_entities
                    .get(&(unit.clone(), source.file_revision.clone()))
                    .cloned()
            });
        let text = s(document, "text");
        let id = digest(&serde_json::to_vec(&(
            &unit,
            &source,
            &text,
            "scoped-rustdoc",
        ))?);
        snapshot.documents.push(DocumentBlock {
            id,
            file_revision: source.file_revision.clone(),
            kind: DocumentKind::Rustdoc,
            title: "module/crate documentation".into(),
            text,
            source,
            owner,
            parent: None,
            links: vec![],
            provenance: documentation_id.clone(),
        });
    }
    let doc_ranges = RangeIndex::new(snapshot.documents.iter().enumerate().filter_map(|(i, d)| {
        Some((
            snapshot.entities[*entity_index.get(d.owner.as_ref()?)?]
                .unit
                .clone(),
            d.source.clone(),
            i,
        ))
    }));
    for link in list(raw, "doc_links") {
        let Some(source) = anchor(&link["source"], &files) else {
            continue;
        };
        let mut targets = vec![];
        for t in list(link, "targets") {
            if let Some(a) = anchor(t, &files) {
                let unit = files
                    .get(&n(t, "file"))
                    .map(|(_, u)| u.clone())
                    .unwrap_or_default();
                targets.extend(
                    definitions
                        .get(&(unit, SourceKey(a.file_revision, a.start, a.end)))
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        }
        targets.sort();
        targets.dedup();
        let link_unit = files
            .get(&n(&link["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        if let Some(doc) = doc_ranges
            .nearest(&link_unit, &source)
            .map(|i| &mut snapshot.documents[i])
        {
            doc.links.push(DocumentLink {
                text: s(link, "text"),
                destination: s(link, "text"),
                target: if targets.len() == 1 {
                    Resolution::Resolved(targets[0].clone())
                } else if targets.is_empty() {
                    Resolution::Unknown("doc target unavailable".into())
                } else {
                    Resolution::Candidates(targets)
                },
                authority: Authority::SemanticResolution,
            });
        }
    }
    for comment in list(raw, "comments") {
        let Some(source) = anchor(&comment["source"], &files) else {
            continue;
        };
        let unit = files
            .get(&n(&comment["source"], "file"))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let owner = body_index
            .nearest(&unit, &source)
            .map(|i| bodies[i].owner.clone());
        let text = s(comment, "text");
        snapshot.documents.push(DocumentBlock {
            id: digest(&serde_json::to_vec(&(&unit, &source, "comment"))?),
            file_revision: source.file_revision.clone(),
            kind: DocumentKind::CodeComment,
            title: text.chars().take(80).collect(),
            text,
            source,
            owner,
            parent: None,
            links: vec![],
            provenance: comment_id.clone(),
        });
    }
    let tests: HashSet<_> = snapshot
        .entities
        .iter()
        .filter(|e| e.attributes.contains_key("test"))
        .map(|e| e.id.clone())
        .collect();
    let test_edges: Vec<_> = snapshot
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls && tests.contains(&r.from))
        .cloned()
        .collect();
    for r in test_edges {
        snapshot.add_relation(&r.from,&r.to,RelationKind::Tests,r.source,r.occurrence,Provenance{authority:Authority::RuleDerived,note:"call from a native #[test] declaration; does not establish executed test coverage".into(),..prov.clone()});
    }
    snapshot.bodies.extend(bodies);
    // A bound abstract declaration is not a runtime target. Enumerate possible
    // bodies from native override facts without multiplying every call site by
    // every implementor. The candidate edge remains explicitly rule-derived.
    let body_owners: HashSet<_> = snapshot.bodies.iter().map(|b| b.owner.clone()).collect();
    let candidates: Vec<_> = snapshot
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Overrides && body_owners.contains(&r.from))
        .cloned()
        .collect();
    for r in candidates {
        snapshot.add_relation(&r.to,&r.from,RelationKind::MayCall,r.source,None,Provenance{
            authority:Authority::RuleDerived,
            note:"possible implementation body from native override relationship; receiver/runtime selection is unknown".into(),..prov.clone()
        });
    }
    for capability in [
        "definitions",
        "type_definitions",
        "references",
        "calls",
        "implementations",
        "bodies",
    ] {
        snapshot.coverage.push(CapabilityCoverage{language:Some(language),capability:capability.into(),state:Support::Partial,reason:"native semantic APIs provide resolved static facts; dynamic targets, failed macro expansions and uncaptured external inputs remain explicit".into()});
    }
    if language == Language::Rust {
        for capability in ["macros", "rustdoc_links"] {
            snapshot.coverage.push(CapabilityCoverage{language:Some(language),capability:capability.into(),state:Support::Partial,reason:"native HIR links and expansions are retained; failed or uncaptured expansions remain gaps".into()});
        }
    }
    Ok(())
}

#[derive(Hash, PartialEq, Eq)]
struct SourceKey(String, u64, u64);
