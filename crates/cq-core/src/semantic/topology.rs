//! Structural projection of observed compilation units, kept apart from motives.
use crate::{Result, model::*};
use std::collections::{BTreeMap, HashMap};
pub fn attach(snapshot: &mut SemanticSnapshot) -> Result<()> {
    let provider=Provenance{provider:"observed-build-topology".into(),version:"1".into(),authority:Authority::SemanticResolution,build_run:None,note:"unit identity and dependency binding from captured build metadata; not a runtime ordering".into()};
    let provider_id = snapshot.intern_provenance(provider.clone());
    let project = digest(&serde_json::to_vec(&(&snapshot.project, "project"))?);
    snapshot.entities.push(Entity {
        stable_handle: None,
        locations: Default::default(),
        id: project.clone(),
        unit: String::new(),
        provider_id: snapshot.project.clone(),
        kind: EntityKind::Project,
        name: snapshot.project.clone(),
        qualified_name: snapshot.project.clone(),
        source: None,
        extent: None,
        signature: String::new(),
        parent: None,
        external: false,
        exported: false,
        attributes: BTreeMap::new(),
        type_definitions: vec![],
        type_navigation: vec![],
        provenance: provider_id.clone(),
    });
    let units = snapshot.units.clone();
    let mut ids = HashMap::new();
    for u in &units {
        let id = digest(&serde_json::to_vec(&(&u.id, "compilation-unit"))?);
        ids.insert(u.id.clone(), id.clone());
        snapshot.entities.push(Entity {
            stable_handle: None,
            locations: Default::default(),
            id: id.clone(),
            unit: u.id.clone(),
            provider_id: u.id.clone(),
            kind: EntityKind::CompilationUnit,
            name: u.name.value().cloned().unwrap_or(u.id.clone()),
            qualified_name: format!("{}::{:?}::{}", snapshot.project, u.language, u.id),
            source: None,
            extent: None,
            signature: String::new(),
            parent: Some(project.clone()),
            external: false,
            exported: false,
            attributes: BTreeMap::from([
                ("role".into(), format!("{:?}", u.role)),
                (
                    "configuration".into(),
                    serde_json::to_string(&u.configuration)?,
                ),
                ("binding".into(), format!("{:?}", u.binding)),
            ]),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: provider_id.clone(),
        });
        snapshot.add_relation(
            &project,
            &id,
            RelationKind::Contains,
            None,
            None,
            provider.clone(),
        );
    }
    let files: Vec<_> = snapshot
        .entities
        .iter()
        .filter(|e| e.kind == EntityKind::File)
        .map(|e| (e.unit.clone(), e.id.clone()))
        .collect();
    for (unit, file) in files {
        if let Some(id) = ids.get(&unit) {
            snapshot.add_relation(
                id,
                &file,
                RelationKind::Contains,
                None,
                None,
                provider.clone(),
            );
        }
    }
    let source_files: HashMap<_, _> = snapshot
        .entities
        .iter()
        .filter(|e| e.kind == EntityKind::File)
        .filter_map(|e| {
            e.source
                .as_ref()
                .map(|a| ((e.unit.clone(), a.file_revision.clone()), e.id.clone()))
        })
        .collect();
    let memberships: Vec<_> = snapshot
        .entities
        .iter()
        .filter(|e| e.kind != EntityKind::File)
        .filter_map(|e| {
            e.source
                .as_ref()
                .and_then(|a| source_files.get(&(e.unit.clone(), a.file_revision.clone())))
                .map(|file| (file.clone(), e.id.clone(), e.source.clone()))
        })
        .collect();
    for (file, entity, source) in memberships {
        snapshot.add_relation(
            &file,
            &entity,
            RelationKind::Contains,
            source,
            None,
            provider.clone(),
        );
    }
    let dependencies = snapshot.build_dependencies.clone();
    for d in dependencies {
        if let (Some(from), Some(to)) = (
            ids.get(&d.consumer),
            d.producer.value().and_then(|id| ids.get(id)),
        ) {
            snapshot.add_relation(
                from,
                to,
                RelationKind::DependsOn,
                None,
                None,
                Provenance {
                    build_run: d.evidence.first().map(|e| e.run_id.clone()),
                    ..provider.clone()
                },
            );
        }
    }
    Ok(())
}
