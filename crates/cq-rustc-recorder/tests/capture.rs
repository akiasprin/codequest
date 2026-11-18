use cq_capture_protocol as p;
use cq_core::analyst::rust::RustBuildRequest;
use cq_core::{
    analyst::{
        BuildObserver,
        rust::{RustBuildObserver, select_context, validate_report},
    },
    control::CaptureControl,
    model::*,
    ports::*,
};
use cq_store::{LocalStore, journal::LocalCapture};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project with spaces");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/compile-workspace"),
        &project,
    );
    let output = temp.path().join("capture");
    (temp, project, output)
}
fn request(project: &Path, output: &Path, args: &[&str]) -> RustBuildRequest {
    RustBuildRequest {
        project: project.into(),
        output: output.into(),
        cargo: "cargo".into(),
        recorder: env!("CARGO_BIN_EXE_cq-rustc-recorder").into(),
        args: args.iter().map(|s| (*s).into()).collect(),
        mode: "exclusive".into(),
        reuse_target: None,
        extra_env_keys: vec![],
        budget: CaptureBudget::default(),
    }
}
fn capture(request: RustBuildRequest, control: CaptureControl) -> (CaptureReport, LocalStore) {
    let observer = RustBuildObserver;
    let plan = observer.plan(request).unwrap();
    let store = LocalStore::open(&plan.output.path()).unwrap();
    let journal = Arc::new(
        LocalCapture::open(
            store.run_path(&plan.run_id).unwrap(),
            &plan.run_id,
            plan.budget.clone(),
        )
        .unwrap(),
    );
    let report = observer
        .observe(plan, journal, &store.content, &control)
        .unwrap();
    store.save_report(&report).unwrap();
    (report, store)
}
fn artifact_set(r: &CaptureReport) -> Vec<(String, Vec<String>, bool, Vec<String>)> {
    let mut s: Vec<_> = r
        .artifacts
        .iter()
        .map(|a| {
            (
                a.target_name.clone(),
                a.target_kinds.clone(),
                a.profile.test,
                a.features.clone(),
            )
        })
        .collect();
    s.sort();
    s
}

