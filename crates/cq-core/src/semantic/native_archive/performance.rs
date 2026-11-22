//! Opt-in checks against retained compiler exports. No project paths or large
//! provider payloads are embedded in the repository.
use super::*;
use crate::semantic::native_batch::Catalogue;
use sha2::{Digest, Sha256};
use std::time::Instant;

fn hash(value: &Value) -> String {
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).unwrap();
    format!("{:x}", writer.0.finalize())
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the entire structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

#[test]
#[ignore = "requires CQ_NATIVE_BENCH_EXPORT and CQ_NATIVE_BENCH_REPORT; run without a concurrent full index"]
fn retained_native_frames_preserve_exact_bytes_and_measure_read_cost() {
    let path = std::env::var("CQ_NATIVE_BENCH_EXPORT").expect("retained native export path");
    let report = std::env::var("CQ_NATIVE_BENCH_REPORT").expect("report path");
    let samples: usize = std::env::var("CQ_NATIVE_BENCH_FILES")
        .unwrap_or_else(|_| "32".into())
        .parse()
        .unwrap();
    assert!(samples > 0);
    let original: Value =
        serde_json::from_reader(BufReader::new(std::fs::File::open(&path).unwrap())).unwrap();
    super::super::provider_contract::validate(&original).unwrap();
    let open_start = Instant::now();
    let archive = Archive::open(Path::new(&path)).unwrap();
    let open_seconds = open_start.elapsed().as_secs_f64();
    super::super::provider_contract::validate_metadata(&archive.metadata).unwrap();
    archive.validate_ranges().unwrap();
    let original_catalogue = Catalogue::new(&original).unwrap();
    let catalogue_start = Instant::now();
    let catalogue = Catalogue::new(&archive.metadata).unwrap();
    let catalogue_seconds = catalogue_start.elapsed().as_secs_f64();
    let files = original["files"].as_array().unwrap();
    let mut selected = (0..files.len().min(samples))
        .map(|i| i * files.len() / files.len().min(samples))
        .map(|i| files[i]["id"].as_u64())
        .collect::<Vec<_>>();
    selected.push(None);
    let mut legacy_wall = 0.0;
    let mut legacy_cpu = 0.0;
    let mut optimized_wall = 0.0;
    let mut optimized_cpu = 0.0;
    let mut total_rows = 0;
    let mut total_reads = 0;
    let mut total_bytes = 0;
    let mut proofs = Vec::new();
    for (ordinal, file) in selected.iter().enumerate() {
        let expected = hash(&original_catalogue.frame(*file).unwrap());
        let plan = catalogue.plan(*file).unwrap();
        let mut legacy = || {
            let cpu = cpu_seconds();
            let now = Instant::now();
            let frame = catalogue
                .render_with(&plan, |row| archive.hydrate_row_ref(row))
                .unwrap();
            legacy_wall += now.elapsed().as_secs_f64();
            legacy_cpu += cpu_seconds() - cpu;
            assert_eq!(hash(&frame), expected, "legacy frame {file:?}");
        };
        let mut optimized = || {
            let mut reader = RowReader::new(&archive);
            let cpu = cpu_seconds();
            let now = Instant::now();
            let frame = catalogue
                .render_with(&plan, |row| reader.hydrate(row))
                .unwrap();
            optimized_wall += now.elapsed().as_secs_f64();
            optimized_cpu += cpu_seconds() - cpu;
            total_rows += reader.rows;
            total_reads += reader.reads;
            total_bytes += reader.bytes;
            assert_eq!(hash(&frame), expected, "read-ahead frame {file:?}");
        };
        // Alternate order so one implementation is not always the cache warmer.
        if ordinal % 2 == 0 {
            legacy();
            optimized();
        } else {
            optimized();
            legacy();
        }
        proofs.push(json!({"file":file,"sha256":expected}));
    }
    let output = json!({"export":path,"export_bytes":std::fs::metadata(&path).unwrap().len(),
        "archive_open_seconds":open_seconds,"catalogue_seconds":catalogue_seconds,"sampled_frames":selected.len(),
        "legacy":{"wall_seconds":legacy_wall,"cpu_seconds":legacy_cpu,"average_cpu_cores":legacy_cpu/legacy_wall},
        "read_ahead":{"wall_seconds":optimized_wall,"cpu_seconds":optimized_cpu,"average_cpu_cores":optimized_cpu/optimized_wall,
            "rows":total_rows,"pread_calls":total_reads,"read_bytes":total_bytes},
        "scope":"frame materialization only; includes all native rows in each selected closure; excludes normalization/publication",
        "all_frame_bytes_equal":true,"proofs":proofs});
    std::fs::write(report, serde_json::to_vec_pretty(&output).unwrap()).unwrap();
}

