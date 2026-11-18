//! Actual compaction of a retained real graph; the old reader keeps its epoch.
use cq_core::{
    ports::facts::FactReader,
    query::{QueryRequest, paged::Engine},
};
use cq_store::revisions::StoreHost;
use serde_json::{Value, json};
use std::{path::Path, time::Instant};
fn check(view: &dyn FactReader, subject: &str) -> cq_core::Result<()> {
    let r = Engine {
        reader: view,
        model: None,
        control: None,
    }
    .execute(
        &QueryRequest {
            operation: "details".into(),
            subject: Some(subject.into()),
            ..Default::default()
        },
        None,
    )?;
    assert_eq!(r.page.items[0]["entity"]["id"], subject);
    Ok(())
}
fn main() -> cq_core::Result<()> {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    let h = StoreHost::open(Path::new(&a[0]), &a[1])?;
    let head = h.head()?;
    let old = h.open_view(Some(&a[2]))?;
    check(&old, &a[3])?;
    let started = Instant::now();
    let result = h.compact()?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(h.head()?, head);
    for k in ["facts", "pages", "links"] {
        assert_eq!(result["physical_before"][k], result["physical_after"][k]);
    }
    assert_eq!(old.revision(), a[2]);
    check(&old, &a[3])?;
    check(&h.open_view(None)?, &a[3])?;
    drop(old);
    let collected = h.collect_epochs()?;
    let v: Value = json!({"compaction_ms":elapsed,"result":result,"old_reader_after_cutover":true,"head_unchanged":true,"collected_after_reader_release":collected,"residency":cq_store::ladybug::residency::diagnostics()});
    std::fs::write(&a[4], serde_json::to_vec_pretty(&v)?)?;
    Ok(())
}