#[test]
fn a02_invalid_requests_have_no_build_side_effects() {
    let (_t, project, output) = fixture();
    for args in [
        vec!["run"],
        vec!["doc"],
        vec!["test"],
        vec!["check", "--target-dir", "bad"],
        vec!["check", "--config", "a=1"],
        vec!["check", "--message-format=human"],
        vec!["check", "--release=bad"],
        vec!["check", "--release", "--profile=dev"],
    ] {
        assert!(
            RustBuildObserver
                .plan(request(&project, &output, &args))
                .is_err()
        );
    }
    let mut r = request(&project, &output, &["check"]);
    r.mode = "preserve".into();
    assert!(RustBuildObserver.plan(r).is_err());
    let mut r = request(&project, &output, &["check"]);
    r.budget.frame_bytes = 0;
    assert!(RustBuildObserver.plan(r).is_err());
    let mut r = request(&project, &output, &["check"]);
    r.output = project.join("output");
    assert!(RustBuildObserver.plan(r).is_err());
    let mut r = request(&project, &output, &["check"]);
    r.project = project.join("missing");
    assert!(RustBuildObserver.plan(r).is_err());
    assert!(!output.exists());
    assert!(!project.join("target").exists());
}
#[test]
fn a06_a08_a11_a13_a17_a25_a27_real_cold_and_warm_workspace() {
    let (temp, project, output) = fixture();
    // Independent Cargo artifact expectation, without the recorder.
    let base = Command::new("cargo")
        .current_dir(&project)
        .args(["check", "--workspace", "--offline", "--message-format=json"])
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("CARGO_TARGET_DIR", temp.path().join("baseline"))
        .output()
        .unwrap();
    assert!(
        base.status.success(),
        "{}",
        String::from_utf8_lossy(&base.stderr)
    );
    let mut expected = vec![];
    for line in base.stdout.split(|b| *b == b'\n') {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line)
            && v["reason"] == "compiler-artifact"
        {
            expected.push((
                v["target"]["name"].as_str().unwrap().to_owned(),
                serde_json::from_value::<Vec<String>>(v["target"]["kind"].clone()).unwrap(),
                v["profile"]["test"].as_bool().unwrap(),
                serde_json::from_value::<Vec<String>>(v["features"].clone()).unwrap(),
            ));
        }
    }
    expected.sort();
    let (cold, store) = capture(
        request(&project, &output, &["check", "--workspace", "--offline"]),
        CaptureControl::default(),
    );
    assert_eq!(cold.exit_code(), 0, "{:?}", cold.gaps);
    assert_eq!(artifact_set(&cold), expected);
    assert!(
        cold.invocations
            .iter()
            .any(|i| i.classification == InvocationKind::Compile
                && i.execution == ExecutionObservation::KnownRustcStarted
                && i.finished.is_some())
    );
    assert!(cold.dependencies.len() >= 4);
    assert!(
        cold.dependencies
            .iter()
            .all(|d| d.producer.value().is_some())
    );
    let shared: Vec<_> = cold
        .units
        .iter()
        .filter(|u| {
            u.root
                .value()
                .is_some_and(|p| p.path() == project.join("shared.rs"))
        })
        .collect();
    assert_eq!(shared.len(), 2);
    assert_ne!(shared[0].id, shared[1].id);
    assert_ne!(
        shared[0].configuration.rust().unwrap().explicit_cfg,
        shared[1].configuration.rust().unwrap().explicit_cfg
    );
    assert!(
        cold.snapshot
            .files
            .iter()
            .any(|f| f.logical_path.path().ends_with("generated.rs"))
    );
    assert!(
        cold.snapshot
            .files
            .iter()
            .any(|f| f.logical_path.path().ends_with("label.txt"))
    );
    assert_eq!(cold.snapshot.binding, InputBinding::PrePostChecked);
    assert!(cold.units.iter().any(|u| u.role == UnitRole::BuildScript));
    assert!(cold.units.iter().any(|u| u.role == UnitRole::ProcMacro));
    assert!(cold.artifacts.len() >= cold.units.len());
    let saved = store.save_report(&cold).unwrap();
    assert_eq!(saved, store.save_report(&cold).unwrap());
    let mut conflict = cold.clone();
    conflict.duration_ms += 1;
    assert!(store.save_report(&conflict).is_err());
    let mut invalid = cold.clone();
    invalid.units[0].evidence[0].index = Some(usize::MAX);
    assert!(validate_report(&invalid, &store.content).is_err());
    let context = select_context(&cold, &cold.units[0].id).unwrap();
    assert!(context.partial);
    let mut req = request(&project, &output, &["check", "--workspace", "--offline"]);
    req.reuse_target = Some(cold.plan.target.path());
    let (warm, _) = capture(req, CaptureControl::default());
    assert_eq!(warm.exit_code(), 0, "{:?}", warm.gaps);
    assert!(warm.artifacts.iter().all(|a| a.fresh));
    assert!(
        warm.units
            .iter()
            .all(|u| u.binding == UnitBinding::ReusedArtifact && u.invocation.value().is_none())
    );
    assert!(
        !warm
            .invocations
            .iter()
            .any(|i| i.classification == InvocationKind::Compile)
    );
    let cold_scripts: Vec<_> = cold
        .build_messages
        .iter()
        .filter(|m| m["reason"] == "build-script-executed")
        .collect();
    let warm_scripts: Vec<_> = warm
        .build_messages
        .iter()
        .filter(|m| m["reason"] == "build-script-executed")
        .collect();
    assert!(!cold_scripts.is_empty() && !warm_scripts.is_empty());
    std::fs::write(project.join("shared.rs"), "// edited after capture").unwrap();
    let (_, old) = store.load_report(&cold.run.id).unwrap();
    assert_eq!(old.snapshot.id, cold.snapshot.id);
}
#[test]
fn a07_a09_a10_lib_bin_tests_and_host_target() {
    let (_temp, project, output) = fixture();
    let version = Command::new("rustc").arg("-vV").output().unwrap();
    let host = String::from_utf8(version.stdout)
        .unwrap()
        .lines()
        .find_map(|s| s.strip_prefix("host: "))
        .unwrap()
        .to_owned();
    for args in [
        vec!["check", "-p", "fixture-engine", "--lib", "--offline"],
        vec![
            "build",
            "-p",
            "fixture-cli",
            "--bin",
            "fixture-cli",
            "--offline",
        ],
        vec![
            "test",
            "--workspace",
            "--no-run",
            "--offline",
            "--target",
            host.as_str(),
        ],
    ] {
        let (report, _) = capture(request(&project, &output, &args), CaptureControl::default());
        assert_eq!(report.exit_code(), 0, "{:?}", report.gaps);
        assert_eq!(&report.plan.requested_args, &args);
        if args[0] == "test" {
            assert!(
                report
                    .units
                    .iter()
                    .any(|u| u.role == UnitRole::Test && u.configuration.rust().unwrap().test)
            );
            assert!(
                report
                    .units
                    .iter()
                    .any(|u| !u.configuration.rust().unwrap().test)
            );
            assert!(report.units.iter().any(|u| u.role == UnitRole::ProcMacro));
            assert!(report.units.iter().any(|u| u.role == UnitRole::BuildScript));
            for unit in &report.units {
                assert_eq!(unit.host.value(), Some(&host));
                assert_eq!(unit.target.value(), Some(&host));
            }
            let unit = report
                .units
                .iter()
                .find(|u| {
                    u.name.value().is_some_and(|s| s == "fixture_engine")
                        && u.configuration.rust().unwrap().test
                })
                .unwrap();
            assert!(
                unit.configuration
                    .rust()
                    .unwrap()
                    .explicit_cfg
                    .contains(&"fixture_active".into())
            );
            assert!(
                !unit
                    .configuration
                    .rust()
                    .unwrap()
                    .explicit_cfg
                    .iter()
                    .any(|s| s.contains("fixture_allowed_only"))
            );
            assert!(
                unit.configuration
                    .rust()
                    .unwrap()
                    .expected_cfg
                    .iter()
                    .any(|s| s.contains("fixture_allowed_only"))
            );
        }
    }
    assert!(!project.join("engine/TEST_WAS_RUN").exists());
}
#[test]
fn a12_exclusive_environment_is_explicit_and_project_config_is_untouched() {
    let (_temp, project, output) = fixture();
    std::fs::create_dir(project.join(".cargo")).unwrap();
    let config=b"[build]\nrustc-wrapper = '/does/not/exist/cache'\nrustc-workspace-wrapper = '/does/not/exist/workspace-cache'\n";
    std::fs::write(project.join(".cargo/config.toml"), config).unwrap();
    let old = std::env::var_os("RUSTC_WRAPPER");
    let (report, _) = capture(
        request(&project, &output, &["check", "--workspace", "--offline"]),
        CaptureControl::default(),
    );
    assert_eq!(report.exit_code(), 0, "{:?}", report.gaps);
    assert_eq!(
        std::fs::read(project.join(".cargo/config.toml")).unwrap(),
        config
    );
    assert_eq!(std::env::var_os("RUSTC_WRAPPER"), old);
    assert_eq!(report.plan.environment_changes.len(), 4);
}
#[test]
fn a15_real_compile_error_is_failed_not_capture_failure() {
    let (_temp, project, output) = fixture();
    std::fs::write(
        project.join("shared.rs"),
        "pub fn value() -> usize { missing_symbol }\n",
    )
    .unwrap();
    let (report, _) = capture(
        request(&project, &output, &["check", "--workspace", "--offline"]),
        CaptureControl::default(),
    );
    assert_eq!(report.run.outcome, BuildOutcome::Failed);
    assert_eq!(report.exit_code(), 1, "{:?}", report.gaps);
    assert!(report.invocations.iter().any(|i|i.finished.as_ref().is_some_and(|e|matches!(&e.event,Event::InvocationFinished{exit,..} if exit.code.is_some_and(|n|n!=0)))));
}

