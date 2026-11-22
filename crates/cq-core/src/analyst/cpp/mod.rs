use super::languages::*;
pub mod command;
mod staged;
use super::native;
use crate::{Error, Result, control::CaptureControl, model::*, ports::*};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::{path::PathBuf, process::Command};
pub fn analyze(
    r: &NativeAnalysisRequest,
    store: &dyn AnalysisWorkspace,
    control: &CaptureControl,
    s: &mut SemanticSnapshot,
) -> Result<()> {
    staged::analyze(r, store, control, s)
}
pub(super) fn finish_links(s: &mut SemanticSnapshot) -> Result<()> {
    let mut definitions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entity in &s.entities {
        if matches!(entity.kind, EntityKind::Function | EntityKind::Method)
            && !entity.attributes.contains_key("declaration_only")
        {
            definitions
                .entry(entity.provider_id.clone())
                .or_default()
                .push(entity.id.clone());
        }
    }
    let aliases: Vec<_> = s
        .entities
        .iter()
        .filter(|e| {
            matches!(e.kind, EntityKind::Function | EntityKind::Method)
                && e.attributes.contains_key("declaration_only")
        })
        .filter_map(|e| {
            definitions
                .get(&e.provider_id)
                .map(|targets| (e.id.clone(), targets.clone()))
        })
        .collect();
    for (from, targets) in aliases {
        if let Some(entity) = s.entities.iter_mut().find(|e| e.id == from) {
            entity.attributes.insert(
                "definition_candidates".into(),
                serde_json::to_string(&targets)?,
            );
        }
        for to in targets {
            s.add_relation(&from,&to,RelationKind::References,None,None,Provenance{provider:"libclang-usr-join".into(),version:"1".into(),authority:Authority::RuleDerived,build_run:None,note:"translation units share a native USR; linker and ODR selection are not observed".into()});
            s.add_relation(&from,&to,RelationKind::MayCall,None,None,Provenance{provider:"libclang-usr-join".into(),version:"1".into(),authority:Authority::RuleDerived,build_run:None,note:"possible body sharing native USR across translation units; linker/configuration selection is not observed".into()});
        }
    }
    let entities: BTreeMap<_, _> = s.entities.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut declarations: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in &s.entities {
        declarations.entry(&e.provider_id).or_default().push(&e.id);
    }
    let body_owners: std::collections::HashSet<_> =
        s.bodies.iter().map(|b| b.owner.as_str()).collect();
    let candidates: Vec<_> = s
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Overrides && body_owners.contains(r.from.as_str()))
        .flat_map(|r| {
            entities
                .get(r.to.as_str())
                .into_iter()
                .flat_map(|e| {
                    declarations
                        .get(e.provider_id.as_str())
                        .into_iter()
                        .flatten()
                })
                .map(move |id| ((*id).to_owned(), r.from.clone()))
        })
        .collect();
    for (from, to) in candidates {
        s.add_relation(&from,&to,RelationKind::MayCall,None,None,Provenance{provider:"libclang-usr-override".into(),version:"1".into(),authority:Authority::RuleDerived,build_run:None,note:"possible implementation from native override USRs across translation units; runtime receiver and link/configuration selection are unknown".into()});
    }
    s.gaps.push(Gap::new("LinkSelectionNotObserved","C/C++ compilation database","translation units are replayed with real compilers; cross-unit definitions are native-USR candidates until the link configuration is observed"));
    Ok(())
}
