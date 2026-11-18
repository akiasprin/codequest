//! R5 component acceptance uses the actual scoped writer, host and Ladybug.
//! Compiler work is deliberately separate; the allocator counts Rust allocations.
use cq_core::{
    model::*,
    ports::{ContentRepository, facts::*},
    query::{QueryRequest, paged::Engine},
    semantic::{
        self,
        records::{Record, Store},
    },
};
use cq_store::{
    LocalStore,
    analysis::{Prepared, ScopeBase, Spool},
    facts::{objects::Objects, writer},
    raw::Journal,
    revisions::StoreHost,
};
use serde_json::{Value, json};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn add(n: usize) {
    let v = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(v, Ordering::Relaxed);
}
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            add(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            add(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            if n >= l.size() {
                add(n - l.size());
            } else {
                LIVE.fetch_sub(l.size() - n, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Tracking = Tracking;
fn reset() -> usize {
    let b = LIVE.load(Ordering::SeqCst);
    PEAK.store(b, Ordering::SeqCst);
    b
}
fn extra(base: usize) -> usize {
    PEAK.load(Ordering::SeqCst).saturating_sub(base)
}
fn io() -> Value {
    let mut m = serde_json::Map::new();
    for line in std::fs::read_to_string("/proc/self/io")
        .unwrap_or_default()
        .lines()
    {
        if let Some((k, n)) = line.split_once(':')
            && let Ok(n) = n.trim().parse::<u64>()
        {
            m.insert(k.into(), json!(n));
        }
    }
    Value::Object(m)
}
fn sample_io(a: &Value, b: &Value) -> Value {
    json!({"read_bytes":b["read_bytes"].as_u64().unwrap_or(0).saturating_sub(a["read_bytes"].as_u64().unwrap_or(0)),"write_bytes":b["write_bytes"].as_u64().unwrap_or(0).saturating_sub(a["write_bytes"].as_u64().unwrap_or(0))})
}
fn prepare(
    store: &LocalStore,
    host: &std::sync::Arc<StoreHost>,
    n: usize,
    v: usize,
) -> cq_core::Result<(Prepared, String)> {
    let mut s = SemanticSnapshot::empty("scale".into(), NativeValue::from("/r5/controlled"));
    s.created_ms = 1;
    s.previous = host.head()?;
    let prov = s.intern_provenance(Provenance {
        provider: "controlled-store-acceptance".into(),
        version: "1".into(),
        authority: Authority::SemanticResolution,
        build_run: None,
        note: "controlled facts, not compiler benchmark".into(),
    });
    let mut units = vec![];
    for name in ["focus", "independent"] {
        let u = semantic_unit(name);
        units.push(u.id.clone());
        s.units.push(u);
    }
    let path = NativeValue::from("/r5/controlled/focus.rs");
    let blob = store
        .content
        .put(b"fn focus(){a()}\nfn a(){}\nfn b(){}\n", 1024)?;
    let f = FileRevision {
        id: digest(&serde_json::to_vec(&(&path, &blob))?),
        logical_path: path,
        blob,
        origin: "controlled".into(),
    };
    let anchor = SourceAnchor {
        file_revision: f.id.clone(),
        start: 0,
        end: 5,
    };
    s.sources = SourceSnapshot::seal(vec![f], InputBinding::PrePostChecked, vec![]);
    let journal = Journal::new(store.root.clone())?;
    for i in 0..n + 3 {
        let name = if i == 0 {
            if v == 0 {
                "focus_zero".into()
            } else {
                "focus_one".into()
            }
        } else if i < 3 {
            format!("target{i}")
        } else {
            format!("independent_{i}")
        };
        let e = Entity {
            id: format!("raw-{i}"),
            stable_handle: None,
            locations: Default::default(),
            unit: units[usize::from(i >= 3)].clone(),
            provider_id: format!("usr:fixed-{i}"),
            kind: EntityKind::Function,
            name: name.clone(),
            qualified_name: format!("contract::{name}"),
            source: (i < 3).then(|| anchor.clone()),
            extent: None,
            signature: "fn()".into(),
            parent: None,
            external: false,
            exported: true,
            attributes: BTreeMap::new(),
            type_definitions: vec![],
            type_navigation: vec![],
            provenance: prov.clone(),
        };
        journal.put(&s, Record::Entity(e), false)?;
    }
    s.add_relation(
        "raw-0",
        if v == 0 { "raw-1" } else { "raw-2" },
        RelationKind::Calls,
        Some(anchor),
        None,
        prov,
    );
    semantic::records::drain(&mut s, &journal)?;
    semantic::record_projection::validate(&s, &journal, &store.content)?;
    let base = if let Some(id) = s.previous.as_deref() {
        ScopeBase::from_view(store, Box::new(host.open_view(Some(id))?))?
    } else {
        ScopeBase::open(store, "scale", None)?
    };
    let mut spool = Spool::new(&s)?;
    let projection = semantic::record_projection::canonicalize(&mut s, &journal, &mut spool)?;
    spool.finish(&mut s, projection, &base)?;
    s.production.as_mut().unwrap().stage_ms.clear();
    s.seal()?;
    let generation = spool.legacy_generation(base.manifest())?;
    let mut focus = String::new();
    spool.each("entity", |_, v| {
        if v["name"].as_str().is_some_and(|n| n.starts_with("focus_")) {
            focus = v["id"].as_str().unwrap().into();
        }
        Ok(())
    })?;
    Ok((
        Prepared {
            header: s,
            spool,
            base,
            generation,
        },
        focus,
    ))
}
fn semantic_unit(name: &str) -> BuildUnit {
    cq_core::analyst::native::unit(cq_core::analyst::native::NativeUnitInput {
        language: Language::Rust,
        run: "controlled".into(),
        name: name.into(),
        root: NativeValue::from(format!("/r5/controlled/{name}.rs").as_str()),
        arguments: vec![],
        cwd: NativeValue::from("/r5/controlled"),
        files: vec![],
        invocation: Observed::Unknown("controlled fixture".into()),
    })
}
fn publish(
    store: &LocalStore,
    h: &std::sync::Arc<StoreHost>,
    p: &Prepared,
) -> cq_core::Result<(writer::Revision, Value)> {
    let o = Objects::buffered(store.root.join("format-2/objects"))?;
    let begin = Instant::now();
    let r = writer::scoped(&o, &store.content, p)?;
    o.sync_pending()?;
    let prepared_ms = begin.elapsed().as_secs_f64() * 1000.;
    let result = h.publish(&r, p.header.previous.as_deref());
    if result.outcome != CommitOutcome::Committed {
        return Err(cq_core::Error::new(
            "AcceptancePublish",
            serde_json::to_string(&result)?,
        ));
    }
    let trace = std::fs::read_dir(h.root.join("publication-traces"))?
        .filter_map(std::result::Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with(&r.revision))
        .ok_or_else(|| cq_core::Error::new("AcceptanceTrace", "publication trace missing"))?;
    let counters = serde_json::from_reader::<_, Value>(std::fs::File::open(trace.path())?)?;
    Ok((
        r,
        json!({"prepare_ms":prepared_ms,"resources":o.resource_stats(),"publication":counters}),
    ))
}
fn rebase(
    store: &LocalStore,
    h: &std::sync::Arc<StoreHost>,
    p: &mut Prepared,
    sequence: usize,
) -> cq_core::Result<()> {
    let head = h.head()?.unwrap();
    p.base = ScopeBase::from_view(store, Box::new(h.open_view(Some(&head))?))?;
    p.header.previous = Some(head);
    p.header.created_ms = sequence as u64 + 10;
    p.header.seal()?;
    let old = &p.base.manifest().unwrap().metadata["semantic_generation"];
    let mut upserts = vec![];
    let old_rows = old["manifest"].as_array().unwrap();
    for v in p.generation["manifest"].as_array().unwrap() {
        if !old_rows.iter().any(|o| o == v) {
            upserts.push(v["key"].clone());
        }
    }
    p.generation["base_generation"] = old["generation"].clone();
    p.generation["sequence"] = json!(old["sequence"].as_u64().unwrap_or(0) + 1);
    p.generation["upserts"] = json!(upserts);
    Ok(())
}
fn main() -> cq_core::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let n: usize = args[0].parse().unwrap();
    let root = Path::new(&args[1]);
    std::fs::create_dir_all(root)?;
    let store = LocalStore::open(&root.join("store"))?;
    let h = StoreHost::open(&store.root, "scale")?;
    let start = Instant::now();
    let (mut a, subject) = prepare(&store, &h, n, 0)?;
    let reuse = args.get(2).is_some_and(|arg| arg == "reuse");
    let (r1, seed) = if reuse {
        let r = a
            .base
            .manifest()
            .expect("reuse needs an existing committed seed")
            .clone();
        assert_eq!(
            r.metadata["counts"]["entities"],
            a.spool.counts()["entities"]
        );
        (
            r,
            json!({"reused_existing_seed":true,"preparation_only":true}),
        )
    } else {
        publish(&store, &h, &a)?
    };
    let seed_ms = start.elapsed().as_secs_f64() * 1000.;
    let (mut b, other) = prepare(&store, &h, n, 1)?;
    assert_eq!(subject, other);
    let before = LIVE.load(Ordering::SeqCst);
    let mut queries = vec![];
    let q = QueryRequest {
        operation: "callees".into(),
        subject: Some(subject.clone()),
        limit: 7,
        ..Default::default()
    };
    for _ in 0..32 {
        let baseline = reset();
        let start = Instant::now();
        let view = h.open_view(None)?;
        let result = Engine {
            reader: &view,
            model: None,
            control: None,
        }
        .execute(&q, None)?;
        assert_eq!(result.page.items.len(), 1);
        queries.push(json!({"ms":start.elapsed().as_secs_f64()*1000.,"rust_peak_extra":extra(baseline),"reads":view.stats()}));
    }
    let held = h.open_view(Some(&r1.revision))?;
    let mut updates = vec![];
    for i in 0..32 {
        let p = if i % 2 == 0 { &mut b } else { &mut a };
        rebase(&store, &h, p, i)?;
        let baseline = reset();
        let before_io = io();
        let start = Instant::now();
        let (r, metrics) = publish(&store, &h, p)?;
        let peak = extra(baseline);
        updates.push(json!({"ms":start.elapsed().as_secs_f64()*1000.,"rust_peak_extra":peak,"io":sample_io(&before_io,&io()),"changes":r.changes,"metrics":metrics,"transfer":r.metadata["transfer"]}));
        let view = h.open_view(None)?;
        let result = Engine {
            reader: &view,
            model: None,
            control: None,
        }
        .execute(&q, None)?;
        assert_eq!(result.page.items.len(), 1);
        assert!(
            result.page.items[0]["entity"]["name"]
                == if i % 2 == 0 { "target2" } else { "target1" }
        );
    }
    let old = Engine {
        reader: &held,
        model: None,
        control: None,
    }
    .execute(&q, None)?;
    assert_eq!(old.page.items[0]["entity"]["name"], "target1");
    let report = json!({"independent_entities":n,"seed_reused":reuse,"seed_ms":seed_ms,"seed":seed,"persistent_rust_bytes_before_tasks":before,"query":queries,"updates":updates,"allocator":"Rust GlobalAlloc live/peak; excludes native C++ malloc; baseline includes prepared identity directories","native_rss":"measure separately with /usr/bin/time -v or /proc sampling","source":"controlled scoped facts, compiler/export costs not included"});
    std::fs::write(
        root.join("results.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
