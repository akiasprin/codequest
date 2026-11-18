mod cli;
mod inspect;
mod phase1;
use cq_core::{
    Result,
    analyst::{BuildObserver, rust::RustBuildObserver},
    control::CaptureControl,
    ports::CaptureReportRepository,
};
use cq_store::{LocalStore, journal::LocalCapture};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}
fn main() {
    let code = match run() {
        Ok((value, code)) => {
            if !value.is_null() {
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
            }
            code
        }
        Err(e) => {
            eprintln!("{}", serde_json::json!({"error":e.to_string()}));
            2
        }
    };
    std::process::exit(code)
}
fn run() -> Result<(serde_json::Value, i32)> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|s| s != "capture") {
        return phase1::run(args);
    }
    match cli::parse(args)? {
        cli::Command::Run(request) => {
            let observer = RustBuildObserver;
            let plan = observer.plan(request)?;
            let store = LocalStore::open(&plan.output.path())?;
            let journal = Arc::new(LocalCapture::open(
                store.run_path(&plan.run_id)?,
                &plan.run_id,
                plan.budget.clone(),
            )?);
            let control = CaptureControl::default();
            let watcher_control = control.clone();
            let done = Arc::new(AtomicBool::new(false));
            let watcher_done = done.clone();
            let previous =
                unsafe { libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t) };
            let watcher = std::thread::spawn(move || {
                while !watcher_done.load(Ordering::Relaxed) {
                    if INTERRUPTED.load(Ordering::Relaxed) {
                        watcher_control.cancel();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            let result = observer.observe(plan, journal, &store.content, &control);
            done.store(true, Ordering::Relaxed);
            let _ = watcher.join();
            unsafe {
                libc::signal(libc::SIGINT, previous);
            }
            let report = result?;
            let digest = store.save_report(&report)?;
            let code = report.exit_code();
            Ok((
                serde_json::json!({"run_id":report.run.id,"build_outcome":report.run.outcome,"tool_exit":report.run.tool_exit,"stop_reason":report.run.stop_reason,"seal":report.seal,"job_accounting":report.job_accounting,"invocations":report.invocations.len(),"units":report.units.len(),"artifacts":report.artifacts.len(),"dependencies":report.dependencies.len(),"fixed_files":report.snapshot.files.len(),"input_binding":report.snapshot.binding,"report_digest":digest,"report_path":store.run_path(&report.run.id)?.join("report.json"),"duration_ms":report.duration_ms,"exit_code":code}),
                code,
            ))
        }
        cli::Command::Inspect {
            output,
            run,
            section,
            limit,
            cursor,
        } => {
            let store = LocalStore::open(&output)?;
            Ok((
                inspect::inspect(&store, &run, &section, limit, cursor.as_deref())?,
                0,
            ))
        }
    }
}
