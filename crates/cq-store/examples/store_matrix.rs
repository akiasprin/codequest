//! Version/residency matrix over a frozen real graph. History entries are
//! synthetic metadata revisions; they are not claimed as new compiler builds.
use cq_core::{
    model::digest,
    ports::facts::*,
    query::{QueryRequest, paged::Engine},
};
use cq_store::{
    facts::{objects::Objects, writer},
    revisions::StoreHost,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Instant};
fn publish(
    h: &Arc<StoreHost>,
    base: &writer::Revision,
    nonce: usize,
    first: bool,
) -> cq_core::Result<writer::Revision> {
    let o = Objects::buffered(h.objects.root.clone())?;
    let mut w = writer::Writer::new(&o, base.roots.clone());
    w.append(
        "metadata",
        "r5_observation",
        &json!({"synthetic_history":true,"nonce":nonce}),
    )?;
    w.flush()?;
    o.sync_pending()?;
    let mut r = base.clone();
    r.roots = w.roots;
    r.changes = w.stats;
    r.parent = if first {
        None
    } else {
        Some(base.revision.clone())
    };
    r.sequence = if first { 1 } else { base.sequence + 1 };
    r.revision = digest(format!("r5:{}:{nonce}", base.revision).as_bytes());
    r.metadata["revision"] = json!(r.revision);
    r.metadata["previous"] = json!(r.parent);
    r.metadata["synthetic_history"] = json!(true);
    if first {
        r.changes = writer::WriteStats::default();
        r.changes.upserts = r.roots.facts.clone();
        let mut n = 0;
        cq_store::facts::tree::visit(&o, &r.roots.facts, |_, _| {
            n += 1;
            Ok(())
        })?;
        r.changes.facts_changed = n;
    }
    r.seal()?;
    let result = h.publish(&r, r.parent.as_deref());
    if result.outcome != CommitOutcome::Committed {
        return Err(cq_core::Error::new(
            "MatrixPublish",
            format!("{:?}", result),
        ));
    }
    Ok(r)
}
fn query(
    h: &Arc<StoreHost>,
    subject: &str,
    revision: &str,
    nonce: usize,
) -> cq_core::Result<Value> {
    let now = Instant::now();
    let v = h.open_view(Some(revision))?;
    assert_eq!(v.manifest().revision, revision);
    let marker = cq_core::ports::facts::FactReader::get(&v, "metadata", "r5_observation")?
        .expect("version-specific marker");
    assert_eq!(marker.value["nonce"], nonce);
    let q = QueryRequest {
        operation: "details".into(),
        subject: Some(subject.into()),
        limit: 7,
        timeout_ms: 30000,
        ..Default::default()
    };
    let engine = Engine {
        reader: &v,
        model: None,
        control: None,
    };
    let result = engine.execute(&q, None)?;
    assert_eq!(result.page.items[0]["entity"]["id"], subject);
    let calls = engine.execute(
        &QueryRequest {
            operation: "callees".into(),
            ..q
        },
        None,
    )?;
    assert!(
        !calls.page.items.is_empty(),
        "select a real caller for the matrix"
    );
    for item in &calls.page.items {
        assert_eq!(item["relation"]["from"], subject);
        assert_eq!(item["relation"]["to"], item["entity"]["id"]);
    }
    let fingerprint = digest(&serde_json::to_vec(&calls.page.items)?);
    let rss_bytes = std::fs::read_to_string("/proc/self/status")?
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024;
    Ok(
        json!({"ms":now.elapsed().as_secs_f64()*1000.,"reads":v.stats(),"rss_bytes":rss_bytes,"callees":calls.page.items.len(),"fingerprint":fingerprint,"revision":revision,"nonce":nonce}),
    )
}
fn main() -> cq_core::Result<()> {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    let source = Path::new(&a[0]);
    let project = &a[1];
    let rev = &a[2];
    let subject = &a[3];
    let out = Path::new(&a[4]);
    std::fs::create_dir_all(out)?;
    let key = digest(project.as_bytes());
    let original: writer::Revision = serde_json::from_reader(std::fs::File::open(
        source
            .join("format-2/projects")
            .join(&key)
            .join("commits")
            .join(rev),
    )?)?;
    let mut hosts = vec![];
    let mut heads = vec![];
    let mut startup = vec![];
    for i in 0..3 {
        let root = out.join(format!("store-{i}"));
        std::fs::create_dir_all(root.join("format-2"))?;
        let now = Instant::now();
        // Embedded queries/rebuild read canonical pages and frozen source blocks
        // from objects. Raw compiler exports remain in the original evidence
        // store; copying them is not part of this query/retention fixture.
        let result = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(source.join("format-2/objects"))
            .arg(root.join("format-2/objects"))
            .status()?;
        assert!(result.success());
        let h = StoreHost::open(&root, project)?;
        let r = publish(&h, &original, 0, true)?;
        h.pin_cursor("r5-0", &r.revision)?;
        startup.push(json!({"store":i,"bootstrap_ms":now.elapsed().as_secs_f64()*1000.}));
        hosts.push(h);
        heads.push(r);
    }
    let mut cells = vec![];
    let mut history = vec![(heads[0].revision.clone(), 0)];
    let mut old = vec![];
    let mut updates = vec![];
    for n in 1..=64 {
        if n > 1 {
            for i in 0..3 {
                let start = Instant::now();
                heads[i] = publish(&hosts[i], &heads[i], n, false)?;
                hosts[i].pin_cursor(&format!("r5-{n}"), &heads[i].revision)?;
                updates.push(start.elapsed().as_secs_f64() * 1000.);
            }
            history.push((heads[0].revision.clone(), n));
        }
        if [1, 8, 64].contains(&n) {
            for projects in [1, 3] {
                for concurrent in [1, 8] {
                    let mut samples = vec![];
                    std::thread::scope(|scope| -> cq_core::Result<()> {
                        let handles = (0..concurrent)
                            .map(|worker| {
                                let hosts = &hosts;
                                let history = &history;
                                let subject = subject.clone();
                                scope.spawn(move || -> cq_core::Result<Vec<Value>> {
                                    let mut rows = vec![];
                                    for j in 0..(32 / concurrent) {
                                        let ordinal = worker * (32 / concurrent) + j;
                                        let (revision, nonce) =
                                            &history[ordinal * (history.len() - 1) / 31];
                                        rows.push(query(
                                            &hosts[(worker + j) % projects],
                                            &subject,
                                            revision,
                                            *nonce,
                                        )?);
                                    }
                                    Ok(rows)
                                })
                            })
                            .collect::<Vec<_>>();
                        for task in handles {
                            samples.extend(task.join().unwrap()?);
                        }
                        Ok(())
                    })?;
                    cells.push(json!({"projects":projects,"concurrent":concurrent,"retained_revisions":n,"samples":samples,"residency":cq_store::ladybug::residency::diagnostics()}));
                }
            }
        }
    }
    let fingerprint = &cells[0]["samples"][0]["fingerprint"];
    for cell in &cells {
        for sample in cell["samples"].as_array().unwrap() {
            assert_eq!(&sample["fingerprint"], fingerprint);
            assert_eq!(sample["reads"]["full_snapshot_decodes"], 0);
        }
    }
    for h in &hosts {
        let plan = h.preview_retention(1)?;
        assert!(plan.revisions.is_empty());
        old.push(json!({"protected_revisions":64,"orphan_objects":plan.objects.len(),"orphan_bytes":plan.bytes}));
    }
    let result = json!({"source_store":source,"source_revision":rev,"source_counts":original.metadata["counts"],"operation":"details plus first 7 callees and version-specific marker; exact caller results equal across all cells; raw compiler exports stay in original source store","history":"metadata-only synthetic versions over frozen real compiler facts; all 64 revisions explicitly cursor-pinned; sampled historical revisions verified by distinct marker","bootstrap":startup,"cells":cells,"updates_ms":updates,"retention":old,"residency":cq_store::ladybug::residency::diagnostics()});
    std::fs::write(
        out.join("results.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(())
}
