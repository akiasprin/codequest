use crate::model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
pub fn overview(s: &SemanticSnapshot) -> Vec<Value> {
    let mut kinds = BTreeMap::new();
    for e in &s.entities {
        *kinds.entry(e.kind.label()).or_insert(0usize) += 1;
    }
    vec![
        json!({"project":s.project,"revision":s.revision,"entities":s.entities.len(),"occurrences":s.occurrences.len(),"relations":s.relations.len(),"documents":s.documents.len(),"kinds":kinds,"context_count":s.contexts.len(),"contexts_query":{"operation":"contexts"},"build_count":s.builds.len(),"builds":s.builds.iter().take(7).map(|b|json!({"id":b.id,"language":b.language,"outcome":b.outcome,"exit":b.exit,"capture":b.captured_report})).collect::<Vec<_>>(),"input_binding":s.sources.binding,"explanation":"Static structure in named build contexts; it does not establish an actual execution or an author motive."}),
    ]
}
pub fn hotspots(s: &SemanticSnapshot) -> Vec<Value> {
    let mut ins = HashMap::new();
    let mut outs = HashMap::new();
    for e in s.relations.iter().filter(|r| r.kind == RelationKind::Calls) {
        *ins.entry(&e.to).or_insert(0usize) += 1;
        *outs.entry(&e.from).or_insert(0usize) += 1;
    }
    let mut result: Vec<_> = s
        .entities
        .iter()
        .filter_map(|e| {
            let input = *ins.get(&e.id).unwrap_or(&0);
            let output = *outs.get(&e.id).unwrap_or(&0);
            (input + output > 0)
                .then(|| json!({"id":e.id,"name":e.qualified_name,"fan_in":input,"fan_out":output}))
        })
        .collect();
    result.sort_by_key(|v| {
        std::cmp::Reverse(v["fan_in"].as_u64().unwrap_or(0) + v["fan_out"].as_u64().unwrap_or(0))
    });
    result
}
pub fn cycles(s: &SemanticSnapshot) -> Vec<Value> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for r in s.relations.iter().filter(|r| r.kind == RelationKind::Calls) {
        adj.entry(&r.from).or_default().push(&r.to);
        reverse.entry(&r.to).or_default().push(&r.from);
    }
    let mut visited = HashSet::new();
    let mut order = vec![];
    for e in &s.entities {
        if visited.contains(e.id.as_str()) {
            continue;
        }
        let mut stack = vec![(e.id.as_str(), false)];
        while let Some((id, done)) = stack.pop() {
            if done {
                order.push(id);
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            stack.push((id, true));
            for next in adj.get(id).into_iter().flatten() {
                if !visited.contains(next) {
                    stack.push((next, false));
                }
            }
        }
    }
    visited.clear();
    let mut result = vec![];
    for id in order.into_iter().rev() {
        if !visited.insert(id) {
            continue;
        }
        let mut component = vec![id];
        let mut queue = vec![id];
        while let Some(node) = queue.pop() {
            for next in reverse.get(node).into_iter().flatten() {
                if visited.insert(next) {
                    component.push(next);
                    queue.push(next);
                }
            }
        }
        if component.len() > 1 || adj.get(id).is_some_and(|a| a.contains(&id)) {
            component.sort();
            result
                .push(json!({"members":component,"meaning":"static strongly connected component"}));
        }
    }
    result
}
pub fn clusters(s: &SemanticSnapshot) -> Vec<Value> {
    let mut labels: HashMap<&str, &str> = s
        .entities
        .iter()
        .map(|e| (e.id.as_str(), e.id.as_str()))
        .collect();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for r in &s.relations {
        if matches!(
            r.kind,
            RelationKind::Calls | RelationKind::Contains | RelationKind::UsesType
        ) {
            adjacency.entry(&r.from).or_default().push(&r.to);
            adjacency.entry(&r.to).or_default().push(&r.from);
        }
    }
    for _ in 0..8 {
        let old = labels.clone();
        for e in &s.entities {
            let mut votes = BTreeMap::new();
            for n in adjacency.get(e.id.as_str()).into_iter().flatten() {
                *votes.entry(old[n]).or_insert(0usize) += 1;
            }
            if let Some((label, _)) = votes
                .into_iter()
                .max_by_key(|(key, count)| (*count, std::cmp::Reverse(*key)))
            {
                labels.insert(&e.id, label);
            }
        }
    }
    let mut groups: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (id, label) in labels {
        groups.entry(label).or_default().insert(id);
    }
    groups.into_values().filter(|v|v.len()>1).map(|members|json!({"members":members,"authority":"rule_derived","algorithm":"bounded label propagation; not author-declared modules"})).collect()
}
