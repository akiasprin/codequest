//! R0 experiments only. Run against a fresh output directory, never a product store.
mod cases;
mod host;
mod model;
mod projection;
mod regressions;
use model::*;
use serde_json::json;
use std::{path::PathBuf, time::Instant};

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("--worker") {
        return cases::worker(
            args.get(1).ok_or("missing mode")?,
            std::path::Path::new(args.get(2).ok_or("missing root")?),
            args.get(3).ok_or("missing stage")?,
        );
    }
    if args.first().map(String::as_str) == Some("--ownership-worker") {
        return require(
            lbug::Database::new(args.get(1).ok_or("missing path")?, projection::config()).is_err(),
            "native second process owner allowed",
        );
    }
    require(
        (args.len() == 2 || (args.len() == 4 && args[2] == "--case")) && args[0] == "--output",
        "usage: cargo run -p cq-store --example store_r0 -- --output FRESH_DIRECTORY [--case NAME]",
    )?;
    let output = PathBuf::from(&args[1]);
    require(!output.exists(), "output directory must not exist")?;
    std::fs::create_dir_all(&output)?;
    type CaseFn = Box<dyn Fn(&std::path::Path) -> Result<serde_json::Value>>;
    let mut tests: Vec<(&str, &str, CaseFn)> = vec![
        (
            "multiversion_incremental_aba",
            "C03",
            Box::new(cases::multiversion),
        ),
        (
            "deletion_and_dangling_rollback",
            "C03/C09",
            Box::new(cases::deletion_and_rollback),
        ),
        (
            "concurrent_publish_cas",
            "C04",
            Box::new(cases::concurrent_writers),
        ),
        (
            "epoch_catchup_and_old_reader",
            "C07",
            Box::new(cases::epoch),
        ),
        ("active_lease_expiry_and_gc", "C06", Box::new(cases::leases)),
        (
            "staging_root_invalidates_gc",
            "C06",
            Box::new(cases::gc_roots),
        ),
        ("admission_gc_race", "C06", Box::new(cases::admission_race)),
        (
            "native_ownership_timeout_interrupt",
            "C10/L08",
            Box::new(cases::ownership_and_cancel),
        ),
        (
            "reader_transaction_during_publish",
            "C05",
            Box::new(cases::mvcc_reader),
        ),
        (
            "high_fanout_keyset_pages",
            "L04/L08",
            Box::new(cases::pagination),
        ),
        (
            "evidence_integrity",
            "C09",
            Box::new(cases::source_integrity),
        ),
    ];
    for stage in [
        "after-files",
        "before-db-commit",
        "after-db-commit",
        "after-head-rename",
        "after-head",
    ] {
        tests.push((
            stage,
            "C05",
            Box::new(move |p| cases::crash(p, stage, false)),
        ));
    }
    tests.push((
        "recovery_killed_before_commit",
        "C04/C05",
        Box::new(|p| cases::crash(p, "after-db-commit", true)),
    ));
    for stage in ["before-route", "after-route"] {
        tests.push((
            stage,
            "C07",
            Box::new(move |p| cases::epoch_crash(p, stage)),
        ));
    }
    tests.extend([
        (
            "sealed_dependencies_regression",
            "F2",
            Box::new(regressions::sealed_dependencies) as CaseFn,
        ),
        (
            "immutable_view_regression",
            "F1",
            Box::new(regressions::immutable_view) as CaseFn,
        ),
        (
            "adjacency_integrity_regression",
            "F3",
            Box::new(regressions::adjacency_integrity) as CaseFn,
        ),
        (
            "relation_identity_regression",
            "F4",
            Box::new(regressions::relation_identity) as CaseFn,
        ),
        (
            "retained_rebuild_regression",
            "F5",
            Box::new(regressions::retained_rebuild) as CaseFn,
        ),
        (
            "oracle_mutation_regression",
            "F7",
            Box::new(regressions::oracle_guards) as CaseFn,
        ),
    ]);
    if args
        .get(3)
        .is_some_and(|name| name.starts_with("commit_fault_"))
    {
        let name = match args[3].as_str() {
            "commit_fault_directory_once" => "commit_fault_directory_once",
            "commit_fault_directory_repeat" => "commit_fault_directory_repeat",
            "commit_fault_head_file" => "commit_fault_head_file",
            _ => return Err("unknown fault case".into()),
        };
        tests.push((name, "F6", Box::new(regressions::commit_fault)));
    }
    if let Some(selected) = args.get(3) {
        tests.retain(|(name, _, _)| name == selected);
        require(!tests.is_empty(), "unknown case")?;
    }
    let started = Instant::now();
    let mut results = vec![];
    for (name, contract, run) in tests {
        println!("START {name}");
        let t = Instant::now();
        let value =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&output.join(name))));
        let record = match value {
            Ok(Ok(evidence)) => {
                json!({"case":name,"contract":contract,"status":"passed","seconds":t.elapsed().as_secs_f64(),"evidence":evidence})
            }
            Ok(Err(e)) => {
                json!({"case":name,"contract":contract,"status":"failed","seconds":t.elapsed().as_secs_f64(),"error":e.to_string()})
            }
            Err(e) => {
                json!({"case":name,"contract":contract,"status":"failed","seconds":t.elapsed().as_secs_f64(),"error":e.downcast_ref::<String>().cloned().or_else(||e.downcast_ref::<&str>().map(|s|s.to_string())).unwrap_or_else(||"panic".into())})
            }
        };
        println!("{} {name}", record["status"]);
        results.push(record);
        save(&output.join("cases.json"), &results)?;
    }
    let passed = results.iter().filter(|r| r["status"] == "passed").count();
    let result = json!({"schema":1,"stage":"R0 isolated prototype","engine":"Ladybug","lbug_crate":"0.22.3","native_version":lbug::VERSION,"native_source":lbug::get_library_source(),"buffer_pool_bytes":512*1024*1024u64,"threads":4,"passed":passed,"total":results.len(),"seconds":started.elapsed().as_secs_f64(),"cases":results,"limitations":["synthetic normalized facts, no compiler capture in this run","not production LocalStore, FactReader or REPL acceptance","GC is small-catalog prototype with logical clock, not production retention","endpoint validation still scans visible facts","keyset correctness does not prove bounded engine scanning","SIGKILL and I/O ordering are not power-loss durability proof","not a real multi-process IPC host or multi-project resource benchmark"]});
    save(&output.join("results.json"), &result)?;
    println!(
        "RESULT {passed}/{} {}",
        result["total"],
        output.join("results.json").display()
    );
    require(
        passed == result["total"].as_u64().ok_or("bad total")? as usize,
        "R0 contains failed cases",
    )
}
