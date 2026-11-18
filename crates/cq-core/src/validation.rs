//! Structural identity, fixed-content and evidence validation shared by stores.
use crate::{Error, Result, input::InputView, model::*, ports::ContentRepository};
use std::collections::HashSet;
pub fn validate_report(report: &CaptureReport, content: &dyn ContentRepository) -> Result<()> {
    if report.schema_version != 1 || report.plan.run_id != report.run.id {
        return Err(Error::new("InvalidReport", "schema/run mismatch"));
    }
    InputView::open(report.snapshot.clone(), content)?;
    let units: HashSet<_> = report.units.iter().map(|u| u.id.as_str()).collect();
    let invocations: HashSet<_> = report.invocations.iter().map(|u| u.id.as_str()).collect();
    let artifacts: HashSet<_> = report.artifacts.iter().map(|a| a.id.as_str()).collect();
    if units.len() != report.units.len()
        || invocations.len() != report.invocations.len()
        || artifacts.len() != report.artifacts.len()
    {
        return Err(Error::new("InvalidReport", "duplicate identities"));
    }
    let events = match &report.seal {
        SealOutcome::Sealed(r) => {
            if r.run_id != report.run.id {
                return Err(Error::new("InvalidEvidence", "receipt run"));
            }
            r.event_count
        }
        _ => 0,
    };
    let check = |e: &EvidenceRef| -> Result<()> {
        let limit = match e.source.as_str() {
            "events.log" => events,
            "build_messages" => report.build_messages.len(),
            "report.probes" => report.probes.len(),
            _ => return Err(Error::new("InvalidEvidence", "unknown evidence source")),
        };
        if e.run_id != report.run.id || e.index.is_none_or(|i| i >= limit) {
            return Err(Error::new(
                "InvalidEvidence",
                "evidence run or index out of bounds",
            ));
        }
        Ok(())
    };
    check(&report.preflight_toolchain.evidence)?;
    for tool in &report.toolchains {
        check(&tool.evidence)?;
        if tool.evidence.source != "report.probes" {
            return Err(Error::new(
                "InvalidEvidence",
                "tool identity must refer to a probe",
            ));
        }
        let probe = &report.probes[tool.evidence.index.unwrap()];
        if tool.command != probe.command
            || tool.version.value().is_some_and(|v| {
                probe.output.text() != Some(v.as_str())
                    || probe.exit.code != Some(0)
                    || probe.exit.spawn_error.is_some()
            })
        {
            return Err(Error::new(
                "InvalidEvidence",
                "tool identity contradicts probe",
            ));
        }
    }
    for inv in &report.invocations {
        for e in &inv.evidence {
            check(e)?;
        }
    }
    for u in &report.units {
        if u.configuration.language() != u.language {
            return Err(Error::new(
                "InvalidReport",
                "unit language/configuration mismatch",
            ));
        }
        if u.invocation
            .value()
            .is_some_and(|i| !invocations.contains(i.as_str()))
            || u.artifacts.iter().any(|a| !artifacts.contains(a.as_str()))
        {
            return Err(Error::new("InvalidReport", "dangling unit endpoint"));
        }
        for e in &u.evidence {
            check(e)?;
        }
        if let Some(e) = u.configuration.evidence() {
            check(e)?;
        }
    }
    for a in &report.artifacts {
        if a.producer
            .value()
            .is_some_and(|p| !units.contains(p.as_str()))
        {
            return Err(Error::new("InvalidReport", "dangling artifact producer"));
        }
        check(&a.evidence)?;
    }
    for d in &report.dependencies {
        if !units.contains(d.consumer.as_str())
            || d.producer
                .value()
                .is_some_and(|p| !units.contains(p.as_str()))
            || d.artifact
                .value()
                .is_some_and(|a| !artifacts.contains(a.as_str()))
        {
            return Err(Error::new("InvalidReport", "dangling dependency"));
        }
        for e in &d.evidence {
            check(e)?;
        }
    }
    for g in report.gaps.iter().chain(&report.snapshot.gaps) {
        for e in &g.evidence {
            check(e)?;
        }
    }
    Ok(())
}