#[derive(Default, serde::Serialize)]
struct Cost {
    wall_seconds: f64,
    cpu_seconds: f64,
}
impl Cost {
    fn measure<T>(&mut self, action: impl FnOnce() -> T) -> T {
        let cpu = cpu_seconds();
        let now = Instant::now();
        let result = action();
        self.wall_seconds += now.elapsed().as_secs_f64();
        self.cpu_seconds += cpu_seconds() - cpu;
        result
    }
}
#[test]
#[ignore = "requires CQ_BUILD_BENCH_CAPTURE and CQ_BUILD_BENCH_REPORT; run after the full index"]
fn retained_build_events_measure_fragment_copy_and_staging_cost() {
    use crate::semantic::native_batch::{FrameMetadata, FrameOutput, FrameSources};
    let path = std::env::var("CQ_BUILD_BENCH_CAPTURE").expect("retained CaptureReport path");
    let output = std::env::var("CQ_BUILD_BENCH_REPORT").expect("benchmark report path");
    let capture: CaptureReport =
        serde_json::from_reader(BufReader::new(std::fs::File::open(&path).unwrap())).unwrap();
    let mut project =
        SemanticSnapshot::empty("build-copy-benchmark".into(), capture.plan.project.clone());
    let events = capture
        .invocations
        .iter()
        .flat_map(|i| {
            i.started
                .iter()
                .chain(i.process.iter())
                .chain(i.finished.iter())
                .chain(i.argument_files.iter())
                .cloned()
        })
        .collect::<Vec<_>>();
    project.builds.push(BuildObservation {
        id: capture.run.id.clone(),
        language: Language::Rust,
        command: capture.plan.build_tool.clone(),
        arguments: capture
            .plan
            .effective_args
            .iter()
            .map(|s| NativeValue::from(s.as_str()))
            .collect(),
        cwd: capture.plan.project.clone(),
        outcome: capture.run.outcome.clone(),
        exit: capture.run.tool_exit.clone(),
        events: vec![],
        rejected_fields: vec![],
        captured_report: Some(capture.run.id.clone()),
        gaps: vec![],
    });
    let mut prefix_bytes = 2;
    let prefix = events
        .iter()
        .take_while(|event| {
            prefix_bytes += serde_json::to_vec(event).unwrap().len() + 1;
            prefix_bytes <= 7 * 1024 * 1024
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut reports = Vec::new();
    for (label, collection) in [
        ("captured_before_limit_rejection", events),
        ("admissible_prefix_of_captured_events", prefix),
        ("current_rejected_collection", vec![]),
    ] {
        project.builds[0].events = collection;
        let events_bytes = serde_json::to_vec(&project.builds[0].events).unwrap().len();
        let sources = FrameSources::new(&project, &[]);
        let frame = json!({"files":[]});
        let mut legacy_clone = Cost::default();
        let mut legacy_write = Cost::default();
        let mut legacy_read = Cost::default();
        let mut revised_clone = Cost::default();
        let mut revised_write = Cost::default();
        let mut revised_read = Cost::default();
        let mut legacy_bytes = 0;
        let mut revised_bytes = 0;
        for _ in 0..8 {
            let legacy = legacy_clone.measure(|| {
                let (mut fragment, _) = sources.fragment(&project, &frame);
                // Reconstruct the former per-file clone and the rejected
                // whole-fragment staging prototype, without changing product
                // limits or publishing this oversized diagnostic fixture.
                fragment.builds.extend(project.builds.last().cloned());
                fragment.providers = project.providers.clone();
                fragment.provenance_records = project.provenance_records.clone();
                fragment
            });
            let old_file = legacy_write.measure(|| {
                let file = tempfile::NamedTempFile::new().unwrap();
                let mut writer = BufWriter::new(file.as_file());
                serde_json::to_writer(&mut writer, &legacy).unwrap();
                writer.flush().unwrap();
                drop(writer);
                file
            });
            legacy_bytes += old_file.as_file().metadata().unwrap().len();
            drop(legacy);
            let old: SemanticSnapshot = legacy_read.measure(|| {
                serde_json::from_reader(BufReader::new(old_file.reopen().unwrap())).unwrap()
            });
            assert_eq!(old.builds[0].events.len(), project.builds[0].events.len());
            let expected = FrameOutput {
                fragment: old,
                peak: 0,
            }
            .into_metadata()
            .unwrap();
            let revised = revised_clone.measure(|| {
                let (fragment, _) = sources.fragment(&project, &frame);
                assert!(fragment.builds.is_empty());
                FrameOutput { fragment, peak: 0 }.into_metadata().unwrap()
            });
            let new_file = revised_write.measure(|| {
                let file = tempfile::NamedTempFile::new().unwrap();
                let mut writer = BufWriter::new(file.as_file());
                serde_json::to_writer(&mut writer, &revised).unwrap();
                writer.flush().unwrap();
                drop(writer);
                file
            });
            revised_bytes += new_file.as_file().metadata().unwrap().len();
            let actual: FrameMetadata = revised_read.measure(|| {
                serde_json::from_reader(BufReader::new(new_file.reopen().unwrap())).unwrap()
            });
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        reports.push(json!({"case":label,"sampled_files":8,"events":project.builds[0].events.len(),"events_json_bytes":events_bytes,
            "legacy_reconstruction":{"clone":legacy_clone,"write":legacy_write,"read":legacy_read,"temporary_bytes":legacy_bytes},
            "revised":{"clone":revised_clone,"write":revised_write,"read":revised_read,"temporary_bytes":revised_bytes},
            "metadata_equal":true}));
    }
    std::fs::write(output, serde_json::to_vec_pretty(&json!({"capture":path,
        "scope":"isolated fragment copying and staging; old staging is an unshipped prototype; timings exclude capture parsing, graph normalization and publication",
        "cases":reports})).unwrap()).unwrap();
}
