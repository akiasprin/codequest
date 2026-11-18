//! Ranking and context summaries change with actual semantic payload deltas.
use crate::{
    analysis::Statistics,
    facts::{
        objects::Objects,
        writer::{self, Revision},
    },
};
use cq_core::{Error, Result, model::*, ports::facts::ReadStats};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub struct Incremental {
    pub stats: Statistics,
    terms: BTreeMap<String, i64>,
    selected: BTreeMap<String, BTreeSet<String>>,
    old_terms: i64,
    term_delta: i64,
}

fn adjust(v: &mut Value, key: &str, n: i64) -> Result<()> {
    let next = v[key].as_u64().unwrap_or(0) as i64 + n;
    if next < 0 {
        return Err(Error::new("InvalidStatistics", "negative count"));
    }
    v[key] = json!(next);
    Ok(())
}
fn kind(m: &mut BTreeMap<String, u64>, key: &str, n: i64) -> Result<()> {
    let next = m.get(key).copied().unwrap_or(0) as i64 + n;
    if next < 0 {
        return Err(Error::new("InvalidStatistics", "negative kind count"));
    }
    if next == 0 {
        m.remove(key);
    } else {
        m.insert(key.into(), next as u64);
    }
    Ok(())
}
fn get(o: &Objects, r: &Revision, f: &str, id: &str) -> Result<Option<Value>> {
    writer::reference(o, &r.roots, f, id, &mut ReadStats::default())?
        .map(|p| writer::decode(o, &p, &mut ReadStats::default()))
        .transpose()
}
impl Incremental {
    pub fn new(
        o: &Objects,
        s: &SemanticSnapshot,
        r: Option<&Revision>,
        universe: &str,
    ) -> Result<Option<Self>> {
        let Some(r) = r.filter(|r| {
            r.metadata["scope_protocol"] == 2
                && r.metadata["semantic_generation"]["universe"] == universe
        }) else {
            return Ok(None);
        };
        let mut contexts = BTreeMap::new();
        let mut selected = BTreeMap::new();
        let prior_count = r.metadata["counts"]["contexts"].as_u64().unwrap_or(0);
        if prior_count != s.contexts.len() as u64 {
            return Ok(None);
        }
        for c in &s.contexts {
            let Some(old) = get(o, r, "context", &c.id)? else {
                return Ok(None);
            };
            if old["units"] != json!(c.units) || old["entry_unit"] != c.entry_unit {
                return Ok(None);
            }
            let Some(v) = get(o, r, "context_stats", &c.id)? else {
                return Ok(None);
            };
            contexts.insert(c.id.clone(), v);
            selected.insert(c.id.clone(), c.units.iter().cloned().collect());
        }
        let mut counts = r.metadata["counts"].clone();
        for (k, n) in [
            ("contexts", s.contexts.len()),
            ("units", s.units.len()),
            ("coverage", s.coverage.len()),
            ("gaps", s.gaps.len()),
        ] {
            counts[k] = json!(n);
        }
        let corpus = get(o, r, "search_stats", "corpus")?.unwrap_or(json!({"terms":0}));
        Ok(Some(Self {
            stats: Statistics {
                counts,
                entity_kinds: serde_json::from_value(r.metadata["entity_kinds"].clone())?,
                relation_kinds: serde_json::from_value(r.metadata["relation_kinds"].clone())?,
                df: BTreeMap::new(),
                terms: 0,
                contexts,
            },
            terms: BTreeMap::new(),
            selected,
            old_terms: corpus["terms"].as_i64().unwrap_or(0),
            term_delta: 0,
        }))
    }
    pub fn change(
        &mut self,
        family: &str,
        row: &Value,
        unit: &str,
        target_unit: Option<&str>,
        sign: i64,
    ) -> Result<()> {
        let count = match family {
            "entity" => "entities",
            "body" => "bodies",
            "occurrence" => "occurrences",
            "relation" => "relations",
            "document" => "documents",
            _ => return Ok(()),
        };
        adjust(&mut self.stats.counts, count, sign)?;
        let relation_kind = if family == "relation" {
            Some(serde_json::from_value::<RelationKind>(row["kind"].clone())?)
        } else {
            None
        };
        let k = relation_kind
            .map(RelationKind::label)
            .unwrap_or_else(|| row["kind"].as_str().unwrap_or(""));
        if family == "entity" {
            kind(&mut self.stats.entity_kinds, k, sign)?;
            if row["external"] == true {
                adjust(&mut self.stats.counts, "external", sign)?;
            }
            let e: Entity = serde_json::from_value(row.clone())?;
            let terms = cq_core::query::search::terms(&e);
            self.term_delta += sign * terms.len() as i64;
            for term in terms.into_iter().collect::<BTreeSet<_>>() {
                *self.terms.entry(term).or_default() += sign;
            }
        }
        if family == "relation" {
            kind(&mut self.stats.relation_kinds, k, sign)?;
        }
        if family == "occurrence" && k == "Calls" && row["executable"]["state"] != "Resolved" {
            adjust(&mut self.stats.counts, "unresolved_calls", sign)?;
        }
        for (id, units) in &self.selected {
            if !units.contains(unit)
                || family == "body"
                || family == "relation" && !target_unit.is_some_and(|u| units.contains(u))
            {
                continue;
            }
            let ctx = self.stats.contexts.get_mut(id).unwrap();
            adjust(&mut ctx["counts"], count, sign)?;
            let map = if family == "entity" {
                Some("entity_kinds")
            } else if family == "relation" {
                Some("relation_kinds")
            } else {
                None
            };
            if let Some(map) = map {
                let mut m = serde_json::from_value(ctx[map].clone())?;
                kind(&mut m, k, sign)?;
                ctx[map] = json!(m);
            }
        }
        Ok(())
    }
    pub fn finish(mut self, o: &Objects, r: &Revision) -> Result<Statistics> {
        let terms = self.old_terms + self.term_delta;
        if terms < 0 {
            return Err(Error::new("InvalidStatistics", "negative terms"));
        }
        self.stats.terms = terms as u64;
        for (term, delta) in self.terms {
            if delta == 0 {
                continue;
            }
            let old = get(o, r, "term_df", &term)?
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let new = old + delta;
            if new < 0 {
                return Err(Error::new(
                    "InvalidStatistics",
                    "negative document frequency",
                ));
            }
            self.stats.df.insert(term, new as u64);
        }
        Ok(self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relation_delta_uses_public_labels_and_both_context_endpoints() {
        let mut s = Incremental {
            stats: Statistics {
                counts: json!({"relations":1}),
                entity_kinds: BTreeMap::new(),
                relation_kinds: BTreeMap::from([("CALLS".into(), 1)]),
                df: BTreeMap::new(),
                terms: 0,
                contexts: BTreeMap::from([(
                    "c".into(),
                    json!({"counts":{"relations":1},"relation_kinds":{"CALLS":1}}),
                )]),
            },
            terms: BTreeMap::new(),
            selected: BTreeMap::from([("c".into(), BTreeSet::from(["local".into()]))]),
            old_terms: 0,
            term_delta: 0,
        };
        let row = json!({"kind":"Calls"});
        s.change("relation", &row, "local", Some("local"), -1)
            .unwrap();
        s.change("relation", &row, "local", Some("external"), 1)
            .unwrap();
        assert_eq!(s.stats.relation_kinds["CALLS"], 1);
        assert_eq!(s.stats.contexts["c"]["counts"]["relations"], 0);
        assert_eq!(s.stats.contexts["c"]["relation_kinds"], json!({}));
        s.change("relation", &row, "local", Some("external"), -1)
            .unwrap();
        assert!(s.stats.relation_kinds.is_empty());
        assert_eq!(s.stats.counts["relations"], 0);
    }
}
