//! Opt-in replay of real scope ownership against the retained canonical spool.
//! Fixture preparation is excluded from the encoding measurement.
use super::*;
use std::{io::BufReader, path::PathBuf, time::Instant};

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Sample {
    pub key: String,
    pub scope: usize,
    pub offset: u64,
    pub reference: ContentRef,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Fixture {
    pub source: PathBuf,
    pub source_bytes: u64,
    pub ownership_revision: String,
    pub ownership_manifest_sha256: String,
    pub scopes: Vec<ScopeDescriptor>,
    pub records: Vec<Sample>,
    pub counts: BTreeMap<String, u64>,
    pub preparation_seconds: f64,
    #[serde(default)]
    pub namespace_rebindings: Vec<Value>,
    #[serde(default)]
    pub selection: Value,
}
pub(crate) fn progress(phase: &str, completed: u64, total: u64) {
    if let Ok(path) = std::env::var("CQ_ENCODING_PROGRESS") {
        let path = PathBuf::from(path);
        let next = path.with_extension("next");
        std::fs::write(
            &next,
            serde_json::to_vec(&json!({"phase":phase,"completed":completed,"total":total}))
                .unwrap(),
        )
        .unwrap();
        std::fs::rename(next, path).unwrap();
    }
}

#[test]
#[ignore = "retained Brocade data: CQ_ENCODING_SOURCE_MANIFEST, CQ_ENCODING_OBJECTS, CQ_ENCODING_CANONICAL_SPOOL, CQ_ENCODING_FIXTURE"]
fn prepare_retained_scoped_encoding_fixture() {
    let started = Instant::now();
    let output = PathBuf::from(std::env::var("CQ_ENCODING_FIXTURE").unwrap());
    assert!(!output.exists(), "preserve existing fixtures");
    let manifest = std::fs::read(std::env::var("CQ_ENCODING_SOURCE_MANIFEST").unwrap()).unwrap();
    let revision: Revision = serde_json::from_slice(&manifest).unwrap();
    revision.verify().unwrap();
    let objects =
        Objects::new(PathBuf::from(std::env::var("CQ_ENCODING_OBJECTS").unwrap())).unwrap();
    let source = PathBuf::from(std::env::var("CQ_ENCODING_CANONICAL_SPOOL").unwrap());
    let target: u64 = std::env::var("CQ_ENCODING_PER_FAMILY")
        .unwrap_or_else(|_| "100000".into())
        .parse()
        .unwrap();
    assert!((1000..=1000000).contains(&target));
    let window = std::env::var("CQ_ENCODING_SCOPE_START").ok().map(|v| {
        (
            v.parse::<u64>().unwrap(),
            std::env::var("CQ_ENCODING_SCOPE_ROWS")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
        )
    });
    let context: Option<Vec<Value>> = std::env::var("CQ_ENCODING_NAMESPACE_CONTEXT")
        .ok()
        .map(|p| serde_json::from_reader(BufReader::new(std::fs::File::open(p).unwrap())).unwrap());
    progress("fixture.scope_ownership", 0, 1);
    let group: Group = serde_json::from_value(
        tree::get(
            &objects,
            &revision.roots.groups,
            "all:scope",
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap(),
    )
    .unwrap();
    let mut candidates = Vec::new();
    tree::visit(&objects, &group.root, |_, value| {
        let reference: FactRef = serde_json::from_value(value.clone())?;
        let value = writer::decode(&objects, &reference, &mut ReadStats::default())?;
        let descriptor: ScopeDescriptor = serde_json::from_value(value["descriptor"].clone())?;
        if descriptor.completion == Completion::Complete
            && descriptor.rows > 0
            && (window.is_some() || descriptor.rows <= target)
        {
            let members: Root = serde_json::from_value(value["members"].clone())?;
            candidates.push((descriptor, members));
        }
        Ok(())
    })
    .unwrap();
    // Whole, original scopes; never trim a descriptor to fit a desired result.
    if window.is_some() {
        candidates.sort_by(|a, b| a.0.key.cmp(&b.0.key));
    } else {
        candidates.sort_by(|a, b| b.0.rows.cmp(&a.0.rows).then(a.0.key.cmp(&b.0.key)));
    }
    let mut counts = BTreeMap::<String, u64>::new();
    let mut selected = Vec::new();
    let mut ordinal = 0;
    let mut selected_rows = 0;
    let mut first_selected = None;
    for (descriptor, members) in candidates {
        let start = ordinal;
        ordinal += descriptor.rows;
        if let Some((from, rows)) = window
            && (ordinal <= from || selected_rows >= rows)
        {
            continue;
        }
        let family = format!("{:?}", descriptor.key.family).to_lowercase();
        let count = counts.entry(family).or_default();
        if window.is_some() || *count + descriptor.rows <= target {
            first_selected.get_or_insert(start);
            selected_rows += descriptor.rows;
            *count += descriptor.rows;
            selected.push((descriptor, members));
        }
    }
    for family in ["entity", "body", "occurrence", "relation", "document"] {
        assert!(
            counts.get(family).copied().unwrap_or(0)
                >= if window.is_some() { 1 } else { target / 2 },
            "unrepresentative family: {family}"
        );
    }
    selected.sort_by(|a, b| a.0.key.cmp(&b.0.key));
    let scopes = selected.iter().map(|v| v.0.clone()).collect::<Vec<_>>();
    let mut wanted = BTreeMap::<String, Vec<(String, usize, ContentRef)>>::new();
    for (index, (_, members)) in selected.iter().enumerate() {
        tree::visit(&objects, members, |key, value| {
            let reference: FactRef = serde_json::from_value(value.clone())?;
            let id = key.split_once(':').unwrap().1;
            let bucket = wanted.entry(id.into()).or_default();
            assert!(!bucket.iter().any(|r| r.0 == key), "duplicate fact key");
            bucket.push((
                key.into(),
                index,
                ContentRef {
                    hash: reference.digest,
                    bytes: reference.length,
                },
            ));
            Ok(())
        })
        .unwrap();
    }
    let expected = wanted.values().map(Vec::len).sum::<usize>();
    assert_eq!(expected as u64, counts.values().sum::<u64>());
    let source_bytes = source.metadata().unwrap().len();
    let selection = json!({"mode":if window.is_some(){"contiguous_production_scope_window"}else{"balanced_families"},
        "first_scope_row_ordinal":first_selected,"records":expected,"requested_window":window});
    let mut stream = serde_json::Deserializer::from_reader(BufReader::with_capacity(
        8 * 1024 * 1024,
        std::fs::File::open(&source).unwrap(),
    ))
    .into_iter::<Value>();
    let mut records = Vec::with_capacity(expected);
    let mut namespace_candidates = Vec::new();
    let mut tick = Instant::now();
    while !wanted.is_empty() {
        let offset = stream.byte_offset() as u64;
        let Some(value) = stream.next() else { break };
        let value = value.unwrap();
        let lookup_id = if value["kind"] == "Project" {
            context
                .as_ref()
                .and_then(|c| c.iter().find(|r| r["actual"]["kind"] == "Project"))
                .and_then(|r| r["old"]["key"].as_str())
                .and_then(|k| k.strip_prefix("entity:"))
                .or_else(|| value["id"].as_str())
        } else {
            value["id"].as_str()
        };
        if let Some(id) = lookup_id
            && let Some(bucket) = wanted.get_mut(id)
        {
            let bytes = serde_json::to_vec(&value).unwrap();
            // The published scope is an independent ownership/hash oracle for
            // the current run's actual disk payload, not a generated substitute.
            let hash = digest(&bytes);
            if let Some(index) = bucket
                .iter()
                .position(|r| r.2.hash == hash && r.2.bytes == bytes.len() as u64)
            {
                let (key, scope, reference) = bucket.remove(index);
                assert_eq!(stream.byte_offset() as u64 - offset, reference.bytes);
                records.push(Sample {
                    key,
                    scope,
                    offset,
                    reference,
                });
            } else if matches!(value["kind"].as_str(), Some("Project" | "CompilationUnit"))
                && let Some((key, scope, reference)) =
                    bucket.iter().find(|r| r.0.starts_with("entity:"))
            {
                namespace_candidates.push(json!({"old":{"key":key,"scope":scopes[*scope],"reference":reference},
                    "found":true,"offset":offset,"bytes":stream.byte_offset() as u64-offset,"actual":value}));
            }
            if bucket.is_empty() {
                let id = id.to_owned();
                wanted.remove(&id);
            }
        }
        if tick.elapsed().as_millis() >= 500 {
            progress(
                "fixture.scan_canonical_spool",
                stream.byte_offset() as u64,
                source_bytes,
            );
            tick = Instant::now();
        }
    }
    if !wanted.is_empty() {
        let missing = wanted.values().flatten().map(|(key,scope,reference)|
            json!({"key":key,"scope":scopes[*scope],"reference":reference})).collect::<Vec<_>>();
        std::fs::write(
            output.with_extension("missing.json"),
            serde_json::to_vec_pretty(&missing).unwrap(),
        )
        .unwrap();
        serde_json::to_writer(
            std::io::BufWriter::new(
                std::fs::File::create_new(output.with_extension("incomplete.json")).unwrap(),
            ),
            &Fixture {
                source: source.clone(),
                source_bytes,
                ownership_revision: revision.revision.clone(),
                ownership_manifest_sha256: digest(&manifest),
                scopes: scopes.clone(),
                records,
                counts,
                preparation_seconds: started.elapsed().as_secs_f64(),
                namespace_rebindings: vec![],
                selection: selection.clone(),
            },
        )
        .unwrap();
        if context.is_some() {
            assert_eq!(
                namespace_candidates.len(),
                wanted.values().map(Vec::len).sum::<usize>(),
                "unexplained mismatches cannot be rebound"
            );
            std::fs::write(
                output.with_extension("namespace-candidates.json"),
                serde_json::to_vec_pretty(&namespace_candidates).unwrap(),
            )
            .unwrap();
            bind_retained_fixture_to_current_project_namespace();
            return;
        }
        panic!(
            "{} original members absent; incomplete offsets retained for diagnosis, not a valid fixture",
            wanted.values().map(Vec::len).sum::<usize>()
        );
    }
    assert!(
        wanted.is_empty(),
        "{} original scope members absent in the current spool",
        wanted.len()
    );
    records.sort_by(|a, b| a.key.cmp(&b.key));
    let fixture = Fixture {
        source,
        source_bytes,
        ownership_revision: revision.revision,
        ownership_manifest_sha256: digest(&manifest),
        scopes,
        records,
        counts,
        preparation_seconds: started.elapsed().as_secs_f64(),
        namespace_rebindings: vec![],
        selection,
    };
    serde_json::to_writer(
        std::io::BufWriter::new(std::fs::File::create_new(output).unwrap()),
        &fixture,
    )
    .unwrap();
    progress("fixture.complete", expected as u64, expected as u64);
}

#[test]
#[ignore = "CQ_ENCODING_FIXTURE: finalize diagnosed Project/CompilationUnit namespace changes; all other differences remain errors"]
fn bind_retained_fixture_to_current_project_namespace() {
    use std::os::unix::fs::FileExt;
    let output = PathBuf::from(std::env::var("CQ_ENCODING_FIXTURE").unwrap());
    assert!(!output.exists());
    let mut fixture: Fixture = serde_json::from_reader(BufReader::new(
        std::fs::File::open(output.with_extension("incomplete.json")).unwrap(),
    ))
    .unwrap();
    let differences: Vec<Value> = serde_json::from_reader(BufReader::new(
        std::fs::File::open(
            std::env::var("CQ_ENCODING_NAMESPACE_DIFFERENCES")
                .map(PathBuf::from)
                .unwrap_or_else(|_| output.with_extension("namespace-candidates.json")),
        )
        .unwrap(),
    ))
    .unwrap();
    let manifest = std::fs::read(std::env::var("CQ_ENCODING_SOURCE_MANIFEST").unwrap()).unwrap();
    assert_eq!(digest(&manifest), fixture.ownership_manifest_sha256);
    let revision: Revision = serde_json::from_slice(&manifest).unwrap();
    revision.verify().unwrap();
    let objects =
        Objects::new(PathBuf::from(std::env::var("CQ_ENCODING_OBJECTS").unwrap())).unwrap();
    let context: Vec<Value> = std::env::var("CQ_ENCODING_NAMESPACE_CONTEXT")
        .ok()
        .map(|p| serde_json::from_reader(BufReader::new(std::fs::File::open(p).unwrap())).unwrap())
        .unwrap_or_else(|| differences.clone());
    let project = context
        .iter()
        .find(|d| d["actual"]["kind"] == "Project")
        .unwrap();
    let old_project_id = project["old"]["key"]
        .as_str()
        .unwrap()
        .strip_prefix("entity:")
        .unwrap();
    let new_project_id = project["actual"]["id"].as_str().unwrap();
    let new_project_name = project["actual"]["name"].as_str().unwrap();
    let mut project_bytes = vec![0; project["bytes"].as_u64().unwrap() as usize];
    std::fs::File::open(&fixture.source)
        .unwrap()
        .read_exact_at(&mut project_bytes, project["offset"].as_u64().unwrap())
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&project_bytes).unwrap(),
        project["actual"]
    );
    fn rebind(v: &mut Value, old_name: &str, new_name: &str, old_id: &str, new_id: &str) {
        match v {
            Value::String(s) if s == old_id => *s = new_id.into(),
            Value::String(s) if s == old_name => *s = new_name.into(),
            Value::String(s) if s.starts_with(&format!("{old_name}::")) => {
                *s = format!("{new_name}{}", &s[old_name.len()..])
            }
            Value::Array(a) => {
                for v in a {
                    rebind(v, old_name, new_name, old_id, new_id)
                }
            }
            Value::Object(o) => {
                for v in o.values_mut() {
                    rebind(v, old_name, new_name, old_id, new_id)
                }
            }
            _ => {}
        }
    }
    let mut old_members = fixture
        .scopes
        .iter()
        .map(|_| BTreeMap::new())
        .collect::<Vec<_>>();
    for r in &fixture.records {
        old_members[r.scope].insert(r.key.clone(), r.reference.clone());
    }
    let source = std::fs::File::open(&fixture.source).unwrap();
    for difference in &differences {
        assert_eq!(difference["found"], true);
        let key = difference["old"]["key"].as_str().unwrap();
        let descriptor: ScopeDescriptor =
            serde_json::from_value(difference["old"]["scope"].clone()).unwrap();
        let index = fixture
            .scopes
            .iter()
            .position(|s| s.key == descriptor.key)
            .unwrap();
        assert_eq!(fixture.scopes[index].digest, descriptor.digest);
        let old_reference: ContentRef =
            serde_json::from_value(difference["old"]["reference"].clone()).unwrap();
        assert!(
            old_members[index]
                .insert(key.into(), old_reference.clone())
                .is_none()
        );
        let (family, id) = key.split_once(':').unwrap();
        assert_eq!(family, "entity");
        let old_fact = writer::reference(
            &objects,
            &revision.roots,
            family,
            id,
            &mut ReadStats::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(old_fact.digest, old_reference.hash);
        let before = writer::decode(&objects, &old_fact, &mut ReadStats::default()).unwrap();
        assert!(matches!(
            before["kind"].as_str(),
            Some("Project" | "CompilationUnit")
        ));
        let mut expected = before.clone();
        rebind(
            &mut expected,
            &revision.project,
            new_project_name,
            old_project_id,
            new_project_id,
        );
        assert_eq!(
            expected, difference["actual"],
            "only a proven namespace substitution is permitted"
        );
        let offset = difference["offset"].as_u64().unwrap();
        let mut raw = vec![0; difference["bytes"].as_u64().unwrap() as usize];
        source.read_exact_at(&mut raw, offset).unwrap();
        let actual: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(actual, expected);
        let reference = ContentRef {
            hash: digest(&raw),
            bytes: raw.len() as u64,
        };
        let new_key = format!("entity:{}", actual["id"].as_str().unwrap());
        fixture.namespace_rebindings.push(json!({"before_key":key,"after_key":new_key,
            "before_reference":old_reference,"after_reference":reference,"before":before,"after":actual}));
        fixture.records.push(Sample {
            key: new_key,
            scope: index,
            offset,
            reference,
        });
    }
    for (i, scope) in fixture.scopes.iter().enumerate() {
        assert_eq!(old_members[i].len() as u64, scope.rows);
        assert_eq!(
            transfer::state_digest(&old_members[i]).unwrap(),
            scope.digest
        );
    }
    let mut current = fixture
        .scopes
        .iter()
        .map(|_| BTreeMap::new())
        .collect::<Vec<_>>();
    for r in &fixture.records {
        assert!(
            current[r.scope]
                .insert(r.key.clone(), r.reference.clone())
                .is_none()
        );
    }
    for (i, scope) in fixture.scopes.iter_mut().enumerate() {
        assert_eq!(current[i].len() as u64, scope.rows);
        scope.digest = transfer::state_digest(&current[i]).unwrap();
    }
    fixture.records.sort_by(|a, b| a.key.cmp(&b.key));
    let mut out = std::io::BufWriter::new(std::fs::File::create_new(output).unwrap());
    serde_json::to_writer(&mut out, &fixture).unwrap();
    out.flush().unwrap();
    progress(
        "fixture.complete",
        fixture.records.len() as u64,
        fixture.records.len() as u64,
    );
}

pub(crate) fn load() -> (Fixture, SemanticSnapshot, Spool) {
    let fixture: Fixture = serde_json::from_reader(BufReader::new(
        std::fs::File::open(std::env::var("CQ_ENCODING_FIXTURE").unwrap()).unwrap(),
    ))
    .unwrap();
    assert_eq!(
        fixture.source.metadata().unwrap().len(),
        fixture.source_bytes
    );
    let mut header = SemanticSnapshot::empty(
        "retained-encoding-replay".into(),
        NativeValue::from("/retained-brocade"),
    );
    header.production = Some(production::Production {
        scopes: fixture.scopes.clone(),
        streamed: true,
        ..Default::default()
    });
    let mut spool = Spool::new(&header).unwrap();
    // The path owns only a disposable placeholder. The read-only descriptor
    // refers to the retained spool; dropping the test cannot unlink its source.
    let placeholder = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let temporary = tempfile::NamedTempFile::from_parts(
        std::fs::File::open(&fixture.source).unwrap(),
        placeholder,
    );
    spool.file = Mutex::new(SpoolFile {
        file: temporary.as_file().try_clone().unwrap(),
        temporary: Some(temporary),
    });
    let scopes = fixture
        .scopes
        .iter()
        .map(|s| Arc::new(s.key.clone()))
        .collect::<Vec<_>>();
    for row in &fixture.records {
        let key: Arc<str> = Arc::from(row.key.as_str());
        let scope = scopes[row.scope].clone();
        assert!(
            spool
                .entries
                .insert(
                    key.clone(),
                    Entry {
                        offset: row.offset,
                        length: row.reference.bytes,
                        reference: row.reference.clone(),
                        scope: scope.clone(),
                        shard: Arc::from("retained")
                    }
                )
                .is_none()
        );
        spool
            .by_scope
            .entry(scope.as_ref().clone())
            .or_default()
            .push(key);
    }
    for (i, scope) in fixture.scopes.iter().enumerate() {
        let records = fixture
            .records
            .iter()
            .filter(|r| r.scope == i)
            .map(|r| (r.key.clone(), r.reference.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(records.len() as u64, scope.rows);
        assert_eq!(transfer::state_digest(&records).unwrap(), scope.digest);
    }
    (fixture, header, spool)
}