fn standalone(
    command: &str,
    args: &[&str],
    budget: CaptureBudget,
    input: Option<&[u8]>,
) -> (std::process::Output, Vec<Envelope>, SealReceipt) {
    let temp = tempfile::tempdir().unwrap();
    let run = p::new_id().unwrap();
    let c = LocalCapture::open(temp.path().join("run"), &run, budget.clone()).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cq-rustc-recorder"));
    cmd.arg(command)
        .args(args)
        .env(p::ENDPOINT_ENV, c.endpoint())
        .env(p::RUN_ENV, &run)
        .env(
            "CQ_CAPTURE_ARGUMENT_FILE_BYTES",
            budget.argument_file_bytes.to_string(),
        )
        .env(
            "CQ_CAPTURE_ARGUMENT_TOTAL_BYTES",
            budget.argument_total_bytes.to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().unwrap();
    let SealOutcome::Sealed(r) = c.seal().unwrap() else {
        panic!()
    };
    let events = c.read(&r, 0, 1000).unwrap().items;
    (output, events, r)
}
#[test]
fn a18_output_input_status_and_signal_are_preserved() {
    let (out, _, r) = standalone(
        "python3",
        &[
            "-c",
            "import sys; b=sys.stdin.buffer.read(); sys.stdout.buffer.write(b); sys.stderr.buffer.write(bytes([255,0,10])); sys.exit(19)",
        ],
        CaptureBudget::default(),
        Some(b"stdin\0bytes\n"),
    );
    assert_eq!(out.status.code(), Some(19));
    assert_eq!(out.stdout, b"stdin\0bytes\n");
    assert_eq!(out.stderr, [255, 0, 10]);
    assert_eq!(r.integrity, JournalIntegrity::Sound);
    let (out, events, _) = standalone(
        "python3",
        &[
            "-c",
            "import os,signal; os.kill(os.getpid(),signal.SIGTERM)",
        ],
        CaptureBudget::default(),
        None,
    );
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(libc::SIGTERM));
    assert!(events.iter().any(|e|matches!(&e.event,Event::InvocationFinished{exit,..} if exit.signal==Some(libc::SIGTERM))));
    let (out, events, _) = standalone("/not/a/compiler", &[], CaptureBudget::default(), None);
    assert_eq!(out.status.code(), Some(127));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::ProcessStarted { .. }))
    );
}
#[test]
fn a14_response_files_preserve_original_arguments_and_bounded_contents() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source with spaces.rs");
    std::fs::write(&source, "pub fn value() {}\n").unwrap();
    let file = temp.path().join("args");
    let contents = format!(
        "--crate-name\nresponse\n{}\n--crate-type\nlib\n--emit\nmetadata\n--out-dir\n{}\n--cfg\nresponse_cfg\n",
        source.display(),
        temp.path().display()
    );
    std::fs::write(&file, &contents).unwrap();
    let arg = format!("@{}", file.display());
    let (out, events, _) = standalone("rustc", &[&arg], CaptureBudget::default(), None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(events.iter().any(|e|matches!(&e.event,Event::ArgumentFileObserved{contents:Some(c),error:None,..} if c.0==contents.as_bytes())));
    assert!(events.iter().any(|e|matches!(&e.event,Event::InvocationStarted{args,..} if args==&vec![NativeValue::from(arg.as_str())])));
    let budget = CaptureBudget {
        argument_file_bytes: 4,
        ..CaptureBudget::default()
    };
    let (out, events, _) = standalone("rustc", &[&arg], budget, None);
    assert!(out.status.success());
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::ArgumentFileObserved {
            contents: None,
            error: Some(_),
            ..
        }
    )));
    let (out, events, _) = standalone(
        "rustc",
        &["@/does/not/exist"],
        CaptureBudget::default(),
        None,
    );
    assert!(!out.status.success());
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::ArgumentFileObserved { error: Some(_), .. }))
    );
}
#[test]
fn a20_missing_collector_fails_open_with_bounded_wait() {
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_cq-rustc-recorder"))
        .args(["python3", "-c", "print('kept'); raise SystemExit(19)"])
        .env(p::ENDPOINT_ENV, "/not/a/socket")
        .env(p::RUN_ENV, p::new_id().unwrap())
        .env("CQ_CAPTURE_REPORT_MS", "5")
        .env("CQ_CAPTURE_REPORT_TOTAL_MS", "15")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(19));
    assert_eq!(out.stdout, b"kept\n");
    assert!(out.stderr.is_empty());
    assert!(start.elapsed() < Duration::from_secs(2));
}

