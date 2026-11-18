//! Real Ladybug projection. Not used by LocalStore or the product query engine.
use crate::model::*;
use lbug::{Connection, Database, SystemConfig, Value};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

pub fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("string serialization")
}
pub fn config() -> SystemConfig {
    SystemConfig::default()
        .buffer_pool_size(512 * 1024 * 1024)
        .max_num_threads(4)
        .max_db_size(2 * 1024 * 1024 * 1024)
        .throw_on_wal_replay_failure(true)
}
pub fn string(v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        _ => Err(format!("expected STRING, got {v:?}").into()),
    }
}
pub fn integer(v: &Value) -> Result<i64> {
    match v {
        Value::Int64(n) => Ok(*n),
        _ => Err(format!("expected INT64, got {v:?}").into()),
    }
}
pub struct Projection {
    pub db: Database,
    pub path: PathBuf,
    log: Mutex<File>,
}
impl Projection {
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        if create {
            fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
        } else {
            require(
                path.is_file(),
                "MissingIndex: refusing to create an empty replacement",
            )?;
        }
        let db = Database::new(path, config())?;
        let log = Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.with_extension("sql.jsonl"))?,
        );
        let p = Self {
            db,
            path: path.into(),
            log,
        };
        if create {
            p.schema()?;
        }
        Ok(p)
    }
    pub fn connection(&self) -> Result<Connection<'_>> {
        let c = Connection::new(&self.db)?;
        c.set_query_timeout(10_000);
        Ok(c)
    }
    pub fn rows(&self, c: &Connection<'_>, sql: &str) -> Result<Vec<Vec<Value>>> {
        let started = Instant::now();
        let result = c.query(sql);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.;
        let mut log = self.log.lock().map_err(|_| "poisoned SQL log")?;
        match result {
            Ok(result) => {
                let rows: Vec<_> = result.collect();
                writeln!(
                    log,
                    "{}",
                    json!({"sql":sql,"milliseconds":elapsed_ms,"rows":rows.len(),"success":true})
                )?;
                Ok(rows)
            }
            Err(e) => {
                writeln!(
                    log,
                    "{}",
                    json!({"sql":sql,"milliseconds":elapsed_ms,"success":false,"error":e.to_string()})
                )?;
                Err(e.into())
            }
        }
    }
    pub fn exec(&self, c: &Connection<'_>, sql: &str) -> Result<()> {
        self.rows(c, sql).map(|_| ())
    }
    pub fn count(&self, c: &Connection<'_>, sql: &str) -> Result<i64> {
        let rows = self.rows(c, sql)?;
        integer(
            rows.first()
                .and_then(|r| r.first())
                .ok_or("no count result")?,
        )
    }
    fn schema(&self) -> Result<()> {
        let c = self.connection()?;
        for sql in [
            "CREATE NODE TABLE EntitySlot(id STRING PRIMARY KEY, context STRING)",
            "CREATE NODE TABLE EntityFact(id STRING PRIMARY KEY, name STRING, doc STRING)",
            "CREATE NODE TABLE Payload(id STRING PRIMARY KEY, context STRING)",
            "CREATE NODE TABLE ShardUse(id STRING PRIMARY KEY, shard STRING, born INT64, retired INT64)",
            "CREATE NODE TABLE Receipt(id STRING PRIMARY KEY, sequence INT64, manifest STRING)",
            "CREATE NODE TABLE Pending(id STRING PRIMARY KEY, json STRING)",
            "CREATE NODE TABLE AdjacencyPage(id STRING PRIMARY KEY, payload STRING, context STRING, subject STRING, items STRING)",
            "CREATE NODE TABLE AdjacencyHead(id STRING PRIMARY KEY, directory STRING)",
            "CREATE REL TABLE OWNS(FROM Payload TO EntityFact)",
            "CREATE REL TABLE DEFINES(FROM EntityFact TO EntitySlot)",
            "CREATE REL TABLE USES(FROM ShardUse TO Payload)",
            "CREATE REL TABLE CALLS(FROM EntitySlot TO EntitySlot, id STRING, payload STRING, authority STRING)",
        ] {
            self.exec(&c, sql)?;
        }
        Ok(())
    }
    fn csv(
        &self,
        c: &Connection<'_>,
        table: &str,
        headers: &[&str],
        rows: &[Vec<String>],
        dir: &Path,
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let path = dir.join(format!("{table}.csv"));
        let mut w = csv::Writer::from_path(&path)?;
        w.write_record(headers)?;
        for row in rows {
            w.write_record(row)?;
        }
        w.flush()?;
        self.exec(
            c,
            &format!(
                "COPY {table} FROM {} (HEADER=true, PARALLEL=false)",
                quote(path.to_str().ok_or("CSV path must be UTF-8")?)
            ),
        )
    }
    pub fn ensure_slots(&self, c: &Connection<'_>, payloads: &[Payload]) -> Result<()> {
        // Forward references are legal: declare all endpoints before importing
        // any shard's relationships. Shard sort order must not decide validity.
        let mut groups = std::collections::BTreeMap::<String, BTreeSet<String>>::new();
        for payload in payloads {
            let hash = digest(&serde_json::to_vec(payload)?);
            if self.count(
                c,
                &format!("MATCH (p:Payload {{id:{}}}) RETURN count(*)", quote(&hash)),
            )? != 0
            {
                continue;
            }
            for node in &payload.nodes {
                require(
                    node.id.starts_with(&format!("{}::", payload.context)),
                    "fixture slot/context mismatch",
                )?;
                groups
                    .entry(payload.context.clone())
                    .or_default()
                    .insert(node.id.clone());
            }
        }
        for (context, ids) in groups {
            let ids = ids.into_iter().collect::<Vec<_>>();
            for chunk in ids.chunks(256) {
                self.exec(c,&format!("UNWIND {} AS slot_id MERGE (s:EntitySlot {{id:slot_id}}) ON CREATE SET s.context={}",serde_json::to_string(chunk)?,quote(&context)))?;
            }
        }
        Ok(())
    }
    pub fn import(
        &self,
        c: &Connection<'_>,
        hash: &str,
        payload: &Payload,
        stats: &mut ImportStats,
    ) -> Result<()> {
        if self.count(
            c,
            &format!("MATCH (p:Payload {{id:{}}}) RETURN count(*)", quote(hash)),
        )? > 0
        {
            return Ok(());
        }
        let dir = tempfile::tempdir_in(self.path.parent().ok_or("missing parent")?)?;
        let mut facts = vec![];
        let mut owns = vec![];
        let mut defines = vec![];
        for (i, n) in payload.nodes.iter().enumerate() {
            let fact = format!("{hash}:{i}");
            facts.push(vec![fact.clone(), n.name.clone(), n.doc.clone()]);
            owns.push(vec![hash.into(), fact.clone()]);
            defines.push(vec![fact, n.id.clone()]);
        }
        self.csv(
            c,
            "Payload",
            &["id", "context"],
            &[vec![hash.into(), payload.context.clone()]],
            dir.path(),
        )?;
        self.csv(c, "EntityFact", &["id", "name", "doc"], &facts, dir.path())?;
        self.csv(c, "OWNS", &["from", "to"], &owns, dir.path())?;
        self.csv(c, "DEFINES", &["from", "to"], &defines, dir.path())?;
        let edges: Vec<_> = payload
            .edges
            .iter()
            .map(|e| {
                Ok(vec![
                    e.from.clone(),
                    e.to.clone(),
                    edge_id(hash, e)?,
                    hash.into(),
                    "synthetic_fixture".into(),
                ])
            })
            .collect::<Result<_>>()?;
        self.csv(
            c,
            "CALLS",
            &["from", "to", "id", "payload", "authority"],
            &edges,
            dir.path(),
        )?;
        stats.adjacency_pages += self.import_adjacency_pages(c, hash, payload, dir.path())?;
        stats.payloads += 1;
        stats.nodes += payload.nodes.len();
        stats.edges += payload.edges.len();
        Ok(())
    }
    fn import_adjacency_pages(
        &self,
        c: &Connection<'_>,
        hash: &str,
        payload: &Payload,
        dir: &Path,
    ) -> Result<usize> {
        let bundles = adjacency_bundles(hash, payload)?;
        let mut pages = vec![];
        let mut heads = vec![];
        for (subject, bundle) in bundles {
            heads.push(vec![
                Self::adjacency_head_key(hash, &subject),
                serde_json::to_string(&bundle.directory)?,
            ]);
            for (ordinal, items) in bundle.items.into_iter().enumerate() {
                pages.push(vec![
                    Self::adjacency_key(hash, &subject, ordinal),
                    hash.into(),
                    payload.context.clone(),
                    subject.clone(),
                    items,
                ]);
            }
        }
        self.csv(c, "AdjacencyHead", &["id", "directory"], &heads, dir)?;
        self.csv(
            c,
            "AdjacencyPage",
            &["id", "payload", "context", "subject", "items"],
            &pages,
            dir,
        )?;
        Ok(pages.len())
    }
    pub fn adjacency_key(hash: &str, subject: &str, ordinal: usize) -> String {
        format!("{hash}:{subject}:{ordinal:08}")
    }
    pub fn adjacency_head_key(hash: &str, subject: &str) -> String {
        format!("{hash}:{subject}")
    }
    pub fn adjacency_sql() -> &'static str {
        "MATCH (p:AdjacencyPage {id:$key}) RETURN p.payload,p.context,p.subject,p.items"
    }
    pub fn adjacency_page(
        &self,
        c: &Connection<'_>,
        hash: &str,
        context: &str,
        subject: &str,
        ordinal: usize,
        expected_root: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut head = c.prepare("MATCH (h:AdjacencyHead {id:$key}) RETURN h.directory")?;
        let mut metadata = c.execute(
            &mut head,
            vec![(
                "key",
                Value::String(Self::adjacency_head_key(hash, subject)),
            )],
        )?;
        let row = metadata
            .next()
            .ok_or("IndexIncomplete: adjacency directory missing")?;
        let bytes = string(&row[0])?;
        require(
            bytes.len() <= 64 * 1024 && digest(bytes.as_bytes()) == expected_root,
            "IndexIncomplete: adjacency directory digest",
        )?;
        let directory: AdjacencyDirectory = serde_json::from_str(&bytes)?;
        require(
            directory.payload == hash
                && directory.context == context
                && directory.subject == subject,
            "AdjacencyScopeMismatch",
        )?;
        require(ordinal <= directory.pages.len(), "InvalidAdjacencyCursor")?;
        if ordinal == directory.pages.len() {
            return Ok(vec![]);
        }
        let proof = &directory.pages[ordinal];
        let mut statement = c.prepare(Self::adjacency_sql())?;
        let mut result = c.execute(
            &mut statement,
            vec![(
                "key",
                Value::String(Self::adjacency_key(hash, subject, ordinal)),
            )],
        )?;
        let Some(row) = result.next() else {
            return Err("IndexIncomplete: expected adjacency page missing".into());
        };
        require(
            string(&row[0])? == hash && string(&row[1])? == context && string(&row[2])? == subject,
            "AdjacencyScopeMismatch",
        )?;
        let items = string(&row[3])?;
        require(
            items.len() <= 16 * 1024
                && items.len() == proof.bytes
                && digest(items.as_bytes()) == proof.hash,
            "IndexIncomplete: adjacency page digest",
        )?;
        let edges: Vec<(String, String)> = serde_json::from_str(&items)?;
        require(
            edges.len() <= 7 && edges.len() == proof.rows,
            "IndexIncomplete: adjacency row count",
        )?;
        Ok(edges)
    }
    pub fn entities(
        &self,
        c: &Connection<'_>,
        sequence: i64,
        context: &str,
        text: &str,
    ) -> Result<Vec<Vec<Value>>> {
        let filter = if text.is_empty() {
            "true".to_owned()
        } else {
            format!(
                "(f.name CONTAINS {} OR f.doc CONTAINS {})",
                quote(text),
                quote(text)
            )
        };
        self.rows(c,&format!("MATCH (u:ShardUse)-[:USES]->(:Payload)-[:OWNS]->(f:EntityFact)-[:DEFINES]->(s:EntitySlot) WHERE u.born <= {sequence} AND u.retired > {sequence} AND s.context = {} AND {filter} RETURN s.id, f.name, f.doc ORDER BY s.id",quote(context)))
    }
    pub fn edges_sql(
        sequence: i64,
        context: &str,
        subject: &str,
        after: &str,
        limit: usize,
        fenced: bool,
    ) -> String {
        let endpoints = if fenced {
            ", (su:ShardUse)-[:USES]->(:Payload)-[:OWNS]->(:EntityFact)-[:DEFINES]->(s), (tu:ShardUse)-[:USES]->(:Payload)-[:OWNS]->(:EntityFact)-[:DEFINES]->(t)"
        } else {
            ""
        };
        let fence = if fenced {
            format!(
                " AND su.born <= {sequence} AND su.retired > {sequence} AND tu.born <= {sequence} AND tu.retired > {sequence}"
            )
        } else {
            String::new()
        };
        format!(
            "MATCH (s:EntitySlot)-[e:CALLS]->(t:EntitySlot), (u:ShardUse)-[:USES]->(p:Payload){endpoints} WHERE s.context = {} AND t.context = {} AND s.id = {} AND p.id = e.payload AND u.born <= {sequence} AND u.retired > {sequence} AND e.id > {}{fence} RETURN e.id, t.id ORDER BY e.id LIMIT {limit}",
            quote(context),
            quote(context),
            quote(subject),
            quote(after)
        )
    }
    pub fn edges(
        &self,
        c: &Connection<'_>,
        sequence: i64,
        context: &str,
        subject: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<Vec<Value>>> {
        self.rows(
            c,
            &Self::edges_sql(sequence, context, subject, after, limit, true),
        )
    }
    pub fn check_endpoints(&self, c: &Connection<'_>, sequence: i64) -> Result<()> {
        // Deliberately full-visible-range validation in R0, measured separately.
        // This is not evidence for a delta-only production validator.
        let nodes = self.rows(c,&format!("MATCH (u:ShardUse)-[:USES]->(:Payload)-[:OWNS]->(:EntityFact)-[:DEFINES]->(s:EntitySlot) WHERE u.born <= {sequence} AND u.retired > {sequence} RETURN s.id"))?;
        let ids: BTreeSet<_> = nodes.iter().map(|r| string(&r[0])).collect::<Result<_>>()?;
        require(ids.len() == nodes.len(), "duplicate visible declaration")?;
        let edges = self.rows(c,&format!("MATCH (s:EntitySlot)-[e:CALLS]->(t:EntitySlot), (u:ShardUse)-[:USES]->(p:Payload) WHERE p.id=e.payload AND u.born <= {sequence} AND u.retired > {sequence} RETURN s.id,t.id"))?;
        for e in edges {
            require(
                ids.contains(&string(&e[0])?) && ids.contains(&string(&e[1])?),
                "dangling visible endpoint",
            )?;
        }
        Ok(())
    }
    pub fn profile(&self, sql: &str) -> Result<String> {
        let c = self.connection()?;
        Ok(c.query(&format!("PROFILE {sql}"))?.to_string())
    }
}
