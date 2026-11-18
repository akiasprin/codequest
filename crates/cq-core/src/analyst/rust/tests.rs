use super::*;
use crate::{Result, input::InputView, model::*, ports::*};
use std::{collections::HashMap, path::Path, sync::Mutex};

#[derive(Default)]
struct MemoryContent(Mutex<HashMap<String, Vec<u8>>>);
impl ContentReader for MemoryContent {
    fn read_blob(&self, id: &BlobId) -> Result<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .get(&id.digest)
            .cloned()
            .ok_or_else(|| crate::Error::new("MissingContent", "test content"))
    }
}
impl ContentRepository for MemoryContent {
    fn put(&self, b: &[u8], _: u64) -> Result<BlobId> {
        let id = BlobId::of(b);
        self.0.lock().unwrap().insert(id.digest.clone(), b.to_vec());
        Ok(id)
    }
    fn freeze(&self, path: &Path, origin: &str, budget: u64) -> Result<FileRevision> {
        let blob = self.put(&std::fs::read(path)?, budget)?;
        let logical_path = NativeValue::new(path);
        Ok(FileRevision {
            id: digest(&serde_json::to_vec(&(&logical_path, &blob))?),
            logical_path,
            blob,
            origin: origin.into(),
        })
    }
}
#[test]
fn a05_detected_input_changes_keep_before_bytes_and_changed_binding() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("lib.rs");
    std::fs::write(&p, "pub fn old() {}\n").unwrap();
    let repo = MemoryContent::default();
    let before = inputs::Inputs::before(t.path(), None, &repo, 10000);
    std::fs::write(&p, "pub fn new() {}\n").unwrap();
    let snapshot = before.after(std::iter::empty(), &[], &repo, 10000);
    assert_eq!(snapshot.binding, InputBinding::Changed);
    let view = InputView::open(snapshot, &repo).unwrap();
    let f = view.resolve_file(&NativeValue::new(&p)).unwrap();
    assert_eq!(
        view.read(
            &SourceAnchor {
                file_revision: f.id.clone(),
                start: 0,
                end: f.blob.len
            },
            1000
        )
        .unwrap(),
        b"pub fn old() {}\n"
    );
}
#[test]
fn a14_a16_unknown_options_and_ambiguous_producers_remain_explicit() {
    let run = "test-run";
    let mut events = vec![];
    for id in ["one", "two"] {
        let event = Event::InvocationStarted {
            recorder_pid: 1,
            command: NativeValue::from("rustc"),
            args: [
                "--crate-name",
                "same",
                "src/lib.rs",
                "--crate-type=lib",
                "--out-dir=/output",
                "-Cextra-filename=-hash",
                "--future-config=value",
            ]
            .into_iter()
            .map(NativeValue::from)
            .collect(),
            cwd: NativeValue::from("/project"),
            environment: vec![],
            removed_environment: vec![],
        };
        for (seq, event) in [
            (0, event),
            (1, Event::ProcessStarted { pid: 1 }),
            (
                2,
                Event::InvocationFinished {
                    exit: ExitObservation {
                        code: Some(0),
                        signal: None,
                        spawn_error: None,
                    },
                    reporting_errors: vec![],
                },
            ),
        ] {
            events.push(Envelope {
                protocol_version: 1,
                run_id: run.into(),
                invocation_id: id.into(),
                seq,
                time_ms: 1,
                event,
            });
        }
    }
    let mut invs = units::invocations(events);
    let mut artifacts = vec![BuildArtifact {
        id: "artifact".into(),
        package_id: "package".into(),
        target_name: "same".into(),
        target_kinds: vec!["lib".into()],
        src_path: NativeValue::from("/project/src/lib.rs"),
        profile: ArtifactProfile {
            opt_level: "0".into(),
            debug_assertions: true,
            overflow_checks: true,
            test: false,
        },
        features: vec![],
        paths: vec![NativeValue::from("/output/libsame-hash.rmeta")],
        fresh: false,
        output_aliases: vec![],
        producer: Observed::Unknown("unbound".into()),
        evidence: EvidenceRef {
            run_id: run.into(),
            source: "build_messages".into(),
            index: Some(0),
        },
    }];
    let tool = ToolchainObservation {
        command: NativeValue::from("rustc"),
        version: Observed::Value("rustc fixture".into()),
        host: Observed::Value("host".into()),
        sysroot: Observed::Unknown("fixture".into()),
        evidence: EvidenceRef {
            run_id: run.into(),
            source: "report.probes".into(),
            index: Some(0),
        },
    };
    let n = units::normalize(
        run,
        &mut invs,
        &mut artifacts,
        &HashMap::from([(NativeValue::from("rustc"), tool)]),
        &HashMap::new(),
        true,
    );
    assert!(matches!(&artifacts[0].producer,Observed::Ambiguous(ids) if ids.len()==2));
    assert_eq!(n.accounting, JobAccounting::Incomplete);
    assert!(
        n.units
            .iter()
            .filter(|u| u.binding == UnitBinding::ObservedInvocation)
            .all(|u| u.configuration.rust().unwrap().coverage == ConfigurationCoverage::Partial)
    );
    assert!(n.gaps.iter().any(|g| g.code == "UnknownCompilerOption"));
}
#[test]
fn a27_make_dependency_paths_preserve_escaped_spaces_and_continuations() {
    let deps = inputs::dep_inputs(b"out: src/a\\ b.rs \\\n src/c.rs\n\nsrc/a\\ b.rs:\n").unwrap();
    assert_eq!(deps, vec![b"src/a b.rs".to_vec(), b"src/c.rs".to_vec()]);
    assert!(inputs::dep_inputs(b"invalid format").is_none());
}
