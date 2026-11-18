//! Manual replay benchmark over retained real compiler exports. This does not
//! measure compiler capture, canonicalization, publication or graph queries.
use cq_core::{
    model::*,
    ports::ContentRepository,
    semantic::{
        native_archive::Archive,
        records::{Record, Source, Store},
    },
};
use cq_store::{LocalStore, raw::Journal};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::BufReader,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

#[derive(Deserialize)]
struct Case {
    project: String,
    root: PathBuf,
    language: String,
    exports: Vec<PathBuf>,
}
struct Sink {
    journal: Journal,
    active: AtomicUsize,
    planning: AtomicUsize,
}
impl Source for Sink {
    fn get(&self, family: &str, id: &str) -> cq_core::Result<Option<Record>> {
        self.journal.get(family, id)
    }
    fn visit(&self, f: &mut dyn FnMut(Record) -> cq_core::Result<()>) -> cq_core::Result<()> {
        self.journal.visit(f)
    }
}
impl Store for Sink {
    fn put(&self, h: &SemanticSnapshot, r: Record, replace: bool) -> cq_core::Result<()> {
        self.journal.put(h, r, replace)
    }
    fn put_batch(
        &self,
        h: &SemanticSnapshot,
        r: Vec<Record>,
        replace: bool,
    ) -> cq_core::Result<()> {
        self.journal.put_batch(h, r, replace)
    }
    fn progress_workers(&self, p: &str, n: u64, total: u64, workers: usize) {
        self.journal.progress_workers(p, n, total, workers)
    }
    fn progress_pipeline(&self, p: &str, n: u64, total: u64, v: &cq_core::progress::Pipeline) {
        self.active.fetch_max(v.active, Ordering::Relaxed);
        self.planning.fetch_max(v.planning, Ordering::Relaxed);
        self.journal.progress_pipeline(p, n, total, v);
    }
}
fn cpu() -> f64 {
    let mut r = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, r.as_mut_ptr()) },
        0
    );
    let r = unsafe { r.assume_init() };
    r.ru_utime.tv_sec as f64
        + r.ru_utime.tv_usec as f64 / 1e6
        + r.ru_stime.tv_sec as f64
        + r.ru_stime.tv_usec as f64 / 1e6
}
fn run() -> cq_core::Result<Value> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        return Err(cq_core::Error::new(
            "Usage",
            "bench_native CASE.json OUTPUT_DIR",
        ));
    }
    let case: Case = serde_json::from_reader(BufReader::new(std::fs::File::open(&args[0])?))?;
    let output = PathBuf::from(&args[1]);
    std::fs::create_dir_all(&output)?;
    let language = match case.language.as_str() {
        "rust" => Language::Rust,
        "go" => Language::Go,
        "c" => Language::C,
        "cpp" => Language::Cpp,
        _ => return Err(cq_core::Error::new("Language", "unsupported")),
    };
    let store = LocalStore::open(&output)?;
    let sink = Sink {
        journal: Journal::new(output.clone())?,
        active: AtomicUsize::new(0),
        planning: AtomicUsize::new(0),
    };
    let mut seed = SemanticSnapshot::empty(case.project.clone(), NativeValue::new(&case.root));
    let mut units = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut times = vec![];
    let mut frames = 0usize;
    for export in &case.exports {
        let t = Instant::now();
        let raw: Value = serde_json::from_reader(BufReader::new(std::fs::File::open(export)?))?;
        let version = raw["version"].as_str().unwrap_or("native-v1").to_owned();
        let mut mapping = vec![];
        let files = raw["files"]
            .as_array()
            .ok_or_else(|| cq_core::Error::new("Export", "missing files"))?;
        frames += files.len() + 1;
        for f in files {
            let path = f["path"].as_str().unwrap();
            paths.insert(path.to_owned());
            let blob = store
                .content
                .put(f["text"].as_str().unwrap().as_bytes(), 1024 * 1024 * 1024)?;
            if let Some(unit) = f["unit"].as_str() {
                let logical_path = NativeValue::from(path);
                let file = FileRevision {
                    id: digest(&serde_json::to_vec(&(&logical_path, &blob))?),
                    logical_path,
                    blob,
                    origin: "retained-export-replay".into(),
                };
                mapping.push(MaterializedInput {
                    path: path.into(),
                    file,
                    unit: unit.into(),
                });
                if units.insert(unit.to_owned()) {
                    let mut u =
                        cq_core::analyst::native::unit(cq_core::analyst::native::NativeUnitInput {
                            language,
                            run: "replay".into(),
                            name: unit.into(),
                            root: NativeValue::new(&case.root),
                            arguments: vec![],
                            cwd: NativeValue::new(&case.root),
                            files: vec![],
                            invocation: Observed::Unknown("replayed compiler export".into()),
                        });
                    u.id = unit.into();
                    seed.units.push(u);
                }
            }
        }
        drop(raw);
        let setup = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let archive = Archive::open(export)?;
        let archive_seconds = t.elapsed().as_secs_f64();
        let start_cpu = cpu();
        let t = Instant::now();
        archive.normalize(
            &mut seed,
            &mapping,
            &store.content,
            "real-export-bench",
            language,
            &version,
            &sink,
        )?;
        let seconds = t.elapsed().as_secs_f64();
        let cpu_seconds = cpu() - start_cpu;
        times.push(json!({"export":export,"setup_seconds":setup,"archive_seconds":archive_seconds,"normalize_seconds":seconds,"normalize_cpu_seconds":cpu_seconds,"average_cpu_cores":cpu_seconds/seconds}));
    }
    let t = Instant::now();
    let mut hash = Sha256::new();
    let mut counts = BTreeMap::<String, u64>::new();
    sink.visit(&mut |r| {
        *counts.entry(r.family().into()).or_default() += 1;
        let key = format!("{}:{}", r.family(), r.id());
        let value = serde_json::to_vec(&r)?;
        hash.update((key.len() as u64).to_le_bytes());
        hash.update(key.as_bytes());
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
        Ok(())
    })?;
    let fingerprint = format!("{:x}", hash.finalize());
    let source_fingerprint = digest(&serde_json::to_vec(&(
        &seed.sources,
        &seed.provenance_records,
    ))?);
    let normalize_seconds: f64 = times
        .iter()
        .map(|v| v["normalize_seconds"].as_f64().unwrap())
        .sum();
    let normalize_cpu_seconds: f64 = times
        .iter()
        .map(|v| v["normalize_cpu_seconds"].as_f64().unwrap())
        .sum();
    Ok(
        json!({"project":case.project,"language":case.language,"exports":case.exports,"unique_paths":paths.len(),"file_frames":frames,"timings":times,"normalize_seconds":normalize_seconds,"normalize_cpu_seconds":normalize_cpu_seconds,"average_cpu_cores":normalize_cpu_seconds/normalize_seconds,"peak_active_tasks_sampled":sink.active.load(Ordering::Relaxed),"peak_planning_tasks_sampled":sink.planning.load(Ordering::Relaxed),"verification_seconds":t.elapsed().as_secs_f64(),"record_counts":counts,"record_fingerprint":fingerprint,"source_fingerprint":source_fingerprint,"journal":sink.journal.stats(),"workers_requested":std::env::var("CQ_NORMALIZE_WORKERS").ok(),"scope":"normalization of retained compiler exports; unit mappings reconstructed from exported unit tags; no compiler capture, canonicalization or publication"}),
    )
}
fn main() {
    match run() {
        Ok(v) => println!("{}", v),
        Err(e) => {
            eprintln!("{}", json!({"error":e.to_string()}));
            std::process::exit(1)
        }
    }
}
