use super::args;
use crate::{Error, Result, model::*, ports::ContentRepository};
pub fn validate_report(report: &CaptureReport, content: &dyn ContentRepository) -> Result<()> {
    crate::validation::validate_report(report, content)?;
    for u in &report.units {
        let config = u.configuration.rust().ok_or_else(|| {
            Error::new(
                "UnsupportedConfiguration",
                "Rust analyst requires Rust configuration",
            )
        })?;
        if let Some(id) = u.invocation.value() {
            let inv = report.invocations.iter().find(|i| &i.id == id).unwrap();
            let args = args::inspect(inv);
            if u.root.value() != args.root.as_ref()
                || u.name.value() != args.name.as_ref()
                || config.explicit_cfg != args.cfg
                || config.expected_cfg != args.check_cfg
                || config.test != args.test
                || config.options != args.raw
            {
                return Err(Error::new(
                    "InvalidEvidence",
                    "unit contradicts captured compiler arguments",
                ));
            }
            if let Some(evidence) = &config.compiler_cfg_evidence {
                if evidence.source != "report.probes" {
                    return Err(Error::new("InvalidEvidence", "cfg must refer to a probe"));
                }
                let probe = &report.probes[evidence.index.unwrap()];
                let start = inv
                    .started
                    .as_ref()
                    .ok_or_else(|| Error::new("InvalidEvidence", "cfg has no invocation start"))?;
                let Event::InvocationStarted { command, cwd, .. } = &start.event else {
                    unreachable!()
                };
                if &probe.command != command
                    || probe.cwd.value() != Some(cwd)
                    || probe.args.get(..2) != Some(&["--print".into(), "cfg".into()])
                {
                    return Err(Error::new(
                        "InvalidEvidence",
                        "cfg probe does not match invocation context",
                    ));
                }
                if config.compiler_cfg.value().is_some_and(|values| {
                    probe
                        .output
                        .text()
                        .map(|s| s.lines().map(str::to_owned).collect::<Vec<_>>())
                        .as_ref()
                        != Some(values)
                        || probe.exit.code != Some(0)
                        || probe.exit.spawn_error.is_some()
                }) {
                    return Err(Error::new(
                        "InvalidEvidence",
                        "cfg values contradict probe output",
                    ));
                }
            } else if config.compiler_cfg.value().is_some() {
                return Err(Error::new(
                    "InvalidEvidence",
                    "cfg values lack probe evidence",
                ));
            }
        }
    }
    for a in &report.artifacts {
        if a.evidence.source != "build_messages" {
            return Err(Error::new(
                "InvalidEvidence",
                "artifact evidence is not a Cargo message",
            ));
        }
        let raw = &report.build_messages[a.evidence.index.unwrap()];
        if raw["reason"] != "compiler-artifact"
            || raw["target"]["name"].as_str() != Some(a.target_name.as_str())
            || raw["fresh"].as_bool() != Some(a.fresh)
        {
            return Err(Error::new(
                "InvalidEvidence",
                "artifact contradicts Cargo message",
            ));
        }
        for alias in &a.output_aliases {
            let inv = report
                .invocations
                .iter()
                .find(|i| i.id == alias.invocation_id)
                .ok_or_else(|| Error::new("InvalidEvidence", "alias invocation missing"))?;
            if !a.paths.contains(&alias.reported_path)
                || !args::output_matches(&args::inspect(inv), &alias.compiler_output)
            {
                return Err(Error::new("InvalidEvidence", "invalid output alias"));
            }
        }
    }
    for d in &report.dependencies {
        let u = report.units.iter().find(|u| u.id == d.consumer).unwrap();
        let inv = u
            .invocation
            .value()
            .and_then(|id| report.invocations.iter().find(|i| &i.id == id))
            .ok_or_else(|| {
                Error::new("InvalidEvidence", "dependency has no captured invocation")
            })?;
        if !args::inspect(inv)
            .externs
            .iter()
            .any(|(alias, path)| alias == &d.alias && path == &d.path)
        {
            return Err(Error::new(
                "InvalidEvidence",
                "dependency contradicts captured --extern",
            ));
        }
    }
    Ok(())
}
