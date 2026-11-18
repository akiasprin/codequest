//! Global collaboration uses compact declaration catalogues. Facts and document
//! bodies are fetched and revised one record at a time through the analysis port.
use super::records::{self, Record, Store};
use crate::{Result, model::*, ports::ContentRepository};
use std::collections::{BTreeMap, HashMap, HashSet};
#[allow(clippy::too_many_arguments)]
fn relation(
    s: &mut SemanticSnapshot,
    store: &dyn Store,
    from: &str,
    to: &str,
    kind: RelationKind,
    source: Option<SourceAnchor>,
    occurrence: Option<String>,
    provenance: ProvenanceId,
) -> Result<()> {
    let id = digest(&serde_json::to_vec(&(
        from,
        to,
        kind,
        &source,
        &occurrence,
        &provenance,
    ))?);
    store.put(
        s,
        Record::Relation(Relation {
            id,
            from: from.into(),
            to: to.into(),
            kind,
            source,
            occurrence,
            provenance,
        }),
        false,
    )
}
pub fn cpp_links(s: &mut SemanticSnapshot, store: &dyn Store) -> Result<()> {
    let mut declarations = BTreeMap::<String, Vec<String>>::new();
    let mut definitions = BTreeMap::<String, Vec<String>>::new();
    let mut keys = HashMap::new();
    let mut aliases = vec![];
    let mut bodies = HashSet::new();
    store.visit(&mut |r| {
        match r {
            Record::Entity(e) => {
                declarations
                    .entry(e.provider_id.clone())
                    .or_default()
                    .push(e.id.clone());
                keys.insert(e.id.clone(), e.provider_id.clone());
                if matches!(e.kind, EntityKind::Function | EntityKind::Method) {
                    if e.attributes.contains_key("declaration_only") {
                        aliases.push((e.id, e.provider_id));
                    } else {
                        definitions.entry(e.provider_id).or_default().push(e.id);
                    }
                }
            }
            Record::Body(b) => {
                bodies.insert(b.owner);
            }
            _ => {}
        }
        Ok(())
    })?;
    let p = s.intern_provenance(Provenance {
        provider: "libclang-usr-join".into(),
        version: "1".into(),
        authority: Authority::RuleDerived,
        build_run: None,
        note: "translation units share a native USR; linker and ODR selection are not observed"
            .into(),
    });
    let call=s.intern_provenance(Provenance{provider:"libclang-usr-join".into(),version:"1".into(),authority:Authority::RuleDerived,build_run:None,note:"possible body sharing native USR across translation units; linker/configuration selection is not observed".into()});
    for (id, key) in aliases {
        if let Some(targets) = definitions.get(&key) {
            if let Some(Record::Entity(mut e)) = store.get("entity", &id)? {
                e.attributes.insert(
                    "definition_candidates".into(),
                    serde_json::to_string(targets)?,
                );
                store.put(s, Record::Entity(e), true)?;
            }
            for to in targets {
                relation(
                    s,
                    store,
                    &id,
                    to,
                    RelationKind::References,
                    None,
                    None,
                    p.clone(),
                )?;
                relation(
                    s,
                    store,
                    &id,
                    to,
                    RelationKind::MayCall,
                    None,
                    None,
                    call.clone(),
                )?;
            }
        }
    }
    let override_p=s.intern_provenance(Provenance{provider:"libclang-usr-override".into(),version:"1".into(),authority:Authority::RuleDerived,build_run:None,note:"possible implementation from native override USRs across translation units; runtime receiver and link/configuration selection are unknown".into()});
    store.visit(&mut |r| {
        if let Record::Relation(r) = r
            && r.kind == RelationKind::Overrides
            && bodies.contains(&r.from)
            && let Some(key) = keys.get(&r.to)
        {
            for from in declarations.get(key).into_iter().flatten() {
                relation(
                    s,
                    store,
                    from,
                    &r.from,
                    RelationKind::MayCall,
                    None,
                    None,
                    override_p.clone(),
                )?;
            }
        }
        Ok(())
    })
}
pub fn topology(s: &mut SemanticSnapshot, store: &dyn Store) -> Result<()> {
    super::topology::attach(s)?;
    records::drain(s, store)?;
    let p=s.intern_provenance(Provenance{provider:"observed-build-topology".into(),version:"1".into(),authority:Authority::SemanticResolution,build_run:None,note:"unit identity and dependency binding from captured build metadata; not a runtime ordering".into()});
    let units = s
        .units
        .iter()
        .map(|u| {
            Ok((
                u.id.clone(),
                digest(&serde_json::to_vec(&(&u.id, "compilation-unit"))?),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let mut files = HashMap::new();
    store.visit_family("entity", &mut |r| {
        if let Record::Entity(e) = r
            && e.kind == EntityKind::File
        {
            if let Some(a) = e.source {
                files.insert((e.unit.clone(), a.file_revision), e.id.clone());
            }
            if let Some(unit) = units.get(&e.unit) {
                relation(
                    s,
                    store,
                    unit,
                    &e.id,
                    RelationKind::Contains,
                    None,
                    None,
                    p.clone(),
                )?;
            }
        }
        Ok(())
    })?;
    store.visit_family("entity", &mut |r| {
        if let Record::Entity(e) = r
            && e.kind != EntityKind::File
            && let Some(a) = e.source
            && let Some(file) = files.get(&(e.unit, a.file_revision.clone()))
        {
            relation(
                s,
                store,
                file,
                &e.id,
                RelationKind::Contains,
                Some(a),
                None,
                p.clone(),
            )?;
        }
        Ok(())
    })
}
#[derive(Clone)]
struct Declaration {
    id: String,
    unit: String,
    external: bool,
}
fn visit_documents(
    store: &dyn Store,
    project: &str,
    family: &str,
    phase: &str,
    label: &str,
    visitor: &mut dyn FnMut(Record) -> Result<()>,
) -> Result<()> {
    let unit = if family == "document" {
        "文档"
    } else {
        "实体"
    };
    store.task_progress(project, phase, label, 0, None, unit);
    let total = store.family_len(family)?;
    store.task_progress(project, phase, label, 0, total, unit);
    let mut done = 0;
    store.visit_family(family, &mut |r| {
        visitor(r)?;
        done += 1;
        store.task_progress(project, phase, label, done, total, unit);
        Ok(())
    })
}
fn needs_lookup(link: &DocumentLink) -> bool {
    link.authority != Authority::SemanticResolution && link.target.resolved().is_none()
}
pub fn documents(
    s: &mut SemanticSnapshot,
    store: &dyn Store,
    content: &dyn ContentRepository,
    max_bytes: u64,
) -> Result<()> {
    let p = Provenance {
        provider: "repository-documents".into(),
        version: s
            .providers
            .iter()
            .map(|p| p.version.as_str())
            .collect::<Vec<_>>()
            .join(";"),
        authority: Authority::AuthorStatement,
        build_run: None,
        note: "repository text, not a generated attribution of intent".into(),
    };
    let repository_p = s.intern_provenance(p.clone());
    let document_paths = crate::documents::repository_paths(&s.root.path())?;
    let mut source_files = s
        .sources
        .files
        .iter()
        .map(|f| (f.logical_path.clone(), f.clone()))
        .collect::<BTreeMap<_, _>>();
    store.task_progress(
        &s.project,
        "documents.parse",
        "文档：读取仓库文件",
        0,
        Some(document_paths.len() as u64),
        "文件",
    );
    for (done, path) in document_paths.iter().enumerate() {
        let mut fragment = SemanticSnapshot::empty(s.project.clone(), s.root.clone());
        let existing = source_files
            .get(&NativeValue::new(path))
            .cloned()
            .into_iter()
            .collect();
        fragment.sources = SourceSnapshot::seal(existing, s.sources.binding.clone(), vec![]);
        crate::documents::parse_file(&mut fragment, path, content, max_bytes, &p)?;
        for file in &fragment.sources.files {
            source_files.insert(file.logical_path.clone(), file.clone());
        }
        s.provenance_records
            .extend(fragment.provenance_records.clone());
        s.gaps.append(&mut fragment.gaps);
        records::drain(&mut fragment, store)?;
        store.task_progress(
            &s.project,
            "documents.parse",
            "文档：读取仓库文件",
            done as u64 + 1,
            Some(document_paths.len() as u64),
            "文件",
        );
    }
    s.sources = SourceSnapshot::seal(
        source_files.into_values().collect(),
        s.sources.binding.clone(),
        s.sources.gaps.clone(),
    );
    let project = s.project.clone();
    let mut entity_ids = HashSet::new();
    let mut file_ids = HashSet::new();
    let mut doc_files = HashSet::new();
    let mut requested = HashSet::<String>::new();
    let mut owner_units = HashMap::new();
    let mut sections = vec![];
    visit_documents(
        store,
        &project,
        "document",
        "documents.ownership",
        "文档：归属与引用收集",
        &mut |r| {
            if let Record::Document(d) = r {
                if d.kind == DocumentKind::Markdown {
                    sections.push((
                        d.file_revision.clone(),
                        crate::documents::heading_slug(&d.title),
                        d.id.clone(),
                    ));
                }
                for link in &d.links {
                    if needs_lookup(link) {
                        let destination = link.destination.trim_matches('`');
                        if !destination.starts_with("http:")
                            && !destination.starts_with("https:")
                            && !destination.contains('#')
                        {
                            requested
                                .insert(destination.trim_start_matches("crate::").replace('`', ""));
                        }
                    }
                }
                doc_files.insert(d.file_revision.clone());
                let owner = if let Some(id) = &d.owner {
                    store.get("entity", id)?
                } else {
                    None
                };
                let owner = match owner {
                    Some(Record::Entity(e)) => Some((e.unit, e.external)),
                    _ => None,
                };
                if let (Some(id), Some((unit, _))) = (&d.owner, &owner) {
                    owner_units.insert(id.clone(), unit.clone());
                }
                if entity_ids.insert(d.id.clone()) && store.get("entity", &d.id)?.is_none() {
                    let e = Entity {
                        id: d.id.clone(),
                        unit: owner.as_ref().map(|(u, _)| u.clone()).unwrap_or_default(),
                        provider_id: d.id.clone(),
                        kind: EntityKind::DocSection,
                        name: d.title.clone(),
                        qualified_name: d.title.clone(),
                        stable_handle: None,
                        locations: Default::default(),
                        source: Some(d.source.clone()),
                        extent: Some(d.source.clone()),
                        signature: String::new(),
                        parent: d.parent.clone(),
                        external: owner.is_some_and(|(_, x)| x),
                        exported: false,
                        attributes: BTreeMap::from([("search_text".into(), d.text.clone())]),
                        type_definitions: vec![],
                        type_navigation: vec![],
                        provenance: d.provenance.clone(),
                    };
                    store.put(s, Record::Entity(e), false)?;
                }
                if let Some(parent) = &d.parent {
                    relation(
                        s,
                        store,
                        parent,
                        &d.id,
                        RelationKind::Contains,
                        Some(d.source.clone()),
                        None,
                        d.provenance.clone(),
                    )?;
                }
                if let Some(owner) = &d.owner {
                    relation(
                        s,
                        store,
                        &d.id,
                        owner,
                        RelationKind::Documents,
                        Some(d.source.clone()),
                        None,
                        d.provenance.clone(),
                    )?;
                }
            }
            Ok(())
        },
    )?;
    let paths = s
        .sources
        .files
        .iter()
        .map(|f| (f.id.clone(), f.logical_path.path()))
        .collect::<HashMap<_, _>>();
    let mut candidates = HashMap::<String, Vec<Declaration>>::new();
    let mut files_by_path = HashMap::<std::path::PathBuf, Vec<String>>::new();
    visit_documents(
        store,
        &project,
        "entity",
        "documents.symbols",
        "文档：查找引用的符号",
        &mut |r| {
            if let Record::Entity(e) = r {
                if e.kind == EntityKind::File
                    && let Some(a) = &e.source
                {
                    file_ids.insert(a.file_revision.clone());
                    if let Some(path) = paths.get(&a.file_revision) {
                        files_by_path
                            .entry(path.clone())
                            .or_default()
                            .push(e.id.clone());
                    }
                }
                let name = &e.qualified_name;
                let mut keys = vec![];
                if requested.contains(name) {
                    keys.push(name.as_str());
                }
                for (i, pair) in name.as_bytes().windows(2).enumerate() {
                    if pair == b"::" && requested.contains(&name[i + 2..]) {
                        keys.push(&name[i + 2..]);
                    }
                }
                for key in keys {
                    candidates
                        .entry(key.to_owned())
                        .or_default()
                        .push(Declaration {
                            id: e.id.clone(),
                            unit: e.unit.clone(),
                            external: e.external,
                        });
                }
            }
            Ok(())
        },
    )?;
    for file in s.sources.files.clone() {
        if !file_ids.contains(&file.id) && doc_files.contains(&file.id) {
            let id = digest(&serde_json::to_vec(&(&file.id, "document-file"))?);
            store.put(
                s,
                Record::Entity(Entity {
                    id,
                    unit: String::new(),
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
                    stable_handle: None,
                    locations: Default::default(),
                    source: Some(SourceAnchor {
                        file_revision: file.id.clone(),
                        start: 0,
                        end: file.blob.len,
                    }),
                    extent: None,
                    signature: String::new(),
                    parent: None,
                    external: false,
                    exported: false,
                    attributes: Default::default(),
                    type_definitions: vec![],
                    type_navigation: vec![],
                    provenance: repository_p.clone(),
                }),
                false,
            )?;
        }
    }
    // Newly created document-file entities also participate in path links.
    for file in &s.sources.files {
        if !file_ids.contains(&file.id) && doc_files.contains(&file.id) {
            files_by_path
                .entry(file.logical_path.path())
                .or_default()
                .push(digest(&serde_json::to_vec(&(&file.id, "document-file"))?));
        }
    }
    let mut section_index = HashMap::<(std::path::PathBuf, String), Vec<String>>::new();
    for (file, slug, id) in sections {
        if let Some(path) = paths.get(&file) {
            section_index
                .entry((path.clone(), slug))
                .or_default()
                .push(id);
        }
    }
    visit_documents(
        store,
        &project,
        "document",
        "documents.links",
        "文档：写入链接关系",
        &mut |r| {
            if let Record::Document(mut d) = r {
                for link in &mut d.links {
                    if needs_lookup(link) {
                        let destination = link.destination.trim_matches('`');
                        if destination.starts_with("http:") || destination.starts_with("https:") {
                            continue;
                        }
                        let (path, fragment) =
                            destination.split_once('#').unwrap_or((destination, ""));
                        let target = paths
                            .get(&d.file_revision)
                            .and_then(|p| p.parent())
                            .map(|base| crate::documents::lexical(&base.join(path)));
                        let candidates = if !fragment.is_empty() {
                            let file = if path.is_empty() {
                                paths.get(&d.file_revision)
                            } else {
                                target.as_ref()
                            };
                            file.and_then(|p| section_index.get(&(p.clone(), fragment.to_owned())))
                                .cloned()
                                .unwrap_or_default()
                        } else if !path.is_empty()
                            && target
                                .as_ref()
                                .is_some_and(|p| files_by_path.contains_key(p))
                        {
                            files_by_path
                                .get(target.as_ref().unwrap())
                                .cloned()
                                .unwrap_or_default()
                        } else {
                            let qualified =
                                destination.trim_start_matches("crate::").replace('`', "");
                            let owner = d.owner.as_ref().and_then(|o| owner_units.get(o));
                            candidates
                                .get(&qualified)
                                .into_iter()
                                .flatten()
                                .filter(|e| owner.is_none_or(|o| e.unit == *o || e.external))
                                .map(|e| e.id.clone())
                                .collect()
                        };
                        link.target = if candidates.len() == 1 {
                            Resolution::Resolved(candidates[0].clone())
                        } else if candidates.is_empty() {
                            Resolution::Unknown(format!(
                                "unresolved link or section anchor: {fragment}"
                            ))
                        } else {
                            Resolution::Candidates(candidates)
                        };
                        link.authority = Authority::AuthorStatement;
                    }
                    if let Some(target) = link.target.resolved() {
                        let p = s.intern_provenance(Provenance {
                            authority: link.authority,
                            note: "explicit document reference; not a runtime relationship".into(),
                            ..s.provenance_records[&d.provenance].clone()
                        });
                        relation(
                            s,
                            store,
                            &d.id,
                            target,
                            RelationKind::DocReferences,
                            Some(d.source.clone()),
                            None,
                            p,
                        )?;
                    }
                }
                store.put(s, Record::Document(d), true)?;
            }
            Ok(())
        },
    )
}
