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
    execution::Stage,
    model::digest,
    ports::facts::{FactRef, ReadStats},
};
use lbug::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
const PAGE_BYTES: u64 = 16 * 1024 * 1024;
const FACT_BYTES: u64 = 32 * 1024 * 1024;
const FACT_ROWS: usize = 64 * 1024;
fn query_timeout_ms() -> Result<u64> {
    let value = std::env::var("CQ_LADYBUG_IMPORT_QUERY_TIMEOUT_MS")
        .map(Some)
        .or_else(|e| match e {
            std::env::VarError::NotPresent => Ok(None),
            _ => Err(cq_core::Error::new(
                "GraphImportBudget",
                "timeout is not UTF-8",
            )),
        })?;
    parse_query_timeout(value.as_deref())
}
fn parse_query_timeout(value: Option<&str>) -> Result<u64> {
    let ms = value
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| {
            cq_core::Error::new(
                "GraphImportBudget",
                "CQ_LADYBUG_IMPORT_QUERY_TIMEOUT_MS must be milliseconds",
            )
        })?
        .unwrap_or(300_000);
    if !(1..=86_400_000).contains(&ms) {
        return Err(cq_core::Error::new(
            "GraphImportBudget",
            "import query timeout must be 1..86400000 milliseconds",
        ));
    }
    Ok(ms)
}
#[derive(Clone, Default, Serialize)]
pub struct ImportStats {
    pub begin_ms: u64,
    pub commit_ms: u64,
    pub checkpoint_ms: u64,
    pub checkpoint_calls: u64,
    pub checkpoint_budget_bytes: u64,
    pub maximum_observed_wal_bytes: u64,
    #[serde(skip)]
    checkpoint_pending_bytes: u64,
    #[serde(skip)]
    wal_path: std::path::PathBuf,
    pub slot_copy_ms: u64,
    pub fact_copy_ms: u64,
    pub link_copy_ms: u64,
    pub batch_transaction_ms: u64,
    pub slot_copy_statements: u64,
    pub slots_inserted: u64,
    pub fact_batches: u64,
    pub query_timeout_ms: u64,
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
fn begin(c: &Connection<'_>, stats: &mut ImportStats) -> Result<()> {
    let start = std::time::Instant::now();
    let r = c.query("BEGIN TRANSACTION");
    stats.begin_ms += start.elapsed().as_millis() as u64;
    r.map_err(|e| err(format!("native BEGIN: {e}")))?;
    Ok(())
}
fn commit(c: &Connection<'_>, stats: &mut ImportStats) -> Result<()> {
    let start = std::time::Instant::now();
    let r = c.query("COMMIT");
    stats.commit_ms += start.elapsed().as_millis() as u64;
    r.map_err(|e| err(format!("native COMMIT: {e}")))?;
    Ok(())
}
fn checkpoint(
    c: &Connection<'_>,
    stats: &mut ImportStats,
    point: &dyn Fn(&str) -> Result<()>,
    force: bool,
) -> Result<()> {
    let wal = match std::fs::metadata(&stats.wal_path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    stats.maximum_observed_wal_bytes = stats.maximum_observed_wal_bytes.max(wal);
    if !force
        && stats.checkpoint_pending_bytes < stats.checkpoint_budget_bytes
        && wal < stats.checkpoint_budget_bytes
    {
        return Ok(());
    }
    point("native-before-checkpoint")?;
    let start = std::time::Instant::now();
    let r = c.query("CHECKPOINT");
    stats.checkpoint_ms += start.elapsed().as_millis() as u64;
    stats.checkpoint_calls += 1;
    r.map_err(|e| err(format!("native CHECKPOINT: {e}")))?;
    stats.checkpoint_pending_bytes = 0;
    point("native-after-checkpoint")?;
    Ok(())
}
fn transaction<T>(
    c: &Connection<'_>,
    stats: &mut ImportStats,
    f: impl FnOnce(&mut ImportStats) -> Result<T>,
) -> Result<T> {
    begin(c, stats)?;
    match f(stats) {
        Ok(v) => {
            if let Err(e) = commit(c, stats) {
                let _ = c.query("ROLLBACK");
                return Err(e);
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
        slots: &mut super::membership::Slots,
    ) -> Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        self.facts.flush()?;
        self.links.flush()?;
        let path = self.dir.path().join("slots.csv");
        let mut csv = csv::Writer::from_path(&path).map_err(err)?;
        csv.write_record(["id"]).map_err(err)?;
        let mut inserted = Vec::new();
        let mut parallel_slots = true;
        for id in &self.slots {
            use sha2::{Digest, Sha256};
            let key: [u8; 32] = Sha256::digest(id.as_bytes()).into();
            if !slots.contains(&key)? {
                csv.write_record([id]).map_err(err)?;
                parallel_slots &= !id.contains(['\n', '\r']);
                inserted.push(key);
            }
        }
        csv.flush()?;
        let transaction_started = std::time::Instant::now();
        transaction(c, stats, |stats| {
            if !inserted.is_empty() {
                let started = std::time::Instant::now();
                c.query(&format!(
                    "COPY Slot FROM {} (HEADER=true,PARALLEL={})",
                    quote(path.to_str().unwrap()),
                    parallel_slots
                ))
                .map_err(|e| err(format!("native COPY Slot: {e}")))?;
                stats.slot_copy_ms += started.elapsed().as_millis() as u64;
            }
            let started = std::time::Instant::now();
            c.query(&format!(
                "COPY Fact FROM {} (HEADER=true,PARALLEL={})",
                quote(self.dir.path().join("facts.csv").to_str().unwrap()),
                self.parallel_facts
            ))
            .map_err(|e| err(format!("native COPY Fact: {e}")))?;
            stats.fact_copy_ms += started.elapsed().as_millis() as u64;
            if self.edges > 0 {
                let started = std::time::Instant::now();
                c.query(&format!(
                    "COPY LINK FROM {} (HEADER=true,PARALLEL={})",
                    quote(self.dir.path().join("links.csv").to_str().unwrap()),
                    self.parallel_links
                ))
                .map_err(|e| err(format!("native COPY LINK: {e}")))?;
                stats.link_copy_ms += started.elapsed().as_millis() as u64;
            }
            Ok(())
        })?;
        stats.batch_transaction_ms += transaction_started.elapsed().as_millis() as u64;
        stats.slot_copy_statements += u64::from(!inserted.is_empty());
        stats.slots_inserted += inserted.len() as u64;
        slots.committed(inserted)?;
        stats.fact_batches += 1;
        stats.transactions += 1;
        stats.facts += self.rows as u64;
        for (family, count) in &self.families {
            *stats.facts_by_family.entry(family.clone()).or_default() += count;
        }
        stats.links += self.edges;
        stats.maximum_fact_transaction_bytes = stats.maximum_fact_transaction_bytes.max(self.bytes);
        stats.checkpoint_pending_bytes += self.bytes;
        point("native-facts-batch")?;
        checkpoint(c, stats, point, false)?;
        *self = Self::new()?;
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct NativeInventory {
    manifest: String,
    incremental: bool,
    pages: BTreeMap<String, crate::facts::inventory::Page<()>>,
}
impl Projection {
    pub(crate) fn import_stage(
        &self,
        o: &Objects,
        r: &Revision,
        stage: Stage,
        path: &std::path::Path,
        control: &cq_core::control::CaptureControl,
    ) -> Result<u64> {
        self.import_policy(
            o,
            r,
            &|_| {
                if control.is_cancelled() {
                    Err(cq_core::Error::new("Cancelled", "native stage paused"))
                } else {
                    Ok(())
                }
            },
            FACT_ROWS,
            (self.buffer_bytes / 2).clamp(32 * 1024 * 1024, 256 * 1024 * 1024),
            Some((stage, path)),
        )
    }
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
        self.import_batches(o, r, point, FACT_ROWS)
    }
    fn import_batches(
        &self,
        o: &Objects,
        r: &Revision,
        point: &dyn Fn(&str) -> Result<()>,
        row_limit: usize,
    ) -> Result<u64> {
        self.import_policy(
            o,
            r,
            point,
            row_limit,
            (self.buffer_bytes / 2).clamp(32 * 1024 * 1024, 256 * 1024 * 1024),
            None,
        )
    }
    fn import_policy(
        &self,
        o: &Objects,
        r: &Revision,
        point: &dyn Fn(&str) -> Result<()>,
        row_limit: usize,
        checkpoint_budget_bytes: u64,
        workflow: Option<(Stage, &std::path::Path)>,
    ) -> Result<u64> {
        let _write = self.import_lock.lock().unwrap();
        let mut stats = ImportStats {
            query_timeout_ms: query_timeout_ms()?,
            page_batch_budget_bytes: PAGE_BYTES,
            fact_batch_budget_bytes: FACT_BYTES,
            fact_batch_row_limit: row_limit,
            checkpoint_budget_bytes,
            wal_path: {
                let mut p = self.database_path.as_os_str().to_owned();
                p.push(".wal");
                p.into()
            },
            ..Default::default()
        };
        let mut decoded_pages = 0;
        let result = (|| {
            if workflow.is_none() && self.receipt(r)? {
                let c = self.connection()?;
                c.set_query_timeout(stats.query_timeout_ms);
                checkpoint(&c, &mut stats, point, true)?;
                std::fs::File::open(&self.directory)?.sync_all()?;
                stats.complete = true;
                return Ok(0);
            }
            let c = self.connection()?;
            // Maintenance writes and checkpoints have their own finite budget.
            // Interactive reads retain their independent 10-second default.
            c.set_query_timeout(stats.query_timeout_ms);
            if Self::exists(&c, "Receipt", &r.revision)? && !self.receipt(r)? {
                return Err(cq_core::Error::new(
                    "RevisionConflict",
                    "native receipt already binds this revision to another manifest",
                ));
            }
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
            let inventory = workflow.map(|(_, path)| path.join("inventory.json"));
            let by_page = if let Some((stage, path)) = workflow
                && stage != Stage::GraphInventory
            {
                let image: crate::raw::Image =
                    serde_json::from_reader(std::fs::File::open(inventory.as_ref().unwrap())?)?;
                let saved: NativeInventory = crate::execution::artifact::restore(path, &image)?;
                if saved.manifest != r.seal || saved.incremental != incremental {
                    return Err(cq_core::Error::new(
                        "InvalidStageCheckpoint",
                        "native inventory binding",
                    ));
                }
                saved.pages
            } else {
                let mut by_page = BTreeMap::<String, crate::facts::inventory::Page<()>>::new();
                tree::visit(o, root, |key, value| {
                    point("native-inventory-row")?;
                    let f: FactRef = serde_json::from_value(value.clone())?;
                    inventoried += 1;
                    if let Some(p) = &mut inventory_progress {
                        p.count(inventoried);
                    }
                    crate::facts::inventory::push(&mut by_page, key, f, ())?;
                    Ok(())
                })?;
                if let Some((_, path)) = workflow {
                    let saved = NativeInventory {
                        manifest: r.seal.clone(),
                        incremental,
                        pages: by_page,
                    };
                    let image = crate::execution::artifact::retain(path, &saved)?;
                    crate::revisions::save(inventory.as_ref().unwrap(), &image)?;
                    saved.pages
                } else {
                    by_page
                }
            };
            stats.inventory_pages = by_page.len();
            stats.inventory_facts = by_page.values().map(|p| p.rows.len()).sum();
            if workflow.is_some_and(|(stage, _)| stage == Stage::GraphInventory) {
                return Ok(0);
            }
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
            if workflow.is_none_or(|(stage, _)| stage != Stage::GraphFacts) {
                let mut page_bytes = 0u64;
                let mut page_count = 0;
                // Each page batch is a transaction. Replays skip committed page hashes.
                begin(&c, &mut stats)?;
                let pages: Result<()> = (|| {
                    for (done, page) in by_page.values().enumerate() {
                        point("native-page-row")?;
                        if let Some(p) = &mut progress {
                            p.count(done as u64);
                        }
                        let object = &page.object;
                        if Self::exists(&c, "Page", &object.hash)? {
                            continue;
                        }
                        if page_count > 0 && page_bytes + object.bytes > PAGE_BYTES {
                            commit(&c, &mut stats)?;
                            stats.transactions += 1;
                            stats.maximum_page_transaction_bytes =
                                stats.maximum_page_transaction_bytes.max(page_bytes);
                            point("native-pages-batch")?;
                            begin(&c, &mut stats)?;
                            page_bytes = 0;
                            page_count = 0;
                        }
                        super::page::store_locator(&c, object)?;
                        stats.page_locators += 1;
                        stats.pages += 1;
                        page_bytes += object.bytes;
                        page_count += 1;
                    }
                    commit(&c, &mut stats)?;
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
                if workflow.is_some_and(|(stage, _)| stage == Stage::GraphPages) {
                    checkpoint(&c, &mut stats, point, true)?;
                    return Ok(0);
                }
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
            let mut slots = super::membership::Slots::load(&c)?;
            let mut chunk = Vec::<(String, FactRef)>::new();
            let mut completed = 0u64;
            let mut consume = |chunk: &mut Vec<(String, FactRef)>, bytes: &[u8]| -> Result<()> {
                point("native-prepare-chunk")?;
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
                        && (batch.rows >= row_limit || batch.bytes + bytes > FACT_BYTES)
                    {
                        batch.flush(&c, &mut stats, point, &mut slots)?;
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
            for page in by_page.into_values() {
                let bytes = o.read(&page.object, &mut ReadStats::default())?;
                decoded_pages += 1;
                for row in page.rows {
                    let (key, reference, ()) = row.expand(&page.object);
                    chunk.push((key, reference));
                    if chunk.len() >= 4096 {
                        consume(&mut chunk, &bytes)?;
                    }
                }
                // Finish a physical page before reading unrelated pages. Only
                // the bounded native CSV transaction spans several pages.
                consume(&mut chunk, &bytes)?;
            }
            batch.flush(&c, &mut stats, point, &mut slots)?;
            if let Some(p) = &mut progress {
                p.detail("activate", "提交版本", 0, Some(1), "版本", 1);
            }
            point("native-before-receipt")?;
            // COPY's bulk pages are not all reconstructed by WAL replay.
            // Make the complete data durable BEFORE a receipt can certify it.
            checkpoint(&c, &mut stats, point, true)?;
            transaction(&c, &mut stats, |_| {
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
            point("native-receipt-committed")?;
            point("native-before-receipt-checkpoint")?;
            // Publication cannot activate HEAD until this checkpoint succeeds.
            // A durable receipt from an interrupted attempt takes the same
            // checkpoint path on retry; it never bypasses the final fence.
            checkpoint(&c, &mut stats, point, true)?;
            point("native-after-receipt-checkpoint")?;
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
    #[ignore = "subprocess helper: exits without destructors at the chosen durability boundary"]
    fn p0_native_crash_child() {
        let root = std::path::PathBuf::from(std::env::var_os("CQ_TEST_NATIVE_CRASH_ROOT").unwrap());
        let stage = std::env::var("CQ_TEST_NATIVE_CRASH_STAGE").unwrap();
        let r: Revision =
            serde_json::from_slice(&std::fs::read(root.join("fixture.json")).unwrap()).unwrap();
        let o = Objects::new(root.join("objects")).unwrap();
        let p = Projection::open(&root.join("native.lbdb"), true).unwrap();
        p.import_policy(
            &o,
            &r,
            &|at| {
                if at == stage {
                    // No Rust/C++ destructors, no Database::drop checkpoint.
                    unsafe { libc::_exit(73) }
                }
                Ok(())
            },
            64,
            4096,
            None,
        )
        .unwrap();
        panic!("requested crash boundary was never reached");
    }
    #[test]
    fn p0_wal_replay_survives_process_death_around_checkpoint_and_receipt() {
        for (stage, has_receipt) in [
            ("native-facts-batch", false),
            ("native-before-checkpoint", false),
            ("native-after-checkpoint", false),
            ("native-receipt-committed", true),
            ("native-before-receipt-checkpoint", true),
            ("native-after-receipt-checkpoint", true),
        ] {
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
            for i in 0..257 {
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
                project: "p0-crash".into(),
                revision: digest(stage.as_bytes()),
                parent: None,
                sequence: 1,
                roots,
                changes,
                metadata: json!({}),
                seal: String::new(),
            };
            r.seal().unwrap();
            std::fs::write(
                t.path().join("fixture.json"),
                serde_json::to_vec(&r).unwrap(),
            )
            .unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "ladybug::import::tests::p0_native_crash_child",
                    "--test-threads=1",
                ])
                .env("CQ_TEST_NATIVE_CRASH_ROOT", t.path())
                .env("CQ_TEST_NATIVE_CRASH_STAGE", stage)
                .env("CQ_WORKERS", "2")
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{stage}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let p = Projection::open(&t.path().join("native.lbdb"), false).unwrap();
            assert_eq!(p.receipt(&r).unwrap(), has_receipt, "{stage}");
            let durable = p.physical_counts().unwrap();
            if has_receipt {
                // Check BEFORE canonical replay: a receipt must never hide
                // missing bulk COPY rows behind an otherwise successful retry.
                assert_eq!(durable["facts"], 259, "{stage}");
                assert_eq!(durable["links"], 257, "{stage}");
            } else if stage == "native-after-checkpoint" {
                let facts = durable["facts"].as_u64().unwrap();
                assert!(
                    facts > 0 && facts < 259,
                    "periodic checkpoint must recover the committed prefix, {stage}: {durable}"
                );
            }
            p.import(&o, &r).unwrap();
            assert!(p.receipt(&r).unwrap());
            let counts = p.physical_counts().unwrap();
            assert_eq!(counts["facts"], 259, "{stage}");
            assert_eq!(counts["links"], 257, "{stage}");
            assert_eq!(counts["receipts"], 1, "{stage}");
            assert!(
                p.import_stats().checkpoint_calls > 0,
                "replayed receipt must pass final checkpoint fence"
            );
            assert_eq!(
                p.rows("CALL current_setting('auto_checkpoint') RETURN *")
                    .unwrap()[0][0],
                lbug::Value::String("False".into())
            );
            drop(p);
            let p = Projection::open(&t.path().join("native.lbdb"), false).unwrap();
            assert_eq!(p.physical_counts().unwrap(), counts);
        }
    }
    #[test]
    fn maintenance_query_timeout_is_finite_configurable_and_separate_from_reads() {
        assert_eq!(parse_query_timeout(None).unwrap(), 300_000);
        for ms in [1, 10_000, 300_000, 86_400_000] {
            assert_eq!(parse_query_timeout(Some(&ms.to_string())).unwrap(), ms);
        }
        for value in ["0", "-1", "86400001", "forever", "18446744073709551616"] {
            assert_eq!(
                parse_query_timeout(Some(value)).unwrap_err().code(),
                "GraphImportBudget"
            );
        }
    }
    #[test]
    fn native_interruption_budget_can_recover_on_a_new_maintenance_connection() {
        let dir = tempfile::tempdir().unwrap();
        let p = Projection::open(&dir.path().join("timeout.lbdb"), true).unwrap();
        let sql = "UNWIND range(0,10000000) AS n RETURN sum(n)";
        let interrupted = p.connection().unwrap();
        interrupted.set_query_timeout(1);
        assert!(
            interrupted
                .query(sql)
                .err()
                .unwrap()
                .to_string()
                .contains("Interrupted")
        );
        drop(interrupted);
        let maintenance = p.connection().unwrap();
        maintenance.set_query_timeout(query_timeout_ms().unwrap());
        let rows: Vec<_> = maintenance.query(sql).unwrap().collect();
        assert_eq!(rows[0][0], lbug::Value::Int128(50_000_005_000_000));
    }
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
        let row_limit = 1024;
        let links = row_limit + 252;
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
        let result = p.import_batches(
            &o,
            &r,
            &|stage| {
                if stage == "native-facts-batch" {
                    Err(Error::new(
                        "InjectedImportFailure",
                        "after committed data batch",
                    ))
                } else {
                    Ok(())
                }
            },
            row_limit,
        );
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
        assert_eq!(stats.query_timeout_ms, query_timeout_ms().unwrap());
        assert_eq!(stats.pages, 0);
        assert!(stats.payload_pages_decoded <= stats.inventory_pages);
        assert_eq!(stats.facts_by_family.values().sum::<u64>(), stats.facts);
        assert!(stats.maximum_fact_transaction_bytes <= stats.fact_batch_budget_bytes);
        assert!(stats.maximum_page_transaction_bytes <= stats.page_batch_budget_bytes);
    }
}
