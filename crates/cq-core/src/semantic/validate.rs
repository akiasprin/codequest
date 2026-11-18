use crate::{Error, Result, input::InputView, model::*, ports::ContentReader};
use std::collections::HashSet;
pub fn validate(snapshot: &SemanticSnapshot, content: &dyn ContentReader) -> Result<()> {
    validate_observations(snapshot)?;
    if let Some(p) = &snapshot.production {
        let mut validator = crate::semantic::production::Validator::new(p)?;
        crate::semantic::production::emit(snapshot, &mut validator)?;
    }
    validate_facts(snapshot, content)
}
pub fn validate_metadata(snapshot: &SemanticSnapshot, content: &dyn ContentReader) -> Result<()> {
    validate_observations(snapshot)?;
    validate_facts(&snapshot.metadata(), content)
}
fn validate_observations(snapshot: &SemanticSnapshot) -> Result<()> {
    if let Some(p) = &snapshot.production {
        let observed = p
            .unit_observations
            .iter()
            .map(|o| o.observed.clone())
            .collect::<Vec<_>>();
        let identities =
            super::identity::compilation_identities(&observed, &snapshot.root.display())?;
        for observation in &p.unit_observations {
            if !snapshot.units.iter().any(|u| u.id == observation.unit)
                || identities.units.get(&observation.observed.id) != Some(&observation.unit)
            {
                return Err(Error::new(
                    "InvalidObservationBinding",
                    "unit configuration does not match semantic identity",
                ));
            }
        }
        for observation in &p.provenance_observations {
            let mut canonical = observation.observed.clone();
            canonical.build_run = None;
            if observation.canonical.0 != digest(&serde_json::to_vec(&canonical)?)
                || !snapshot
                    .provenance_records
                    .contains_key(&observation.canonical)
                || observation
                    .observed
                    .build_run
                    .as_ref()
                    .is_some_and(|id| !snapshot.builds.iter().any(|b| &b.id == id))
            {
                return Err(Error::new(
                    "InvalidObservationBinding",
                    "provenance does not match retained build observation",
                ));
            }
        }
    }
    Ok(())
}
fn validate_facts(snapshot: &SemanticSnapshot, content: &dyn ContentReader) -> Result<()> {
    if snapshot.schema_version != 2 {
        return Err(Error::new("InvalidSnapshot", "unsupported semantic schema"));
    }
    for (id, p) in &snapshot.provenance_records {
        if id.0 != digest(&serde_json::to_vec(p)?) {
            return Err(Error::new("DigestMismatch", "provenance registry"));
        }
    }
    for id in snapshot
        .entities
        .iter()
        .map(|e| &e.provenance)
        .chain(snapshot.occurrences.iter().map(|e| &e.provenance))
        .chain(snapshot.relations.iter().map(|e| &e.provenance))
        .chain(snapshot.documents.iter().map(|e| &e.provenance))
    {
        if !snapshot.provenance_records.contains_key(id) {
            return Err(Error::new(
                "DanglingProvenance",
                "fact refers to absent shared evidence",
            ));
        }
    }
    if !snapshot.analysis_inputs.is_empty() {
        InputView::open(
            SourceSnapshot::seal(
                snapshot.analysis_inputs.clone(),
                InputBinding::PrePostChecked,
                vec![],
            ),
            content,
        )?;
    }
    let view = InputView::open(snapshot.sources.clone(), content)?;
    let relations: HashSet<_> = snapshot.relations.iter().map(|r| &r.id).collect();
    let documents: HashSet<_> = snapshot.documents.iter().map(|d| &d.id).collect();
    if relations.len() != snapshot.relations.len() || documents.len() != snapshot.documents.len() {
        return Err(Error::new(
            "DuplicateIdentity",
            format!(
                "relation/document identity collision: relations {}/{}, documents {}/{}",
                relations.len(),
                snapshot.relations.len(),
                documents.len(),
                snapshot.documents.len()
            ),
        ));
    }
    let ids: HashSet<_> = snapshot.entities.iter().map(|e| e.id.as_str()).collect();
    let bodies: HashSet<_> = snapshot.bodies.iter().map(|b| b.id.as_str()).collect();
    let occ: HashSet<_> = snapshot.occurrences.iter().map(|o| o.id.as_str()).collect();
    if ids
        .iter()
        .chain(bodies.iter())
        .chain(occ.iter())
        .any(|id| id.is_empty())
        || relations.iter().any(|id| id.is_empty())
        || documents.iter().any(|id| id.is_empty())
    {
        return Err(Error::new("InvalidIdentity", "empty semantic identity"));
    }
    if ids.len() != snapshot.entities.len()
        || bodies.len() != snapshot.bodies.len()
        || occ.len() != snapshot.occurrences.len()
    {
        let mut previous = std::collections::HashMap::new();
        let mut difference = String::new();
        for o in &snapshot.occurrences {
            if let Some(old) = previous.insert(&o.id, o) {
                let a = serde_json::to_value(old)?;
                let b = serde_json::to_value(o)?;
                let fields: Vec<_> = b
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.keys())
                    .filter(|key| a[*key] != b[*key])
                    .collect();
                difference = format!("; occurrence {} conflicting fields: {:?}", o.id, fields);
                break;
            }
        }
        return Err(Error::new(
            "DuplicateIdentity",
            format!(
                "semantic identity collision: entities {}/{}, bodies {}/{}, occurrences {}/{}{}",
                ids.len(),
                snapshot.entities.len(),
                bodies.len(),
                snapshot.bodies.len(),
                occ.len(),
                snapshot.occurrences.len(),
                difference
            ),
        ));
    }
    let source_files: std::collections::HashMap<_, _> = view
        .snapshot()
        .files
        .iter()
        .map(|f| (f.id.as_str(), f))
        .collect();
    let anchor = |a: &SourceAnchor| -> Result<()> {
        let file = source_files
            .get(a.file_revision.as_str())
            .copied()
            .ok_or_else(|| Error::new("MissingContent", "source revision absent"))?;
        if a.start > a.end || a.end > file.blob.len {
            return Err(Error::new("InvalidRange", "semantic anchor out of bounds"));
        }
        Ok(())
    };
    for e in &snapshot.entities {
        for a in e.locations.anchors() {
            anchor(a)?;
        }
        if let Some(a) = &e.source {
            anchor(a)?;
        }
        if let Some(a) = &e.extent {
            anchor(a)?;
        }
        for nav in &e.type_navigation {
            anchor(&nav.source)?;
            if let Some(a) = &nav.extent {
                anchor(a)?;
            }
            if nav.candidates.iter().any(|id| !ids.contains(id.as_str())) {
                return Err(Error::new(
                    "DanglingEntity",
                    "type navigation candidate absent",
                ));
            }
        }
        if e.parent
            .as_ref()
            .is_some_and(|id| !ids.contains(id.as_str()))
            || e.type_definitions
                .iter()
                .any(|id| !ids.contains(id.as_str()))
        {
            return Err(Error::new("DanglingEntity", "missing parent/type target"));
        }
    }
    let parents: std::collections::HashMap<_, _> = snapshot
        .entities
        .iter()
        .filter_map(|e| e.parent.as_ref().map(|p| (e.id.as_str(), p.as_str())))
        .collect();
    let mut done = HashSet::new();
    for child in parents.keys() {
        let mut chain = HashSet::new();
        let mut next = *child;
        while let Some(parent) = parents.get(next) {
            if done.contains(next) {
                break;
            }
            if !chain.insert(next) {
                return Err(Error::new("ParentCycle", "declaring-container cycle"));
            }
            next = parent;
        }
        done.extend(chain);
    }
    for b in &snapshot.bodies {
        anchor(&b.source)?;
        if !ids.contains(b.owner.as_str())
            || b.parent
                .as_ref()
                .is_some_and(|p| !bodies.contains(p.as_str()))
        {
            return Err(Error::new("DanglingBody", "missing owner/parent"));
        }
    }
    for o in &snapshot.occurrences {
        for a in o.locations.anchors() {
            anchor(a)?;
        }
        anchor(&o.source)?;
        for a in &o.arguments {
            anchor(a)?;
        }
        for binding in &o.argument_bindings {
            anchor(&binding.source)?;
            match &binding.parameter {
                Resolution::Resolved(id) if !ids.contains(id.as_str()) => {
                    return Err(Error::new(
                        "DanglingOccurrence",
                        "argument parameter absent",
                    ));
                }
                Resolution::Candidates(targets)
                    if targets.iter().any(|id| !ids.contains(id.as_str())) =>
                {
                    return Err(Error::new(
                        "DanglingOccurrence",
                        "argument candidates absent",
                    ));
                }
                _ => {}
            }
        }
        for resolution in [&o.declaration, &o.executable] {
            let targets: Vec<_> = match resolution {
                Resolution::Resolved(s) => vec![s],
                Resolution::Candidates(v) => v.iter().collect(),
                _ => vec![],
            };
            if targets
                .iter()
                .any(|id| !ids.contains(id.as_str()) && !bodies.contains(id.as_str()))
            {
                return Err(Error::new("DanglingOccurrence", "target absent"));
            }
        }
        if o.owner
            .as_ref()
            .is_some_and(|id| !ids.contains(id.as_str()) && !bodies.contains(id.as_str()))
        {
            return Err(Error::new("DanglingOccurrence", "owner absent"));
        }
    }
    for r in &snapshot.relations {
        if !ids.contains(r.from.as_str())
            || !ids.contains(r.to.as_str())
            || r.occurrence
                .as_ref()
                .is_some_and(|id| !occ.contains(id.as_str()))
        {
            return Err(Error::new("DanglingRelation", "endpoint/occurrence absent"));
        }
        if let Some(a) = &r.source {
            anchor(a)?;
        }
    }
    for d in &snapshot.documents {
        anchor(&d.source)?;
        if d.owner
            .as_ref()
            .is_some_and(|id| !ids.contains(id.as_str()))
        {
            return Err(Error::new("DanglingDocument", "owner absent"));
        }
    }
    for trace in &snapshot.runtime_traces {
        if !ids.contains(trace.from.as_str())
            || !ids.contains(trace.to.as_str())
            || trace.count == 0
        {
            return Err(Error::new("InvalidTrace", "runtime trace endpoint/count"));
        }
        content.read_blob(&trace.raw)?;
    }
    let mut check = snapshot.clone();
    check.seal()?;
    if !snapshot.revision.is_empty() && snapshot.revision != check.revision {
        return Err(Error::new("DigestMismatch", "semantic revision digest"));
    }
    Ok(())
}
