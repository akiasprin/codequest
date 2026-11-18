use crate::{Error, Result, model::*, ports::ContentRepository};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn index_repository(
    snapshot: &mut SemanticSnapshot,
    content: &dyn ContentRepository,
    max_bytes: u64,
) -> Result<()> {
    let root = snapshot.root.path();
    let mut paths = vec![];
    discover(&root, &mut paths, 0)?;
    let version = snapshot
        .providers
        .iter()
        .map(|p| p.version.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let provenance = Provenance {
        provider: "repository-documents".into(),
        version,
        authority: Authority::AuthorStatement,
        build_run: None,
        note: "repository text, not a generated attribution of intent".into(),
    };
    let repository_provenance = snapshot.intern_provenance(provenance.clone());
    for path in paths {
        parse_file(snapshot, &path, content, max_bytes, &provenance)?;
    }
    snapshot.sources = SourceSnapshot::seal(
        snapshot.sources.files.clone(),
        snapshot.sources.binding.clone(),
        snapshot.sources.gaps.clone(),
    );
    // Native doc ownership is retained; generic blocks and sections also become
    // graph entities so text retrieval, structural queries and docs share IDs.
    let docs = snapshot.documents.clone();
    let owners: std::collections::HashMap<_, _> = snapshot
        .entities
        .iter()
        .map(|e| (e.id.clone(), (e.unit.clone(), e.external)))
        .collect();
    let mut document_ids: std::collections::HashSet<_> = owners.keys().cloned().collect();
    for d in &docs {
        if document_ids.insert(d.id.clone()) {
            snapshot.entities.push(Entity {
                stable_handle: None,
                locations: Default::default(),
                id: d.id.clone(),
                unit: d
                    .owner
                    .as_ref()
                    .and_then(|id| owners.get(id))
                    .map(|(unit, _)| unit.clone())
                    .unwrap_or_default(),
                provider_id: d.id.clone(),
                kind: EntityKind::DocSection,
                name: d.title.clone(),
                qualified_name: d.title.clone(),
                source: Some(d.source.clone()),
                extent: Some(d.source.clone()),
                signature: String::new(),
                parent: d.parent.clone(),
                external: d
                    .owner
                    .as_ref()
                    .and_then(|id| owners.get(id))
                    .is_some_and(|(_, external)| *external),
                exported: false,
                attributes: BTreeMap::from([("search_text".into(), d.text.clone())]),
                type_definitions: vec![],
                type_navigation: vec![],
                provenance: d.provenance.clone(),
            });
        }
        if let Some(parent) = &d.parent {
            snapshot.add_relation(
                parent,
                &d.id,
                RelationKind::Contains,
                Some(d.source.clone()),
                None,
                d.provenance.clone(),
            );
        }
        if let Some(owner) = &d.owner {
            snapshot.add_relation(
                &d.id,
                owner,
                RelationKind::Documents,
                Some(d.source.clone()),
                None,
                d.provenance.clone(),
            );
        }
    }
    let file_nodes: std::collections::HashSet<_> = snapshot
        .entities
        .iter()
        .filter(|e| e.kind == EntityKind::File)
        .filter_map(|e| e.source.as_ref().map(|a| a.file_revision.clone()))
        .collect();
    for file in &snapshot.sources.files {
        if !file_nodes.contains(&file.id)
            && snapshot
                .documents
                .iter()
                .any(|d| d.file_revision == file.id)
        {
            let id = digest(&serde_json::to_vec(&(&file.id, "document-file"))?);
            snapshot.entities.push(Entity {
                stable_handle: None,
                locations: Default::default(),
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
                attributes: BTreeMap::new(),
                type_definitions: vec![],
                type_navigation: vec![],
                provenance: repository_provenance.clone(),
            });
        }
    }
    resolve_links(snapshot)?;
    Ok(())
}
fn discover(root: &Path, paths: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if depth > 48 || paths.len() > 100000 {
        return Err(Error::new("DiscoveryLimit", "document scope limit"));
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if ![
                ".git",
                "target",
                "node_modules",
                ".codequest",
                ".codequest-tools",
                "vendor",
            ]
            .iter()
            .any(|s| entry.file_name() == *s)
            {
                discover(&path, paths, depth + 1)?;
            }
        } else if path.extension().and_then(|s| s.to_str()).is_some_and(|e| {
            [
                "md", "markdown", "rst", "txt", "pdf", "adoc", "toml", "yml", "yaml", "json",
            ]
            .contains(&e.to_lowercase().as_str())
        }) || entry.file_name() == "README"
            || entry.file_name() == "LICENSE"
        {
            paths.push(path);
        }
    }
    Ok(())
}
struct DocumentSegment {
    kind: DocumentKind,
    title: String,
    text: String,
    range: std::ops::Range<usize>,
    parent: Option<String>,
}
fn add_block(
    snapshot: &mut SemanticSnapshot,
    file: &FileRevision,
    segment: DocumentSegment,
    p: &Provenance,
) -> String {
    let DocumentSegment {
        kind,
        title,
        text,
        range,
        parent,
    } = segment;
    let start = range.start;
    let end = range.end;
    let id = digest(&serde_json::to_vec(&(&file.id, start, end, kind)).expect("document identity"));
    let provenance = snapshot.intern_provenance(p.clone());
    snapshot.documents.push(DocumentBlock {
        id: id.clone(),
        file_revision: file.id.clone(),
        kind,
        title,
        text,
        source: SourceAnchor {
            file_revision: file.id.clone(),
            start: start as u64,
            end: end as u64,
        },
        owner: None,
        parent,
        links: vec![],
        provenance,
    });
    id
}
fn markdown(snapshot: &mut SemanticSnapshot, file: &FileRevision, text: &str, p: &Provenance) {
    let mut headings = vec![];
    let mut active = None;
    let mut links = vec![];
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active = Some((usize::from(level as u8), range.start, String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, title)) = &mut active {
                    title.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(h) = active.take() {
                    headings.push(h);
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                links.push((range.start, dest_url.to_string()))
            }
            _ => {}
        }
    }
    if headings.is_empty() || headings[0].1 > 0 {
        headings.insert(
            0,
            (
                0,
                0,
                file.logical_path
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    let mut stack: Vec<(usize, String)> = vec![];
    for (i, (level, start, title)) in headings.iter().enumerate() {
        while stack.last().is_some_and(|(l, _)| l >= level) {
            stack.pop();
        }
        let end = headings.get(i + 1).map(|h| h.1).unwrap_or(text.len());
        let id = add_block(
            snapshot,
            file,
            DocumentSegment {
                kind: DocumentKind::Markdown,
                title: title.clone(),
                text: text[*start..end].into(),
                range: *start..end,
                parent: stack.last().map(|(_, id)| id.clone()),
            },
            p,
        );
        let block = snapshot.documents.last_mut().unwrap();
        for (at, destination) in &links {
            if *at >= *start && *at < end {
                block.links.push(DocumentLink {
                    text: destination.clone(),
                    destination: destination.clone(),
                    target: Resolution::Unknown("not yet resolved".into()),
                    authority: Authority::AuthorStatement,
                });
            }
        }
        stack.push((*level, id));
    }
}
fn sections(
    snapshot: &mut SemanticSnapshot,
    file: &FileRevision,
    kind: DocumentKind,
    text: &str,
    p: &Provenance,
) {
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut sections = vec![(
        0,
        file.logical_path
            .path()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )];
    let mut offset = 0;
    if kind == DocumentKind::ReStructuredText {
        for (i, line) in lines.iter().enumerate() {
            if i + 1 < lines.len() {
                let underline = lines[i + 1].trim();
                if underline.len() >= line.trim().len()
                    && underline.len() >= 3
                    && underline.chars().all(|c| c == '=' || c == '-' || c == '~')
                {
                    sections.push((offset, line.trim().to_owned()));
                }
            }
            offset += line.len();
        }
    }
    sections.sort_by_key(|(p, _)| *p);
    sections.dedup_by_key(|(p, _)| *p);
    for (i, (start, title)) in sections.iter().enumerate() {
        let end = sections.get(i + 1).map(|s| s.0).unwrap_or(text.len());
        add_block(
            snapshot,
            file,
            DocumentSegment {
                kind,
                title: title.clone(),
                text: text[*start..end].into(),
                range: *start..end,
                parent: None,
            },
            p,
        );
    }
}
pub fn resolve_links(snapshot: &mut SemanticSnapshot) -> Result<()> {
    let file_paths: std::collections::HashMap<_, _> = snapshot
        .sources
        .files
        .iter()
        .map(|f| (f.id.clone(), f.logical_path.path()))
        .collect();
    let sections: Vec<_> = snapshot
        .documents
        .iter()
        .filter(|d| d.kind == DocumentKind::Markdown)
        .map(|d| {
            (
                d.file_revision.clone(),
                heading_slug(&d.title),
                d.id.clone(),
            )
        })
        .collect();
    let mut relations = vec![];
    for doc in &mut snapshot.documents {
        for link in &mut doc.links {
            if link.target.resolved().is_none() {
                let destination = link.destination.trim_matches('`');
                if destination.starts_with("http:") || destination.starts_with("https:") {
                    continue;
                }
                let (path, fragment) = destination.split_once('#').unwrap_or((destination, ""));
                let base = file_paths.get(&doc.file_revision).and_then(|p| p.parent());
                let target_path = base.map(|base| {
                    let raw = base.join(path);
                    lexical(&raw)
                });
                let candidates: Vec<_> = if !fragment.is_empty() {
                    let destination_file = if path.is_empty() {
                        file_paths.get(&doc.file_revision)
                    } else {
                        target_path.as_ref()
                    };
                    sections
                        .iter()
                        .filter(|(file, slug, _)| {
                            file_paths.get(file) == destination_file && slug == fragment
                        })
                        .map(|(_, _, id)| id.clone())
                        .collect()
                } else if !path.is_empty()
                    && target_path
                        .as_ref()
                        .is_some_and(|p| file_paths.values().any(|f| f == p))
                {
                    let p = target_path.as_ref().unwrap();
                    snapshot
                        .entities
                        .iter()
                        .filter(|e| {
                            e.source
                                .as_ref()
                                .and_then(|a| file_paths.get(&a.file_revision))
                                == Some(p)
                                && e.kind == EntityKind::File
                        })
                        .map(|e| e.id.clone())
                        .collect()
                } else {
                    let qualified = destination.trim_start_matches("crate::").replace('`', "");
                    snapshot
                        .entities
                        .iter()
                        .filter(|e| {
                            e.qualified_name == qualified
                                || e.qualified_name.ends_with(&format!("::{qualified}"))
                        })
                        .filter(|e| {
                            doc.owner
                                .as_ref()
                                .and_then(|id| snapshot.entities.iter().find(|x| &x.id == id))
                                .is_none_or(|owner| e.unit == owner.unit || e.external)
                        })
                        .map(|e| e.id.clone())
                        .collect()
                };
                link.target = if candidates.len() == 1 {
                    Resolution::Resolved(candidates[0].clone())
                } else if candidates.is_empty() {
                    Resolution::Unknown(format!("unresolved link or section anchor: {fragment}"))
                } else {
                    Resolution::Candidates(candidates)
                };
                link.authority = Authority::AuthorStatement;
            }
            if let Some(target) = link.target.resolved() {
                relations.push((
                    doc.id.clone(),
                    target.to_owned(),
                    doc.source.clone(),
                    Provenance {
                        authority: link.authority,
                        note: "explicit document reference; not a runtime relationship".into(),
                        ..snapshot.provenance_records[&doc.provenance].clone()
                    },
                ));
            }
        }
    }
    for (from, to, source, p) in relations {
        snapshot.add_relation(
            &from,
            &to,
            RelationKind::DocReferences,
            Some(source),
            None,
            p,
        );
    }
    Ok(())
}

pub(super) fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

pub(super) fn heading_slug(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .replace(' ', "-")
}

pub(super) fn repository_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![];
    discover(root, &mut paths, 0)?;
    Ok(paths)
}
pub(super) fn parse_file(
    snapshot: &mut SemanticSnapshot,
    path: &Path,
    content: &dyn ContentRepository,
    max_bytes: u64,
    provenance: &Provenance,
) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let kind = match extension.as_str() {
        "md" | "markdown" => DocumentKind::Markdown,
        "rst" => DocumentKind::ReStructuredText,
        "pdf" => DocumentKind::PdfPage,
        "toml" | "yaml" | "yml" | "json" => DocumentKind::Configuration,
        _ => DocumentKind::PlainText,
    };
    let revision = content.freeze(path, "repository_document", max_bytes)?;
    if let Some(previous) = snapshot
        .sources
        .files
        .iter()
        .find(|f| f.logical_path == revision.logical_path)
        && previous.blob != revision.blob
    {
        return Err(Error::new(
            "DocumentInputChanged",
            "document bytes differ from captured input; refresh required",
        ));
    }
    if !snapshot
        .sources
        .files
        .iter()
        .any(|f| f.logical_path == revision.logical_path)
    {
        snapshot.sources.files.push(revision.clone());
    }
    if kind == DocumentKind::PdfPage {
        let frozen_pdf = tempfile::NamedTempFile::new()?;
        std::fs::write(frozen_pdf.path(), content.read_blob(&revision.blob)?)?;
        let mut command = std::process::Command::new("pdftotext");
        command.arg("-layout").arg(frozen_pdf.path()).arg("-");
        let output = crate::process::output(
            command,
            &crate::control::CaptureControl::default(),
            30000,
            (max_bytes as usize).min(64 * 1024 * 1024),
        )?;
        if !output.status.success() {
            snapshot.gaps.push(Gap::new(
                "PdfExtractionFailed",
                path.display().to_string(),
                String::from_utf8_lossy(&output.stderr),
            ));
            return Ok(());
        }
        if output.stdout.len() as u64 > max_bytes {
            return Err(Error::new(
                "ContentLimit",
                "PDF extracted text exceeds budget",
            ));
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|e| Error::new("InvalidDocumentEncoding", e.to_string()))?;
        // PDF text offsets belong to the extracted text, not PDF byte offsets.
        let logical_path =
            NativeValue::from(format!("extracted-pdf://{}", path.display()).as_str());
        let blob = content.put(text.as_bytes(), max_bytes)?;
        let text_revision = FileRevision {
            id: digest(&serde_json::to_vec(&(&logical_path, &blob))?),
            logical_path,
            blob,
            origin: format!("pdftotext from {}", revision.id),
        };
        snapshot.sources.files.push(text_revision.clone());
        let mut start = 0;
        for (index, page) in text.split('\u{c}').enumerate() {
            if !page.trim().is_empty() {
                add_block(
                    snapshot,
                    &text_revision,
                    DocumentSegment {
                        kind: DocumentKind::PdfPage,
                        title: format!(
                            "{} — page {}",
                            path.file_name().unwrap().to_string_lossy(),
                            index + 1
                        ),
                        text: page.into(),
                        range: start..start + page.len(),
                        parent: None,
                    },
                    provenance,
                );
            }
            start += page.len() + 1;
        }
    } else {
        let bytes = content.read_blob(&revision.blob)?;
        let Ok(text) = String::from_utf8(bytes) else {
            snapshot.gaps.push(Gap::new(
                "DocumentEncodingUnknown",
                path.display().to_string(),
                "binary/non-UTF8 document preserved without decoded text",
            ));
            return Ok(());
        };
        if kind == DocumentKind::Markdown {
            markdown(snapshot, &revision, &text, provenance);
        } else {
            sections(snapshot, &revision, kind, &text, provenance);
        }
    }
    Ok(())
}