struct BrokenEndpoint {
    inner: LocalCapture,
}
impl CaptureJournal for BrokenEndpoint {
    fn endpoint(&self) -> &Path {
        Path::new("/not/a/capture/socket")
    }
    fn write_output(&self, s: OutputStream, b: &[u8]) -> cq_core::Result<()> {
        self.inner.write_output(s, b)
    }
    fn seal(&self) -> cq_core::Result<SealOutcome> {
        self.inner.seal()
    }
    fn recover(&self) -> cq_core::Result<SealOutcome> {
        self.inner.recover()
    }
    fn read(&self, r: &SealReceipt, o: usize, l: usize) -> cq_core::Result<EventPage> {
        self.inner.read(r, o, l)
    }
    fn output(&self, r: &SealReceipt, s: OutputStream) -> cq_core::Result<Vec<u8>> {
        self.inner.output(r, s)
    }
    fn health(&self) -> CaptureHealth {
        self.inner.health()
    }
}
#[test]
fn a20_compiler_success_is_independent_of_unavailable_capture_channel() {
    let (_temp, project, output) = fixture();
    let plan = RustBuildObserver
        .plan(request(
            &project,
            &output,
            &["check", "--workspace", "--offline"],
        ))
        .unwrap();
    let store = LocalStore::open(&output).unwrap();
    let capture = LocalCapture::open(
        store.run_path(&plan.run_id).unwrap(),
        &plan.run_id,
        plan.budget.clone(),
    )
    .unwrap();
    let report = RustBuildObserver
        .observe(
            plan,
            Arc::new(BrokenEndpoint { inner: capture }),
            &store.content,
            &CaptureControl::default(),
        )
        .unwrap();
    assert_eq!(report.run.outcome, BuildOutcome::Succeeded);
    assert_eq!(report.run.tool_exit.code, Some(0));
    assert_eq!(report.exit_code(), 2);
    assert!(report.invocations.is_empty());
    assert!(
        report
            .units
            .iter()
            .all(|u| u.binding == UnitBinding::UnattributedArtifact)
    );
    assert!(
        report
            .gaps
            .iter()
            .any(|g| g.code == "ArtifactProducerUnresolved")
    );
}
#[test]
fn a19_cancel_and_deadline_leave_unrelated_process_alive() {
    for cancel in [false, true] {
        let (_temp, project, output) = fixture();
        std::fs::write(project.join("engine/build.rs"),"fn main(){std::fs::write(\"SLOW_STARTED\", b\"started\").unwrap();std::thread::sleep(std::time::Duration::from_secs(30));}\n").unwrap();
        let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
        let control = CaptureControl::default();
        let mut req = request(&project, &output, &["check", "--workspace", "--offline"]);
        req.budget.cancel_grace_ms = 100;
        req.budget.build_timeout_ms = if cancel { 5000 } else { 400 };
        let marker = project.join("engine/SLOW_STARTED");
        let thread = if cancel {
            let c = control.clone();
            Some(std::thread::spawn(move || {
                let start = Instant::now();
                while !marker.exists() && start.elapsed() < Duration::from_secs(4) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                c.cancel();
            }))
        } else {
            None
        };
        let start = Instant::now();
        let (report, _) = capture(req, control);
        if let Some(t) = thread {
            t.join().unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(
            report.run.stop_reason.as_deref(),
            Some(if cancel {
                "user_cancelled"
            } else {
                "deadline_exceeded"
            })
        );
        assert_eq!(report.exit_code(), if cancel { 130 } else { 2 });
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }
}
