//! Replay a bounded prefix of a retained, task-owned raw journal. This measures
//! physical transport, normalized storage and shard sealing, not semantic
//! reconstruction or a complete compiler-to-query run.
use cq_core::{
    Result,
    model::*,
    semantic::{
        normalized::{Projection, Sink},
        production::Production,
        records::{Record, Source, Store},
    },
};
use cq_store::{
    LocalStore,
    analysis::{ScopeBase, Spool},
    raw::Journal,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::BufReader,
    path::PathBuf,
    time::Instant,
};
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let input = PathBuf::from(&args[0]);
    let count: usize = args[1].parse().unwrap();
    let output = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir_in(&output)?;
    let store = LocalStore::open(temp.path())?;
    let mut header =
        SemanticSnapshot::empty("retained-block".into(), NativeValue::from("/retained"));
    header.production = Some(Production::default());
    let journal = Journal::new(temp.path().into())?;
    let mut expected = BTreeMap::new();
    let mut family_counts = BTreeMap::<String, u64>::new();
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut actual = 0;
    let started = Instant::now();
    let reader = BufReader::with_capacity(8 * 1024 * 1024, std::fs::File::open(&input)?);
    let mut stream = serde_json::Deserializer::from_reader(reader).into_iter::<Record>();
    for r in stream.by_ref().take(count) {
        let r = r?;
        let encoded = serde_json::to_vec(&r)?;
        expected.insert(
            format!("{}:{}", r.family(), r.id()),
            (digest(&encoded), digest(&serde_json::to_vec(&r.value()?)?)),
        );
        *family_counts.entry(r.family().into()).or_default() += 1;
        bytes += encoded.len();
        rows.push(r);
        actual += 1;
        if rows.len() >= 4096 || bytes >= 8 * 1024 * 1024 {
            journal.put_batch(&header, std::mem::take(&mut rows), true)?;
            bytes = 0;
        }
    }
    if !rows.is_empty() {
        journal.put_batch(&header, rows, true)?;
    }
    let ingest = started.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut observed = BTreeMap::new();
    journal.visit(&mut |r| {
        observed.insert(
            format!("{}:{}", r.family(), r.id()),
            digest(&serde_json::to_vec(&r)?),
        );
        Ok(())
    })?;
    assert_eq!(observed.len(), expected.len());
    for (key, hash) in &observed {
        assert_eq!(hash, &expected[key].0, "raw record {key}");
    }
    let scan = t.elapsed().as_secs_f64();
    let mut spool = Spool::new(&header)?;
    let mut sources = BTreeMap::from([("$metadata".into(), None)]);
    let t = Instant::now();
    journal.visit_batches(None, &mut |rows| {
        let rows = rows
            .into_iter()
            .map(|r| {
                let (unit, source) = match &r {
                    Record::Entity(v) => (v.unit.clone(), v.source.as_ref()),
                    Record::Body(v) => (v.unit.clone(), Some(&v.source)),
                    Record::Occurrence(v) => (v.unit.clone(), Some(&v.source)),
                    Record::Relation(v) => ("$project".into(), v.source.as_ref()),
                    Record::Document(v) => ("$project".into(), Some(&v.source)),
                };
                let shard = source
                    .map(|a| a.file_revision.clone())
                    .unwrap_or_else(|| "$metadata".into());
                sources.entry(shard.clone()).or_insert(None);
                (unit, shard, r)
            })
            .collect();
        spool.batch(rows)
    })?;
    let spool_seconds = t.elapsed().as_secs_f64();
    let base = ScopeBase::open(&store, &header.project, None)?;
    let t = Instant::now();
    spool.finish(
        &mut header,
        Projection {
            universe: digest(b"retained-benchmark"),
            provenance: BTreeMap::new(),
            sources,
        },
        &base,
    )?;
    let seal_seconds = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let generation = spool.legacy_generation(None)?;
    let shard_seconds = t.elapsed().as_secs_f64();
    let mut identities = BTreeSet::new();
    for family in ["entity", "body", "occurrence", "relation", "document"] {
        spool.each(family, |k, value| {
            assert_eq!(
                digest(&serde_json::to_vec(&value)?),
                expected[k].1,
                "normalized payload {k}"
            );
            identities.insert(k.to_owned());
            Ok(())
        })?;
    }
    assert_eq!(identities, expected.keys().cloned().collect());
    let result = json!({"input":input,"requested_records":count,"input_records":actual,"live_records":expected.len(),"family_counts":family_counts,"workers":cq_core::compute::workers()?,"ingest_seconds":ingest,"sequential_scan_verify_seconds":scan,"spool_encode_seconds":spool_seconds,"scope_seal_seconds":seal_seconds,"shard_hash_seconds":shard_seconds,"shards":generation["manifest"].as_array().unwrap().len(),"scope":"retained real raw payload transport, encoding and shard sealing; opaque identities retained; no compiler, semantic identity remapping, graph publication or query benchmark","passed":true});
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!("{result}");
    Ok(())
}
