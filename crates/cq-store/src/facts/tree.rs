//! Persistent radix map. Leaf pages are bounded; changing one key copies only
//! its leaf and ancestors. Stable scan cursors use hashed key plus original key.
use super::objects::Objects;
use cq_core::{
    Error, Result,
    model::digest,
    ports::facts::{ObjectRef, ReadStats},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
const LEAF: usize = 32;
pub type Root = Option<ObjectRef>;
#[derive(Serialize, Deserialize)]
pub(crate) enum Node {
    Leaf(BTreeMap<String, Value>),
    Branch(BTreeMap<u8, ObjectRef>),
}
pub(crate) fn order(key: &str) -> String {
    format!("{}:{key}", digest(key.as_bytes()))
}
/// Build an initial immutable map once from strictly ordered keys. Only a
/// leaf-sized lookahead and the branch stack are retained, not the whole map.
pub(crate) fn build_sorted(
    o: &Objects,
    rows: impl Iterator<Item = Result<(String, Value)>>,
) -> Result<(Root, u64)> {
    build_sorted_prefix(o, rows, "")
}
pub(crate) fn build_sorted_prefix(
    o: &Objects,
    rows: impl Iterator<Item = Result<(String, Value)>>,
    prefix: &str,
) -> Result<(Root, u64)> {
    struct Input<I: Iterator<Item = Result<(String, Value)>>> {
        rows: std::iter::Peekable<I>,
        replay: std::collections::VecDeque<(String, Value)>,
        last: Option<String>,
    }
    impl<I: Iterator<Item = Result<(String, Value)>>> Input<I> {
        fn peek(&mut self) -> Result<Option<&str>> {
            if !self.replay.is_empty() {
                return Ok(self.replay.front().map(|r| r.0.as_str()));
            }
            if self.rows.peek().is_some_and(Result::is_err) {
                return Err(self.rows.next().unwrap().unwrap_err());
            }
            Ok(self
                .rows
                .peek()
                .and_then(|r| r.as_ref().ok())
                .map(|r| r.0.as_str()))
        }
        fn take(&mut self) -> Result<Option<(String, Value)>> {
            if let Some(row) = self.replay.pop_front() {
                return Ok(Some(row));
            }
            let Some(row) = self.rows.next().transpose()? else {
                return Ok(None);
            };
            if self.last.as_ref().is_some_and(|k| k >= &row.0) {
                return Err(Error::new(
                    "InvalidSortedIndex",
                    "unordered or duplicate key",
                ));
            }
            self.last = Some(row.0.clone());
            Ok(Some(row))
        }
    }
    fn build<I: Iterator<Item = Result<(String, Value)>>>(
        o: &Objects,
        input: &mut Input<I>,
        prefix: &str,
    ) -> Result<(Root, u64)> {
        let mut batch = vec![];
        while batch.len() <= LEAF && input.peek()?.is_some_and(|k| k.starts_with(prefix)) {
            batch.push(input.take()?.unwrap());
        }
        if batch.len() <= LEAF {
            let count = batch.len() as u64;
            let root = if count == 0 {
                None
            } else {
                Some(o.json(&Node::Leaf(batch.into_iter().collect()))?)
            };
            return Ok((root, count));
        }
        if prefix.len() >= 64 {
            return Err(Error::new("HashCollision", "initial index collision"));
        }
        for row in batch.into_iter().rev() {
            input.replay.push_front(row);
        }
        let mut children = BTreeMap::new();
        let mut count = 0;
        while let Some(key) = input.peek()? {
            if !key.starts_with(prefix) {
                break;
            }
            let n = nibble(key, prefix.len())?;
            let (root, size) = build(o, input, &format!("{prefix}{n:x}"))?;
            children.insert(
                n,
                root.ok_or_else(|| Error::new("InvalidSortedIndex", "empty branch"))?,
            );
            count += size;
        }
        Ok((Some(o.json(&Node::Branch(children))?), count))
    }
    let mut input = Input {
        rows: rows.peekable(),
        replay: Default::default(),
        last: None,
    };
    let result = build(o, &mut input, prefix)?;
    if input.peek()?.is_some() {
        return Err(Error::new(
            "InvalidSortedIndex",
            "key outside declared hash partition",
        ));
    }
    Ok(result)
}
fn nibble(key: &str, depth: usize) -> Result<u8> {
    key.as_bytes()
        .get(depth)
        .and_then(|b| (*b as char).to_digit(16))
        .map(|n| n as u8)
        .ok_or_else(|| Error::new("InvalidManifestTree", "depth or hashed key"))
}
pub fn get(o: &Objects, root: &Root, key: &str, s: &mut ReadStats) -> Result<Option<Value>> {
    let key = order(key);
    let mut next = root.clone();
    let mut depth = 0;
    while let Some(r) = next {
        match o.decode::<Node>(&r, s)? {
            Node::Leaf(rows) => return Ok(rows.get(&key).cloned()),
            Node::Branch(children) => {
                next = children.get(&nibble(&key, depth)?).cloned();
                depth += 1;
            }
        }
    }
    Ok(None)
}
pub fn set(
    o: &Objects,
    root: &Root,
    key: &str,
    value: Option<Value>,
    s: &mut ReadStats,
) -> Result<Root> {
    change(o, root, &order(key), value, 0, s)
}
/// Apply a sealed batch without persisting the intermediate tree after every
/// individual key. Each affected leaf/ancestor is written once for this batch.
pub fn edit(
    o: &Objects,
    root: &Root,
    changes: Vec<(String, Option<Value>)>,
    s: &mut ReadStats,
) -> Result<Root> {
    let changes = changes
        .into_iter()
        .map(|(k, v)| (order(&k), v))
        .collect::<BTreeMap<_, _>>();
    edits(o, root, changes, 0, s)
}
fn edits(
    o: &Objects,
    root: &Root,
    changes: BTreeMap<String, Option<Value>>,
    depth: usize,
    s: &mut ReadStats,
) -> Result<Root> {
    if changes.is_empty() {
        return Ok(root.clone());
    }
    let node = if let Some(r) = root {
        o.decode(r, s)?
    } else {
        Node::Leaf(BTreeMap::new())
    };
    match node {
        Node::Leaf(mut rows) => {
            let original = rows.clone();
            for (k, v) in changes {
                if let Some(v) = v {
                    rows.insert(k, v);
                } else {
                    rows.remove(&k);
                }
            }
            if rows == original {
                Ok(root.clone())
            } else {
                save_leaf(o, rows, depth)
            }
        }
        Node::Branch(mut children) => {
            let mut split = BTreeMap::<u8, BTreeMap<String, Option<Value>>>::new();
            for (k, v) in changes {
                split.entry(nibble(&k, depth)?).or_default().insert(k, v);
            }
            let mut changed = false;
            for (n, changes) in split {
                let old = children.get(&n).cloned();
                let new = edits(o, &old, changes, depth + 1, s)?;
                changed |= old != new;
                if let Some(r) = new {
                    children.insert(n, r);
                } else {
                    children.remove(&n);
                }
            }
            if !changed {
                Ok(root.clone())
            } else if children.is_empty() {
                Ok(None)
            } else {
                Ok(Some(o.json(&Node::Branch(children))?))
            }
        }
    }
}
fn save_leaf(o: &Objects, rows: BTreeMap<String, Value>, depth: usize) -> Result<Root> {
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() <= LEAF {
        return Ok(Some(o.json(&Node::Leaf(rows))?));
    }
    if depth >= 64 {
        return Err(Error::new(
            "HashCollision",
            "manifest leaf collision budget",
        ));
    }
    let mut split: BTreeMap<u8, BTreeMap<String, Value>> = BTreeMap::new();
    for (k, v) in rows {
        split.entry(nibble(&k, depth)?).or_default().insert(k, v);
    }
    let mut children = BTreeMap::new();
    for (n, rows) in split {
        children.insert(n, save_leaf(o, rows, depth + 1)?.unwrap());
    }
    Ok(Some(o.json(&Node::Branch(children))?))
}
fn change(
    o: &Objects,
    root: &Root,
    key: &str,
    value: Option<Value>,
    depth: usize,
    s: &mut ReadStats,
) -> Result<Root> {
    let node = if let Some(r) = root {
        o.decode(r, s)?
    } else {
        Node::Leaf(BTreeMap::new())
    };
    match node {
        Node::Leaf(mut rows) => {
            if rows.get(key) == value.as_ref() {
                return Ok(root.clone());
            }
            if let Some(v) = value {
                rows.insert(key.into(), v);
            } else {
                rows.remove(key);
            }
            save_leaf(o, rows, depth)
        }
        Node::Branch(mut children) => {
            let n = nibble(key, depth)?;
            let old = children.get(&n).cloned();
            let updated = change(o, &old, key, value, depth + 1, s)?;
            if updated == old {
                return Ok(root.clone());
            }
            if let Some(r) = updated {
                children.insert(n, r);
            } else {
                children.remove(&n);
            }
            if children.is_empty() {
                Ok(None)
            } else {
                Ok(Some(o.json(&Node::Branch(children))?))
            }
        }
    }
}
#[derive(Debug)]
pub struct Page {
    pub rows: Vec<(String, String, Value)>,
    pub after: Option<String>,
    pub complete: bool,
}
pub fn scan(
    o: &Objects,
    root: &Root,
    after: Option<&str>,
    limit: usize,
    s: &mut ReadStats,
) -> Result<Page> {
    if limit == 0 || limit > 1000 {
        return Err(Error::new("InvalidBudget", "tree page limit 1..1000"));
    }
    let mut rows = Vec::new();
    if let Some(r) = root {
        walk(o, r, "", after, limit + 1, &mut rows, s)?;
    }
    let complete = rows.len() <= limit;
    rows.truncate(limit);
    let last = if complete {
        None
    } else {
        rows.last().map(|(sort, _, _)| sort.clone())
    };
    Ok(Page {
        rows,
        after: last,
        complete,
    })
}
fn walk(
    o: &Objects,
    r: &ObjectRef,
    prefix: &str,
    after: Option<&str>,
    cap: usize,
    rows: &mut Vec<(String, String, Value)>,
    s: &mut ReadStats,
) -> Result<()> {
    if rows.len() >= cap {
        return Ok(());
    }
    if let Some(a) = after {
        let hi = format!("{prefix}~");
        if hi.as_str() < a {
            return Ok(());
        }
    }
    match o.decode::<Node>(r, s)? {
        Node::Leaf(items) => {
            for (sort, v) in items {
                if after.is_none_or(|a| sort.as_str() > a) {
                    let key = sort
                        .get(65..)
                        .ok_or_else(|| Error::new("InvalidManifestTree", "leaf key"))?
                        .to_owned();
                    rows.push((sort, key, v));
                    if rows.len() >= cap {
                        break;
                    }
                }
            }
        }
        Node::Branch(children) => {
            for (n, child) in children {
                walk(o, &child, &format!("{prefix}{n:x}"), after, cap, rows, s)?;
                if rows.len() >= cap {
                    break;
                }
            }
        }
    }
    Ok(())
}
pub fn visit(
    o: &Objects,
    root: &Root,
    mut f: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<()> {
    let mut after = None;
    loop {
        let p = scan(o, root, after.as_deref(), 256, &mut ReadStats::default())?;
        for (_, k, v) in p.rows {
            f(&k, &v)?;
        }
        if p.complete {
            break;
        }
        after = p.after;
    }
    Ok(())
}

/// Visit only different Merkle paths. Mixed leaf/branch shapes are partitioned
/// in memory, so a split/merge never requires materializing the larger subtree.
type DiffVisitor<'a> = dyn FnMut(&str, Option<&Value>, Option<&Value>) -> Result<()> + 'a;
pub fn diff(
    o: &Objects,
    left: &Root,
    right: &Root,
    stats: &mut ReadStats,
    mut f: impl FnMut(&str, Option<&Value>, Option<&Value>) -> Result<()>,
) -> Result<()> {
    enum Part {
        Ref(Root),
        Leaf(BTreeMap<String, Value>),
    }
    fn read(o: &Objects, p: Part, s: &mut ReadStats) -> Result<Node> {
        match p {
            Part::Ref(Some(r)) => o.decode(&r, s),
            Part::Ref(None) => Ok(Node::Leaf(BTreeMap::new())),
            Part::Leaf(v) => Ok(Node::Leaf(v)),
        }
    }
    fn split(n: Node, depth: usize) -> Result<BTreeMap<u8, Part>> {
        match n {
            Node::Branch(c) => Ok(c
                .into_iter()
                .map(|(k, v)| (k, Part::Ref(Some(v))))
                .collect()),
            Node::Leaf(rows) => {
                let mut out = BTreeMap::<u8, BTreeMap<String, Value>>::new();
                for (k, v) in rows {
                    out.entry(nibble(&k, depth)?).or_default().insert(k, v);
                }
                Ok(out.into_iter().map(|(k, v)| (k, Part::Leaf(v))).collect())
            }
        }
    }
    fn run(
        o: &Objects,
        a: Part,
        b: Part,
        depth: usize,
        s: &mut ReadStats,
        f: &mut DiffVisitor<'_>,
    ) -> Result<()> {
        if let (Part::Ref(a), Part::Ref(b)) = (&a, &b)
            && a == b
        {
            return Ok(());
        }
        if depth > 64 {
            return Err(Error::new("InvalidManifestTree", "diff depth"));
        }
        let a = read(o, a, s)?;
        let b = read(o, b, s)?;
        match (a, b) {
            (Node::Leaf(a), Node::Leaf(b)) => {
                for key in a
                    .keys()
                    .chain(b.keys())
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    let old = a.get(key);
                    let new = b.get(key);
                    if old != new {
                        let key = key
                            .get(65..)
                            .ok_or_else(|| Error::new("InvalidManifestTree", "diff leaf key"))?;
                        f(key, old, new)?;
                    }
                }
                Ok(())
            }
            (a, b) => {
                let mut a = split(a, depth)?;
                let mut b = split(b, depth)?;
                for k in a
                    .keys()
                    .chain(b.keys())
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    run(
                        o,
                        a.remove(&k).unwrap_or(Part::Ref(None)),
                        b.remove(&k).unwrap_or(Part::Ref(None)),
                        depth + 1,
                        s,
                        f,
                    )?;
                }
                Ok(())
            }
        }
    }
    run(
        o,
        Part::Ref(left.clone()),
        Part::Ref(right.clone()),
        0,
        stats,
        &mut f,
    )
}
