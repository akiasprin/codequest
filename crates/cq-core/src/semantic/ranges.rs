use crate::model::SourceAnchor;
use std::collections::HashMap;
/// Interval lookup scoped by semantic instance and immutable file revision.
type RangeGroup = Vec<(SourceAnchor, usize, u64)>;
pub(super) struct RangeIndex {
    groups: HashMap<(String, String), RangeGroup>,
}
impl RangeIndex {
    pub fn new(rows: impl IntoIterator<Item = (String, SourceAnchor, usize)>) -> Self {
        let mut groups: HashMap<_, Vec<_>> = HashMap::new();
        for (unit, a, i) in rows {
            groups
                .entry((unit, a.file_revision.clone()))
                .or_default()
                .push((a, i, 0));
        }
        for rows in groups.values_mut() {
            rows.sort_by_key(|(a, _, _)| (a.start, a.end));
            let mut end = 0;
            for (a, _, prefix) in rows {
                end = end.max(a.end);
                *prefix = end;
            }
        }
        Self { groups }
    }
    pub fn nearest(&self, unit: &str, a: &SourceAnchor) -> Option<usize> {
        let rows = self.groups.get(&(unit.into(), a.file_revision.clone()))?;
        let mut at = rows.partition_point(|(r, _, _)| r.start <= a.start);
        let mut best = None;
        let mut width = u64::MAX;
        while at > 0 {
            at -= 1;
            let (r, id, prefix) = &rows[at];
            if *prefix < a.end {
                break;
            }
            if r.end >= a.end && r.end - r.start < width {
                best = Some(*id);
                width = r.end - r.start;
            }
        }
        best
    }
}
