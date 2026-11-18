use crate::{Error, Result, model::*};
use std::collections::HashSet;
#[derive(Clone, Debug, serde::Serialize)]
pub struct ContextSelection {
    pub entry: String,
    pub units: Vec<String>,
    pub partial: bool,
    pub gaps: Vec<Gap>,
}
pub fn select_context(report: &CaptureReport, entry: &str) -> Result<ContextSelection> {
    if !report.units.iter().any(|u| u.id == entry) {
        return Err(Error::new("MissingUnit", entry));
    }
    let mut visited = HashSet::new();
    let mut queue = vec![entry.to_owned()];
    let mut gaps = vec![];
    while let Some(unit) = queue.pop() {
        if !visited.insert(unit.clone()) {
            continue;
        }
        for dep in report.dependencies.iter().filter(|d| d.consumer == unit) {
            match &dep.producer {
                Observed::Value(id) => queue.push(id.clone()),
                _ => gaps.push(Gap::new("DependencyUnresolved", &unit, &dep.alias)),
            }
        }
        if let Some(u) = report.units.iter().find(|u| u.id == unit)
            && (u.binding != UnitBinding::ObservedInvocation
                || u.configuration.coverage() == ConfigurationCoverage::Partial)
        {
            gaps.push(Gap::new(
                "ConfigurationPartial",
                &unit,
                "not a complete observed compilation context",
            ));
        }
    }
    gaps.push(Gap::new(
        "CompilerInputBindingPartial",
        &report.snapshot.id,
        "captured files are fixed; original compiler reads were not exhaustively observed",
    ));
    let mut units: Vec<_> = visited.into_iter().collect();
    units.sort();
    Ok(ContextSelection {
        entry: entry.into(),
        units,
        partial: true,
        gaps,
    })
}
