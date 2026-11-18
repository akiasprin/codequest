use super::normalize::{anchor, list, n};
use crate::{Result, model::*};
use serde_json::Value;
use std::collections::HashMap;
pub(super) type Slots = HashMap<String, Vec<(usize, bool, Vec<String>)>>;
pub(super) fn bind(
    snapshot: &mut SemanticSnapshot,
    raw: &Value,
    files: &HashMap<u64, (FileRevision, String)>,
) -> Result<Slots> {
    let mut sources: HashMap<(String, String, u64, u64), Vec<usize>> = HashMap::new();
    for (i, e) in snapshot.entities.iter().enumerate() {
        if let Some(a) = &e.source {
            sources
                .entry((e.unit.clone(), a.file_revision.clone(), a.start, a.end))
                .or_default()
                .push(i);
        }
    }
    let resolve = |span: &Value| -> Vec<usize> {
        let Some(a) = anchor(span, files) else {
            return vec![];
        };
        let Some((_, unit)) = files.get(&n(span, "file")) else {
            return vec![];
        };
        sources
            .get(&(unit.clone(), a.file_revision, a.start, a.end))
            .cloned()
            .unwrap_or_default()
    };
    let mut parameters = Slots::new();
    for group in list(raw, "parameters") {
        let owners: Vec<_> = resolve(&group["owner"])
            .into_iter()
            .filter(|i| {
                matches!(
                    snapshot.entities[*i].kind,
                    EntityKind::Function | EntityKind::Method | EntityKind::Body
                )
            })
            .collect();
        if owners.len() != 1 {
            continue;
        }
        let owner = snapshot.entities[owners[0]].id.clone();
        let mut slots = vec![];
        for slot in list(group, "slots") {
            let mut ids = vec![];
            let index = n(slot, "index") as usize;
            let receiver = slot["receiver"] == true;
            for binding in list(slot, "bindings") {
                for i in resolve(binding) {
                    let e = &mut snapshot.entities[i];
                    if matches!(e.kind, EntityKind::Parameter | EntityKind::Variable) {
                        e.kind = EntityKind::Parameter;
                        e.parent = Some(owner.clone());
                        e.attributes
                            .insert("parameter_index".into(), index.to_string());
                        e.attributes.insert("receiver".into(), receiver.to_string());
                        ids.push(e.id.clone());
                    }
                }
            }
            ids.sort();
            ids.dedup();
            slots.push((index, receiver, ids));
        }
        parameters.insert(owner, slots);
    }
    Ok(parameters)
}
pub(super) fn arguments(
    arguments: &[SourceAnchor],
    target: &str,
    method: bool,
    parameters: &Slots,
) -> Vec<ArgumentBinding> {
    let slots = parameters.get(target);
    let shift = if method {
        slots
            .map(|s| s.iter().filter(|(_, receiver, _)| *receiver).count())
            .unwrap_or(0)
    } else {
        0
    };
    arguments.iter().enumerate().map(|(position,source)|{let ids=slots.and_then(|slots|slots.iter().find(|(i,_,_)|*i==position+shift)).map(|(_,_,ids)|ids.clone()).unwrap_or_default();ArgumentBinding{position,source:source.clone(),parameter:if ids.len()==1{Resolution::Resolved(ids[0].clone())}else if ids.is_empty(){Resolution::Unknown("formal parameter is unnamed, variadic, indirect, or outside native coverage".into())}else{Resolution::Candidates(ids)}}}).collect()
}
