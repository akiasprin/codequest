//! Long-lived native projection. Page membership is authenticated by the fixed
//! revision's canonical tree, never by a mutable "latest" record in the index.
use crate::facts::writer::Revision;
use cq_core::{Error, Result, ports::facts::ObjectRef};
use lbug::{Connection, Database, SystemConfig, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
mod page;
pub struct Projection {
    db: Database,
    queries: Mutex<u64>,
    cache: Mutex<BTreeMap<String, Arc<Vec<u8>>>>,
    imports: Mutex<import::ImportStats>,
    import_lock: Mutex<()>,
    directory: std::path::PathBuf,
}
fn err(e: impl std::fmt::Display) -> Error {
    Error::new("GraphDatabase", e.to_string())
}
pub fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
impl Projection {
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        Self::open_limited(path, create, 16 * 1024 * 1024 * 1024)
    }
    pub fn open_limited(path: &Path, create: bool, disk_limit: u64) -> Result<Self> {
        if !create && !path.is_file() {
            return Err(Error::new(
                "RecoveryRequired",
                "index missing; rebuild from facts",
            ));
        }
        if let Some(p) = path.parent() {
            if create {
                crate::atomic::create_directory(p)?;
            } else {
                std::fs::create_dir_all(p)?;
            }
        }
        let db = Database::new(
            path,
            SystemConfig::default()
                .buffer_pool_size(residency::BUFFER_BYTES)
                .max_num_threads(2)
                .max_db_size(disk_limit.min(16 * 1024 * 1024 * 1024))
                .throw_on_wal_replay_failure(true),
        )
        .map_err(err)?;
        let p = Self {
            db,
            queries: Mutex::new(0),
            cache: Mutex::new(BTreeMap::new()),
            imports: Mutex::new(Default::default()),
            import_lock: Mutex::new(()),
            directory: path.parent().unwrap().to_owned(),
        };
        if create {
            let c = p.connection()?;
            for sql in [
                "CREATE NODE TABLE Page(id STRING PRIMARY KEY, bytes STRING)",
                "CREATE NODE TABLE Receipt(id STRING PRIMARY KEY, seal STRING, seq INT64)",
                "CREATE NODE TABLE Slot(id STRING PRIMARY KEY)",
                "CREATE NODE TABLE Fact(id STRING PRIMARY KEY, payload STRING, family STRING, key STRING, kind STRING, name STRING, context STRING)",
                "CREATE REL TABLE LINK(FROM Slot TO Slot, id STRING, payload STRING, kind STRING)",
            ] {
                c.query(sql).map_err(err)?;
            }
        }
        Ok(p)
    }
    fn connection(&self) -> Result<Connection<'_>> {
        let c = Connection::new(&self.db).map_err(err)?;
        c.set_query_timeout(10_000);
        Ok(c)
    }
    pub fn rows(&self, sql: &str) -> Result<Vec<Vec<Value>>> {
        *self.queries.lock().unwrap() += 1;
        Ok(self.connection()?.query(sql).map_err(err)?.collect())
    }
    fn exists(c: &Connection<'_>, table: &str, id: &str) -> Result<bool> {
        let rows: Vec<_> = c
            .query(&format!(
                "MATCH (n:{table} {{id:{}}}) RETURN n.id",
                quote(id)
            ))
            .map_err(err)?
            .collect();
        Ok(!rows.is_empty())
    }
    pub fn receipt(&self, r: &Revision) -> Result<bool> {
        let rows = self.rows(&format!(
            "MATCH (n:Receipt {{id:{}}}) RETURN n.seal",
            quote(&r.revision)
        ))?;
        Ok(matches!(rows.first().and_then(|r|r.first()),Some(Value::String(s)) if s==&r.seal))
    }
    pub fn page(
        &self,
        objects: &crate::facts::objects::Objects,
        r: &ObjectRef,
        timeout: u64,
        io: &mut cq_core::ports::facts::ReadStats,
    ) -> Result<(Arc<Vec<u8>>, u64)> {
        let started = std::time::Instant::now();
        if r.bytes > crate::facts::objects::MAX_OBJECT as u64 {
            return Err(Error::new("IndexIncomplete", "page size exceeds budget"));
        }
        if let Some(bytes) = self.cache.lock().unwrap().get(&r.hash).cloned() {
            if bytes.len() as u64 != r.bytes {
                return Err(Error::new("IndexIncomplete", "page length"));
            }
            return Ok((bytes, 0));
        }
        let c = self.connection()?;
        c.set_query_timeout(timeout.max(1));
        let mut stmt = c
            .prepare("MATCH (p:Page {id:$id}) RETURN p.bytes")
            .map_err(err)?;
        *self.queries.lock().unwrap() += 1;
        let rows: Vec<_> = c
            .execute(&mut stmt, vec![("id", Value::String(r.hash.clone()))])
            .map_err(err)?
            .collect();
        let Some(Value::String(s)) = rows.first().and_then(|r| r.first()) else {
            return Err(Error::new("IndexIncomplete", &r.hash));
        };
        io.bytes_decoded += s.len() as u64;
        let (decoded, extra_queries) = page::load(&c, objects, r, s, started, timeout.max(1), io)?;
        *self.queries.lock().unwrap() += extra_queries;
        let bytes = Arc::new(decoded);
        let mut cache = self.cache.lock().unwrap();
        if cache.values().map(|b| b.len()).sum::<usize>() + bytes.len() > 8 * 1024 * 1024 {
            cache.clear();
        }
        cache.insert(r.hash.clone(), bytes.clone());
        Ok((bytes, 1 + extra_queries))
    }
    pub fn profile(&self, id: &str) -> Result<String> {
        *self.queries.lock().unwrap() += 1;
        Ok(self
            .connection()?
            .query(&format!(
                "PROFILE MATCH (p:Page {{id:{}}}) RETURN p.bytes",
                quote(id)
            ))
            .map_err(err)?
            .to_string())
    }
    pub fn physical_counts(&self) -> Result<serde_json::Value> {
        let mut counts = serde_json::Map::new();
        for (table, name) in [
            ("Page", "pages"),
            ("Fact", "facts"),
            ("Receipt", "receipts"),
        ] {
            let rows = self.rows(&format!("MATCH (n:{table}) RETURN count(n)"))?;
            if let Some(Value::Int64(n)) = rows.first().and_then(|r| r.first()) {
                counts.insert(name.into(), serde_json::json!(n));
            }
        }
        let rows = self.rows("MATCH ()-[r:LINK]->() RETURN count(r)")?;
        if let Some(Value::Int64(n)) = rows.first().and_then(|r| r.first()) {
            counts.insert("links".into(), serde_json::json!(n));
        }
        Ok(serde_json::Value::Object(counts))
    }
    pub fn query_count(&self) -> u64 {
        *self.queries.lock().unwrap()
    }
}
pub mod residency;

mod import;
