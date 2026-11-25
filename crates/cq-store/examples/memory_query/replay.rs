//! Restrict a historical benchmark to an independently frozen source tree.
use super::*;

#[derive(Default)]
struct ArchiveContent(HashMap<String, Vec<u8>>);

impl ContentReader for ArchiveContent {
    fn read_blob(&self, id: &BlobId) -> Result<Vec<u8>> {
        self.0
            .get(&id.digest)
            .cloned()
            .ok_or_else(|| Error::new("SourceUnavailable", "blob not in verified archive"))
    }
}

fn restrict_resolution(resolution: &mut Resolution, targets: &HashSet<&str>) {
    match resolution {
        Resolution::Resolved(id) if !targets.contains(id.as_str()) => {
            *resolution = Resolution::Unknown("target outside verified view".into());
        }
        Resolution::Candidates(ids) => ids.retain(|id| targets.contains(id.as_str())),
        Resolution::Unknown(reason) => *reason = "unresolved in compiler capture".into(),
        _ => {}
    }
}

pub(super) fn load_verified(snapshot: &Path, archive: &Path) -> Result<Server> {
    let started = Instant::now();
    let old = decode(snapshot, started)?;
    let archive = archive.canonicalize()?;
    let root = old.root.path();
    let mut view = SemanticSnapshot::empty(old.project, old.root);
    view.revision = old.revision;
    view.gaps.push(Gap::new(
        "VerifiedReplaySubset",
        "repository graph",
        "Only graph facts anchored to verified archive files are included. External and unanchored facts, build metadata, and extracted documents are omitted. Counts apply to this view.",
    ));

    let mut files = Vec::new();
    let mut excluded = Vec::new();
    let mut external_files = 0;
    let mut content = ArchiveContent::default();
    for mut file in old.sources.files {
        let logical_path = file.logical_path.path();
        let Ok(relative) = logical_path.strip_prefix(&root) else {
            external_files += 1;
            continue;
        };
        let bytes = archive
            .join(relative)
            .canonicalize()
            .ok()
            .filter(|path| path.starts_with(&archive) && path.is_file())
            .map(std::fs::read)
            .transpose()?;
        let Some(bytes) = bytes.filter(|bytes| BlobId::of(bytes) == file.blob) else {
            if relative.extension().is_some_and(|ext| ext == "md")
                || relative.starts_with("mockups")
                    && relative.ends_with("src/capture/html-attrs.json")
            {
                excluded.push(relative.display().to_string());
                continue;
            }
            return Err(Error::new(
                "ArchiveMismatch",
                relative.display().to_string(),
            ));
        };
        content.0.entry(file.blob.digest.clone()).or_insert(bytes);
        file.origin = "verified replay archive".into();
        files.push(file);
    }
    view.sources = SourceSnapshot::seal(files, old.sources.binding, vec![]);
    let verified: HashMap<_, _> = view
        .sources
        .files
        .iter()
        .map(|f| (f.id.as_str(), f.blob.len))
        .collect();
    let valid_anchor = |anchor: &SourceAnchor| {
        anchor.start <= anchor.end
            && verified
                .get(anchor.file_revision.as_str())
                .is_some_and(|&len| anchor.end <= len)
    };
    view.entities = old.entities;
    view.entities.retain(|e| {
        !e.external
            && e.source.as_ref().is_some_and(valid_anchor)
            && e.extent.as_ref().is_none_or(valid_anchor)
            && e.locations.anchors().all(valid_anchor)
    });
    let entity_ids: HashSet<_> = view.entities.iter().map(|e| e.id.clone()).collect();
    let allowed_entity = |id: &str| entity_ids.contains(id);
    for entity in &mut view.entities {
        entity.parent.take_if(|parent| !allowed_entity(parent));
        entity.attributes.retain(|key, _| key == "test");
        entity.type_definitions.retain(|id| allowed_entity(id));
        entity.type_navigation.retain_mut(|nav| {
            nav.candidates.retain(|id| allowed_entity(id));
            valid_anchor(&nav.source) && nav.extent.as_ref().is_none_or(valid_anchor)
        });
    }
    view.bodies = old.bodies;
    view.bodies
        .retain(|b| allowed_entity(&b.owner) && valid_anchor(&b.source));
    let targets: HashSet<_> = view
        .entities
        .iter()
        .map(|e| e.id.as_str())
        .chain(view.bodies.iter().map(|b| b.id.as_str()))
        .collect();
    view.occurrences = old.occurrences;
    view.occurrences.retain_mut(|o| {
        if !valid_anchor(&o.source) || !o.locations.anchors().all(valid_anchor) {
            return false;
        }
        o.owner.take_if(|id| !targets.contains(id.as_str()));
        o.arguments.retain(valid_anchor);
        o.argument_bindings.retain_mut(|binding| {
            restrict_resolution(&mut binding.parameter, &targets);
            valid_anchor(&binding.source)
        });
        restrict_resolution(&mut o.declaration, &targets);
        restrict_resolution(&mut o.executable, &targets);
        true
    });
    let occurrence_ids: HashSet<_> = view.occurrences.iter().map(|o| o.id.as_str()).collect();
    view.relations = old.relations;
    view.relations.retain(|r| {
        targets.contains(r.from.as_str())
            && targets.contains(r.to.as_str())
            && r.source.as_ref().is_none_or(valid_anchor)
            && r.occurrence
                .as_ref()
                .is_none_or(|id| occurrence_ids.contains(id.as_str()))
    });
    view.provenance_records = old.provenance_records;
    for provenance in view.provenance_records.values_mut() {
        provenance.note.clear();
        provenance.build_run = None;
    }
    // Drop bulky fields (including extracted documents and raw build observations)
    // before building the query indices. Only the new view is given to the engine.
    drop(old.documents);
    drop(old.builds);
    drop(old.production);
    log(
        "load.verified_archive",
        started,
        json!({
            "excluded": excluded,
            "external_files": external_files,
            "retained": counts(&view),
        }),
    );
    index_snapshot(view, content, started)
}

pub(super) fn listen_verified(snapshot: &Path, archive: &Path, socket: &Path) -> Result<()> {
    let server = load_verified(snapshot, archive)?;
    listen(Box::leak(Box::new(server)), socket)
}

pub(super) fn bench_verified(
    snapshot: &Path,
    archive: &Path,
    calls: &Path,
    output: &Path,
) -> Result<()> {
    let server = load_verified(snapshot, archive)?;
    bench(&server, calls, output)
}
