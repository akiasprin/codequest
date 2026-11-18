use super::RustBuildRequest;
use crate::{Error, Result, model::*};
use std::path::{Component, Path, PathBuf};

pub fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut clean = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    if clean.exists() {
        return Ok(clean.canonicalize()?);
    }
    let name = clean
        .file_name()
        .ok_or_else(|| Error::new("InvalidPath", "missing file name"))?
        .to_os_string();
    Ok(absolute(
        clean
            .parent()
            .ok_or_else(|| Error::new("InvalidPath", "missing parent"))?,
    )?
    .join(name))
}
pub fn plan(request: RustBuildRequest) -> Result<CapturePlan> {
    if request.mode != "exclusive" {
        return Err(Error::new(
            "UnsupportedMode",
            "only exclusive analysis builds are supported",
        ));
    }
    let project = request.project.canonicalize()?;
    if !project.join("Cargo.toml").is_file() {
        return Err(Error::new("InvalidRequest", "project requires Cargo.toml"));
    }
    let output = absolute(&request.output)?;
    if output.starts_with(&project) {
        return Err(Error::new(
            "InvalidRequest",
            "capture output must be outside project sources",
        ));
    }
    let b = &request.budget;
    if [
        b.build_timeout_ms,
        b.probe_timeout_ms,
        b.journal_bytes,
        b.content_bytes,
        b.drain_timeout_ms,
        b.report_timeout_ms,
        b.invocation_report_timeout_ms,
        b.cancel_grace_ms,
    ]
    .contains(&0)
        || b.frame_bytes == 0
        || b.frame_bytes > cq_capture_protocol::MAX_FRAME
        || b.queue_bytes == 0
        || b.argument_file_bytes == 0
        || b.argument_total_bytes == 0
        || b.journal_bytes > 1024 * 1024 * 1024
    {
        return Err(Error::new(
            "InvalidBudget",
            "budgets must be positive; frame limit is 8 MiB and journal limit is 1 GiB",
        ));
    }
    let recorder = request.recorder.canonicalize()?;
    if !recorder.is_file() {
        return Err(Error::new("InvalidRequest", "recorder is not a file"));
    }
    let mut effective = request.args.clone();
    let base = usize::from(effective.first().is_some_and(|s| s.starts_with('+')));
    let command = effective
        .get(base)
        .map(String::as_str)
        .ok_or_else(|| Error::new("UnsupportedCommand", "missing Cargo command"))?;
    if !["check", "build", "test"].contains(&command) {
        return Err(Error::new("UnsupportedCommand", command));
    }
    if command == "test" && !effective.iter().any(|s| s == "--no-run") {
        return Err(Error::new("UnsupportedCommand", "test must have --no-run"));
    }
    let mut i = base + 1;
    let mut json = false;
    let mut release = false;
    let mut profile = false;
    while i < effective.len() {
        let arg = &effective[i];
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
        match flag {
            "--message-format" => {
                let value = if let Some(v) = inline {
                    v
                } else {
                    i += 1;
                    effective
                        .get(i)
                        .map(String::as_str)
                        .ok_or_else(|| Error::new("InvalidRequest", "missing message format"))?
                };
                if value != "json" || json {
                    return Err(Error::new(
                        "InvalidRequest",
                        "one JSON message format is required",
                    ));
                }
                json = true;
            }
            "-p" | "--package" | "--bin" | "--example" | "--test" | "--bench" | "--profile"
            | "--features" | "--target" | "--manifest-path" | "--jobs" | "-j" | "--exclude" => {
                let value = if let Some(v) = inline {
                    v.to_owned()
                } else {
                    i += 1;
                    effective.get(i).cloned().ok_or_else(|| {
                        Error::new("InvalidRequest", format!("missing value for {flag}"))
                    })?
                };
                if value.is_empty() || value.starts_with('-') {
                    return Err(Error::new(
                        "InvalidRequest",
                        format!("invalid value for {flag}"),
                    ));
                }
                if flag == "--profile" {
                    profile = true;
                }
                if flag == "--manifest-path" {
                    let manifest = Path::new(&value);
                    let p = if manifest.is_absolute() {
                        manifest.to_path_buf()
                    } else {
                        project.join(manifest)
                    };
                    if p.canonicalize()? != project.join("Cargo.toml") {
                        return Err(Error::new(
                            "InvalidRequest",
                            "manifest does not match project",
                        ));
                    }
                }
            }
            "--lib"
            | "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--all-features"
            | "--no-default-features"
            | "--locked"
            | "--offline"
            | "--frozen"
            | "--keep-going"
            | "--quiet"
            | "-q"
            | "--verbose"
            | "-v"
            | "-vv" => {
                if inline.is_some() {
                    return Err(Error::new("InvalidRequest", arg));
                }
            }
            "--no-run" if command == "test" && inline.is_none() => {}
            "--release" if inline.is_none() => release = true,
            _ => {
                return Err(Error::new(
                    "UnsupportedCommand",
                    format!("unsupported Cargo option: {arg}"),
                ));
            }
        }
        i += 1;
    }
    if release && profile {
        return Err(Error::new(
            "InvalidRequest",
            "--release conflicts with --profile",
        ));
    }
    if !json {
        effective.push("--message-format=json".into());
    }
    let run_id = cq_capture_protocol::new_id()?;
    let target = if let Some(t) = request.reuse_target {
        absolute(&t)?
    } else {
        output.join("targets").join(&run_id)
    };
    if target.starts_with(&project) {
        return Err(Error::new(
            "InvalidRequest",
            "analysis target must be outside sources",
        ));
    }
    let changes = [
        (
            "RUSTC_WRAPPER",
            NativeValue::new(&recorder),
            "exclusive capture recorder",
        ),
        (
            "RUSTC_WORKSPACE_WRAPPER",
            NativeValue::from(""),
            "disable nested/cache compiler wrappers in this analysis build",
        ),
        (
            "CARGO_TARGET_DIR",
            NativeValue::new(&target),
            "dedicated analysis artifacts",
        ),
        (
            "CARGO_BUILD_BUILD_DIR",
            NativeValue::new(&target),
            "keep intermediate artifacts in dedicated analysis directory",
        ),
    ]
    .into_iter()
    .map(|(key, effective, reason)| EnvironmentChange {
        key: key.into(),
        previous: std::env::var_os(key)
            .map(NativeValue::new)
            .map(Observed::Value)
            .unwrap_or_else(|| {
                Observed::Unknown("effective Cargo configuration is not reconstructed".into())
            }),
        effective,
        reason: reason.into(),
    })
    .collect();
    Ok(CapturePlan {
        run_id,
        project: NativeValue::new(project),
        output: NativeValue::new(output),
        target: NativeValue::new(target),
        build_tool: NativeValue::new(request.cargo),
        recorder_digest: digest(&std::fs::read(&recorder)?),
        recorder: NativeValue::new(recorder),
        requested_args: request.args,
        effective_args: effective,
        environment_changes: changes,
        extra_env_keys: request.extra_env_keys,
        budget: request.budget,
    })
}
