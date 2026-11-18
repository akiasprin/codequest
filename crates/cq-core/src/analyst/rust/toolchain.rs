use super::process::probe;
use crate::model::*;
use std::process::Command;

pub(super) fn observe(
    command: NativeValue,
    cwd: &NativeValue,
    timeout: u64,
    probes: &mut Vec<ProbeObservation>,
    run: &str,
) -> ToolchainObservation {
    let mut cmd = Command::new(command.os());
    cmd.current_dir(cwd.path());
    let version = probe(cmd, vec!["-vV".into()], timeout);
    let index = probes.len();
    let text = version
        .output
        .text()
        .filter(|s| s.starts_with("rustc "))
        .filter(|_| version.exit.code == Some(0) && version.exit.spawn_error.is_none());
    let host = text
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("host: ")))
        .map(str::to_owned);
    let version_text = text.map(str::to_owned);
    probes.push(version);
    let mut sysroot = Observed::Unknown("unidentified compiler".into());
    if version_text.is_some() {
        let mut cmd = Command::new(command.os());
        cmd.current_dir(cwd.path());
        let p = probe(cmd, vec!["--print".into(), "sysroot".into()], timeout);
        if p.exit.code == Some(0) && p.exit.spawn_error.is_none() {
            let mut bytes = p.output.0.clone();
            while bytes.last().is_some_and(|c| *c == b'\n' || *c == b'\r') {
                bytes.pop();
            }
            sysroot = Observed::Value(NativeValue(bytes));
        }
        probes.push(p);
    }
    ToolchainObservation {
        command,
        version: version_text
            .map(Observed::Value)
            .unwrap_or_else(|| Observed::Unknown("-vV did not identify a working rustc".into())),
        host: host
            .map(Observed::Value)
            .unwrap_or_else(|| Observed::Unknown("host was not observed".into())),
        sysroot,
        evidence: EvidenceRef {
            run_id: run.into(),
            source: "report.probes".into(),
            index: Some(index),
        },
    }
}
pub(super) fn cfg(
    command: &NativeValue,
    cwd: &NativeValue,
    args: Vec<String>,
    timeout: u64,
    probes: &mut Vec<ProbeObservation>,
) -> Observed<Vec<String>> {
    let mut cmd = Command::new(command.os());
    cmd.current_dir(cwd.path());
    let mut options = vec!["--print".into(), "cfg".into()];
    options.extend(args);
    let p = probe(cmd, options, timeout);
    let value = if p.exit.code == Some(0) && p.exit.spawn_error.is_none() {
        p.output
            .text()
            .map(|s| s.lines().map(str::to_owned).collect())
            .map(Observed::Value)
            .unwrap_or_else(|| Observed::Unknown("non-UTF8 cfg output".into()))
    } else {
        Observed::Unknown("cfg probe failed or exceeded budget".into())
    };
    probes.push(p);
    value
}
