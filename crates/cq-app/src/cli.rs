use cq_core::analyst::rust::RustBuildRequest;
use cq_core::{Error, Result, model::*};
use std::{collections::HashMap, ffi::OsString, path::PathBuf};

pub enum Command {
    Run(RustBuildRequest),
    Inspect {
        output: PathBuf,
        run: String,
        section: String,
        limit: usize,
        cursor: Option<String>,
    },
}
pub fn parse(args: Vec<OsString>) -> Result<Command> {
    if args.len() < 2 || args[0] != "capture" {
        return Err(Error::new(
            "Usage",
            "cq capture run --project DIR --output DIR --mode exclusive -- cargo check | cq capture inspect --output DIR --run ID [--section units --limit 20 --cursor TOKEN]",
        ));
    }
    let action = args[1]
        .to_str()
        .ok_or_else(|| Error::new("Usage", "invalid command"))?;
    let mut values = HashMap::new();
    let mut i = 2;
    while i < args.len() && args[i] != "--" {
        let key = args[i]
            .to_str()
            .ok_or_else(|| Error::new("Usage", "option names must be UTF-8"))?
            .to_owned();
        let allowed = if action == "run" {
            &[
                "--project",
                "--output",
                "--mode",
                "--recorder",
                "--reuse-target",
                "--build-timeout-ms",
                "--probe-timeout-ms",
                "--journal-bytes",
                "--content-bytes",
                "--frame-bytes",
                "--queue-bytes",
                "--drain-timeout-ms",
                "--cancel-grace-ms",
                "--env-key",
            ][..]
        } else {
            &["--output", "--run", "--section", "--limit", "--cursor"][..]
        };
        if !allowed.contains(&key.as_str()) || i + 1 >= args.len() {
            return Err(Error::new(
                "Usage",
                format!("invalid/missing option: {key}"),
            ));
        }
        if values.insert(key.clone(), args[i + 1].clone()).is_some() {
            return Err(Error::new("Usage", format!("duplicate option: {key}")));
        }
        i += 2;
    }
    let required = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| Error::new("Usage", format!("missing {key}")))
    };
    let text = |key: &str| -> Result<String> {
        required(key)?
            .into_string()
            .map_err(|_| Error::new("Usage", format!("{key} must be UTF-8")))
    };
    let output = PathBuf::from(required("--output")?);
    if action == "run" {
        if i >= args.len() || args.len() <= i + 2 {
            return Err(Error::new(
                "Usage",
                "expected -- cargo check/build/test --no-run",
            ));
        }
        let cargo = PathBuf::from(&args[i + 1]);
        let cargo_args = args[i + 2..]
            .iter()
            .map(|s| {
                s.clone().into_string().map_err(|_| {
                    Error::new(
                        "Usage",
                        "Cargo options must be UTF-8; observed rustc arguments remain raw OS bytes",
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut budget = CaptureBudget::default();
        for (key, field) in [
            ("--build-timeout-ms", &mut budget.build_timeout_ms),
            ("--probe-timeout-ms", &mut budget.probe_timeout_ms),
            ("--journal-bytes", &mut budget.journal_bytes),
            ("--content-bytes", &mut budget.content_bytes),
            ("--drain-timeout-ms", &mut budget.drain_timeout_ms),
            ("--cancel-grace-ms", &mut budget.cancel_grace_ms),
        ] {
            if let Some(value) = values.get(key) {
                *field = value
                    .to_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::new("Usage", key))?;
            }
        }
        for (key, field) in [
            ("--frame-bytes", &mut budget.frame_bytes),
            ("--queue-bytes", &mut budget.queue_bytes),
        ] {
            if let Some(value) = values.get(key) {
                *field = value
                    .to_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::new("Usage", key))?;
            }
        }
        let recorder = values
            .get("--recorder")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_exe()?.with_file_name("cq-rustc-recorder"));
        Ok(Command::Run(RustBuildRequest {
            project: PathBuf::from(required("--project")?),
            output,
            cargo,
            recorder,
            args: cargo_args,
            mode: text("--mode")?,
            reuse_target: values.get("--reuse-target").map(PathBuf::from),
            extra_env_keys: values
                .get("--env-key")
                .map(|s| vec![s.to_string_lossy().into_owned()])
                .unwrap_or_default(),
            budget,
        }))
    } else if action == "inspect" {
        if i != args.len() {
            return Err(Error::new("Usage", "inspect has no trailing command"));
        }
        let limit = values
            .get("--limit")
            .map(|s| {
                s.to_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::new("Usage", "invalid limit"))
            })
            .transpose()?
            .unwrap_or(20);
        if limit == 0 || limit > 1000 {
            return Err(Error::new("InvalidBudget", "page size must be 1..1000"));
        }
        Ok(Command::Inspect {
            output,
            run: text("--run")?,
            section: values
                .get("--section")
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "units".into()),
            limit,
            cursor: values
                .get("--cursor")
                .map(|s| s.to_string_lossy().into_owned()),
        })
    } else {
        Err(Error::new("Usage", "expected run or inspect"))
    }
}
