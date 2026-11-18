use std::{
    path::{Path, PathBuf},
    process::Command,
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
fn recorder() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_cq")).with_file_name("cq-rustc-recorder")
}
fn run(project: &Path, output: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cq"))
        .args(["capture", "run", "--project"])
        .arg(project)
        .arg("--output")
        .arg(output)
        .args(["--mode", "exclusive", "--recorder"])
        .arg(recorder())
        .args(extra)
        .output()
        .unwrap()
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let project = t.path().join("project");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/compile-workspace"),
        &project,
    );
    let out = t.path().join("out");
    (t, project, out)
}
#[test]
fn a25_a26_cli_saved_report_pages_are_bound_and_progress() {
    let (_temp, project, output) = fixture();
    let out = run(
        &project,
        &output,
        &["--", "cargo", "check", "--workspace", "--offline"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = summary["run_id"].as_str().unwrap();
    for section in ["units", "invocations", "artifacts", "gaps", "inputs"] {
        let mut seen = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        let mut count = 0;
        loop {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_cq"));
            cmd.args(["capture", "inspect", "--output"])
                .arg(&output)
                .args(["--run", id, "--section", section, "--limit", "1"]);
            if let Some(c) = &cursor {
                cmd.args(["--cursor", c]);
            }
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let page: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(page["report_digest"], summary["report_digest"]);
            count += page["items"].as_array().unwrap().len();
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if let Some(c) = &cursor {
                assert!(seen.insert(c.clone()));
            } else {
                assert_eq!(count, page["total"].as_u64().unwrap() as usize);
                break;
            }
        }
    }
    let out = Command::new(env!("CARGO_BIN_EXE_cq"))
        .args(["capture", "inspect", "--output"])
        .arg(&output)
        .args(["--run", id, "--cursor", "ff"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    std::fs::write(
        project.join("shared.rs"),
        "pub fn value()->usize{unknown}\n",
    )
    .unwrap();
    let out = run(
        &project,
        &output,
        &["--", "cargo", "check", "--workspace", "--offline"],
    );
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["build_outcome"], "Failed");
    let out = run(
        &project,
        &output,
        &[
            "--journal-bytes",
            "1",
            "--",
            "cargo",
            "check",
            "--workspace",
            "--offline",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(serde_json::from_slice::<serde_json::Value>(&out.stdout).is_ok());
}
#[test]
fn a15_cli_explicit_unsupported_toolchain_is_not_started() {
    let (_temp, project, output) = fixture();
    let out = Command::new(env!("CARGO_BIN_EXE_cq"))
        .args(["capture", "run", "--project"])
        .arg(&project)
        .arg("--output")
        .arg(&output)
        .args(["--mode", "exclusive", "--recorder"])
        .arg(recorder())
        .args(["--", "cargo", "check", "--workspace", "--offline"])
        .env("RUSTC", "/does/not/exist/rustc")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["build_outcome"], "NotStarted");
    assert_eq!(r["invocations"], 0);
}
#[test]
fn a19_a26_cli_sigint_is_cancellation_not_build_error() {
    let (_temp, project, output) = fixture();
    std::fs::write(project.join("engine/build.rs"),"fn main(){std::fs::write(\"CLI_SLOW_STARTED\", b\"started\").unwrap();std::thread::sleep(std::time::Duration::from_secs(30));}").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cq"))
        .args(["capture", "run", "--project"])
        .arg(&project)
        .arg("--output")
        .arg(&output)
        .args(["--mode", "exclusive", "--recorder"])
        .arg(recorder())
        .args([
            "--cancel-grace-ms",
            "50",
            "--",
            "cargo",
            "check",
            "--workspace",
            "--offline",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !project.join("engine/CLI_SLOW_STARTED").exists()
        && start.elapsed() < Duration::from_secs(5)
    {
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["build_outcome"], "Cancelled");
    assert_eq!(r["stop_reason"], "user_cancelled");
}
