//! Native data batches may commit before publication. Only the final receipt
//! certifies completeness; canonical revision membership keeps orphans private.
use super::{Projection, err, quote};
use crate::facts::{
    objects::Objects,
    tree,
    writer::{Revision, record_bytes},
};
use cq_core::{
    Result,
    model::digest,
    ports::facts::{FactRef, ReadStats},
};
use lbug::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
const PAGE_BYTES: u64 = 16 * 1024 * 1024;
const FACT_BYTES: u64 = 32 * 1024 * 1024;
const FACT_ROWS: usize = 16 * 1024;
#[derive(Clone, Default, Serialize)]
pub struct ImportStats {
    pub inventory_pages: usize,
    pub inventory_facts: usize,
    pub payload_pages_decoded: usize,
    pub facts_by_family: BTreeMap<String, u64>,
    pub transactions: u64,
    pub pages: u64,
    pub page_locators: u64,
    pub facts: u64,
    pub links: u64,
    pub maximum_page_transaction_bytes: u64,
    pub maximum_fact_transaction_bytes: u64,
    pub page_batch_budget_bytes: u64,
    pub fact_batch_budget_bytes: u64,
    pub fact_batch_row_limit: usize,
    pub complete: bool,
}
fn transaction<T>(c: &Connection<'_>, f: impl FnOnce() -> Result<T>) -> Result<T> {
    c.query("BEGIN TRANSACTION").map_err(err)?;
    match f() {
        Ok(v) => {
            if let Err(e) = c.query("COMMIT") {
                let _ = c.query("ROLLBACK");
                return Err(err(e));
            }
            Ok(v)
        }
        Err(e) => {
            let _ = c.query("ROLLBACK");
            Err(e)
        }
    }
}
#[derive(Default, Deserialize)]
struct DisplayFields {
    #[serde(default)]
    kind: serde_json::Value,
    #[serde(default)]
    name: serde_json::Value,
    #[serde(default)]
    unit: serde_json::Value,
    #[serde(default)]
    from: serde_json::Value,
    #[serde(default)]
    to: serde_json::Value,
}
fn display_fields(bytes: &[u8]) -> Result<DisplayFields> {
    if bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{') {
        Ok(serde_json::from_slice(bytes)?)
    } else {
        let _: serde::de::IgnoredAny = serde_json::from_slice(bytes)?;
        Ok(DisplayFields::default())
    }
}
struct Batch {
    dir: tempfile::TempDir,
    facts: csv::Writer<std::fs::File>,
    links: csv::Writer<std::fs::File>,
    slots: BTreeSet<String>,
    rows: usize,
    edges: u64,
    bytes: u64,
    families: BTreeMap<String, u64>,
    parallel_facts: bool,
    parallel_links: bool,
}
impl Batch {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let facts = csv::WriterBuilder::new()
            .quote_style(csv::QuoteStyle::Always)
            .from_path(dir.path().join("facts.csv"))
            .map_err(err)?;
        let links = csv::WriterBuilder::new()
            .quote_style(csv::QuoteStyle::Always)
            .from_path(dir.path().join("links.csv"))
            .map_err(err)?;
        let mut b = Self {
            dir,
            facts,
            links,
            slots: BTreeSet::new(),
            rows: 0,
            edges: 0,
            bytes: 0,
            families: BTreeMap::new(),
            parallel_facts: true,
            parallel_links: true,
        };
        b.headers()?;
        Ok(b)
    }
    fn headers(&mut self) -> Result<()> {
        self.facts
            .write_record(["id", "payload", "family", "key", "kind", "name", "context"])
            .map_err(err)?;
        self.links
            .write_record(["from", "to", "id", "payload", "kind"])
            .map_err(err)?;
        Ok(())
    }
    fn flush(
        &mut self,
        c: &Connection<'_>,
        stats: &mut ImportStats,
        point: &dyn Fn(&str) -> Result<()>,
    ) -> Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        self.facts.flush()?;
        self.links.flush()?;
        transaction(c, || {
            if !self.slots.is_empty() {
                c.query(&format!(
                    "UNWIND {} AS id MERGE (:Slot {{id:id}})",
                    serde_json::to_string(&self.slots)?
                ))
                .map_err(err)?;
            }
            c.query(&format!(
                "COPY Fact FROM {} (HEADER=true,PARALLEL={})",
                quote(self.dir.path().join("facts.csv").to_str().unwrap()),
                self.parallel_facts
            ))
            .map_err(err)?;
            if self.edges > 0 {
                c.query(&format!(
                    "COPY LINK FROM {} (HEADER=true,PARALLEL={})",
                    quote(self.dir.path().join("links.csv").to_str().unwrap()),
                    self.parallel_links
                ))
                .map_err(err)?;
            }
            Ok(())
        })?;
        stats.transactions += 1;
        stats.facts += self.rows as u64;
        for (family, count) in &self.families {
            *stats.facts_by_family.entry(family.clone()).or_default() += count;
        }
        stats.links += self.edges;
        stats.maximum_fact_transaction_bytes = stats.maximum_fact_transaction_bytes.max(self.bytes);
        point("native-facts-batch")?;
        *self = Self::new()?;
        Ok(())
    }
}
impl Projection {
    pub fn import_stats(&self) -> ImportStats {
        self.imports.lock().unwrap().clone()
    }
    pub fn import(&self, o: &Objects, r: &Revision) -> Result<u64> {
        self.import_with(o, r, &|_| Ok(()))
    }
    pub(crate) fn import_with(
        &self,
        o: &Objects,
        r: &Revision,
        point: &dyn Fn(&str) -> Result<()>,
    ) -> Result<u64> {
        let _write = self.import_lock.lock().unwrap();
        let mut stats = ImportStats {
            page_batch_budget_bytes: PAGE_BYTES,
            fact_batch_budget_bytes: FACT_BYTES,
            fact_batch_row_limit: FACT_ROWS,
            ..Default::default()
        };
        let mut decoded_pages = 0;
        let result = (|| {
            if self.receipt(r)? {
                std::fs::File::open(&self.directory)?.sync_all()?;
                stats.complete = true;
                return Ok(0);
            }
            let c = self.connection()?;
            let incremental = r
                .parent
                .as_ref()
                .map(|p| Self::exists(&c, "Receipt", p))
                .transpose()?
                .unwrap_or(false);
            let root = if incremental {
                &r.changes.upserts
            } else {
                &r.roots.facts
            };
            // A root is a compact inventory, not payloads. Group it once so
            // native row extraction cannot rehash/read a MiB page per fact.
            let mut inventory_progress = crate::facts::limits::root(&o.root).map(|store| {
                cq_core::progress::Progress::stage(
                    store,
                    &r.project,
                    "native_inventory",
                    "整理图发布清单",
                    Some(r.changes.facts_changed),
                    "事实",
                )
            });
            let mut inventoried = 0;
            let mut by_page = BTreeMap::<String, Vec<(String, FactRef)>>::new();
            tree::visit(o, root, |key, value| {
                let f: FactRef = serde_json::from_value(value.clone())?;
                inventoried += 1;
                if let Some(p) = &mut inventory_progress {
                    p.count(inventoried);
                }
                by_page
                    .entry(f.object.hash.clone())
                    .or_default()
                    .push((key.into(), f));
                Ok(())
            })?;
            stats.inventory_pages = by_page.len();
            stats.inventory_facts = by_page.values().map(Vec::len).sum();
            let mut progress = crate::facts::limits::root(&o.root).map(|store| {
                cq_core::progress::Progress::stage(
                    store,
                    &r.project,
                    "native_pages",
                    "发布图数据库页",
                    Some(stats.inventory_pages as u64),
                    "页",
                )
            });
            let mut page_bytes = 0u64;
            let mut page_count = 0;
            // Each page batch is a transaction. Replays skip committed page hashes.
            c.query("BEGIN TRANSACTION").map_err(err)?;
            let pages: Result<()> = (|| {
                for (done, rows) in by_page.values().enumerate() {
                    if let Some(p) = &mut progress {
                        p.count(done as u64);
                    }
                    let f = rows[0].1.clone();
                    if Self::exists(&c, "Page", &f.object.hash)? {
                        continue;
                    }
                    if page_count > 0 && page_bytes + f.object.bytes > PAGE_BYTES {
                        c.query("COMMIT").map_err(err)?;
                        stats.transactions += 1;
                        stats.maximum_page_transaction_bytes =
                            stats.maximum_page_transaction_bytes.max(page_bytes);
                        point("native-pages-batch")?;
                        c.query("BEGIN TRANSACTION").map_err(err)?;
                        page_bytes = 0;
                        page_count = 0;
                    }
                    super::page::store_locator(&c, &f.object)?;
                    stats.page_locators += 1;
                    stats.pages += 1;
                    page_bytes += f.object.bytes;
                    page_count += 1;
                }
                c.query("COMMIT").map_err(err)?;
                stats.transactions += 1;
                stats.maximum_page_transaction_bytes =
                    stats.maximum_page_transaction_bytes.max(page_bytes);
                point("native-pages-batch")?;
                Ok(())
            })();
            if let Err(e) = pages {
                let _ = c.query("ROLLBACK");
                return Err(e);
            }
            let mut batch = Batch::new()?;
            if let Some(p) = &mut progress {
                p.detail(
                    "native_facts",
                    "发布图数据库事实",
                    0,
                    Some(stats.inventory_facts as u64),
                    "事实",
                    cq_core::compute::workers()? as u64,
                );
            }
            let existing = super::membership::Membership::load(&c)?;
            let mut chunk = Vec::<(String, FactRef)>::new();
            let mut completed = 0u64;
            let mut consume = |chunk: &mut Vec<(String, FactRef)>, bytes: &[u8]| -> Result<()> {
                if chunk.is_empty() {
                    return Ok(());
                }
                let prepared = cq_core::compute::map(std::mem::take(chunk), |(key, f)| {
                    let id = digest(format!("{key}:{}", f.digest).as_bytes());
                    let present = existing.contains(&super::membership::key(&id)?)?;
                    let fields = if present {
                        None
                    } else {
                        Some(display_fields(record_bytes(bytes, &f)?)?)
                    };
                    Ok((key, f, id, fields))
                })?;
                for (key, f, id, fields) in prepared {
                    completed += 1;
                    if let Some(p) = &mut progress {
                        p.count(completed);
                    }
                    let Some(v) = fields else {
                        continue;
                    };
                    let family = key.split(':').next().unwrap_or("");
                    let kind = v.kind.as_str().unwrap_or("");
                    let name = v.name.as_str().unwrap_or("");
                    let unit = v.unit.as_str().unwrap_or("");
                    let row = [id.as_str(), &f.object.hash, family, &key, kind, name, unit];
                    let edge = if family == "relation" {
                        Some([
                            v.from
                                .as_str()
                                .ok_or_else(|| err("relation source absent"))?,
                            v.to.as_str().ok_or_else(|| err("relation target absent"))?,
                            id.as_str(),
                            &f.object.hash,
                            kind,
                        ])
                    } else {
                        None
                    };
                    // CSV quotes can double every input byte; this is a conservative bound.
                    let bytes = 2 * row.iter().map(|s| s.len() as u64 + 2).sum::<u64>()
                        + edge
                            .as_ref()
                            .map(|a| 2 * a.iter().map(|s| s.len() as u64 + 2).sum::<u64>())
                            .unwrap_or(0)
                        + 2;
                    if batch.rows > 0
                        && (batch.rows >= FACT_ROWS || batch.bytes + bytes > FACT_BYTES)
                    {
                        batch.flush(&c, &mut stats, point)?;
                    }
                    // Ladybug's parallel CSV reader rejects quoted newlines.
                    // Keep exact document names and fall back for this batch.
                    batch.parallel_facts &= !row.iter().any(|s| s.contains(['\n', '\r']));
                    batch.facts.write_record(row).map_err(err)?;
                    batch.rows += 1;
                    *batch.families.entry(family.into()).or_default() += 1;
                    batch.bytes += bytes;
                    if let Some(edge) = edge {
                        batch.parallel_links &= !edge.iter().any(|s| s.contains(['\n', '\r']));
                        batch.slots.insert(edge[0].into());
                        batch.slots.insert(edge[1].into());
                        batch.links.write_record(edge).map_err(err)?;
                        batch.edges += 1;
                    }
                }
                Ok(())
            };
            for rows in by_page.into_values() {
                let bytes = o.read(&rows[0].1.object, &mut ReadStats::default())?;
                decoded_pages += 1;
                for (key, reference) in rows {
                    chunk.push((key, reference));
                    if chunk.len() >= 4096 {
                        consume(&mut chunk, &bytes)?;
                    }
                }
                // Finish a physical page before reading unrelated pages. Only
                // the bounded native CSV transaction spans several pages.
                consume(&mut chunk, &bytes)?;
            }
            batch.flush(&c, &mut stats, point)?;
            if let Some(p) = &mut progress {
                p.detail("activate", "提交版本", 0, Some(1), "版本", 1);
            }
            point("native-before-receipt")?;
            transaction(&c, || {
                c.query(&format!(
                    "MERGE (r:Receipt {{id:{}}}) SET r.seal={},r.seq={}",
                    quote(&r.revision),
                    quote(&r.seal),
                    r.sequence
                ))
                .map_err(err)?;
                Ok(())
            })?;
            stats.transactions += 1;
            std::fs::File::open(&self.directory)?.sync_all()?;
            stats.complete = true;
            Ok(stats.pages)
        })();
        stats.payload_pages_decoded = decoded_pages;
        *self.imports.lock().unwrap() = stats;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::writer::{Roots, Writer};
    use cq_core::Error;
    use serde_json::json;
    #[test]
    fn interrupted_native_batches_have_no_receipt_and_replay_without_duplicate_edges() {
        let t = tempfile::tempdir().unwrap();
        let o = Objects::buffered(t.path().join("objects")).unwrap();
        let mut w = Writer::new(&o, Roots::default());
        for id in ["a", "b"] {
            w.append(
                "entity",
                id,
                &json!({"id":id,"kind":"Function","name":id,"unit":"u","external":false}),
            )
            .unwrap();
        }
        let links = FACT_ROWS + 252;
        for i in 0..links {
            let id = format!("r{i}");
            w.append(
                "relation",
                &id,
                &json!({"id":id,"from":"a","to":"b","kind":"Calls"}),
            )
            .unwrap();
        }
        let (roots, changes) = w.finish_inventory().unwrap();
        o.sync_pending().unwrap();
        let mut r = Revision {
            format: 2,
            project: "controlled".into(),
            revision: digest(b"batch"),
            parent: None,
            sequence: 1,
            roots,
            metadata: json!({}),
            changes,
            seal: String::new(),
        };
        r.seal().unwrap();
        let p = Projection::open(&t.path().join("native.lbdb"), true).unwrap();
        let result = p.import_with(&o, &r, &|stage| {
            if stage == "native-facts-batch" {
                Err(Error::new(
                    "InjectedImportFailure",
                    "after committed data batch",
                ))
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap_err().code(), "InjectedImportFailure");
        assert!(!p.receipt(&r).unwrap());
        assert!(!p.import_stats().complete);
        assert!(p.physical_counts().unwrap()["links"].as_u64().unwrap() > 0);
        assert!(p.physical_counts().unwrap()["links"].as_u64().unwrap() < links as u64);
        // Reopen the durable partial database rather than replaying against
        // the same live connection; publication recovery must survive restart.
        drop(p);
        let p = Projection::open(&t.path().join("native.lbdb"), false).unwrap();
        assert_eq!(
            p.buffer_bytes,
            super::super::residency::buffer_bytes().unwrap()
        );
        assert!(!p.receipt(&r).unwrap());
        p.import(&o, &r).unwrap();
        assert!(p.receipt(&r).unwrap());
        let counts = p.physical_counts().unwrap();
        assert_eq!(counts["facts"], links as u64 + 2);
        assert_eq!(counts["links"], links as u64);
        assert_eq!(counts["receipts"], 1);
        let stats = p.import_stats();
        assert_eq!(stats.pages, 0);
        assert!(stats.payload_pages_decoded <= stats.inventory_pages);
        assert_eq!(stats.facts_by_family.values().sum::<u64>(), stats.facts);
        assert!(stats.maximum_fact_transaction_bytes <= stats.fact_batch_budget_bytes);
        assert!(stats.maximum_page_transaction_bytes <= stats.page_batch_budget_bytes);
    }
}
