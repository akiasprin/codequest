mod deadline;
mod process;
mod report;
use cq_capture_protocol::{EnvironmentEntry, Event, ExitObservation, NativeValue};
use std::{env, io::Read};

fn main() {
    let mut args = env::args_os().skip(1);
    let Some(command) = args.next() else {
        std::process::exit(127)
    };
    let args: Vec<_> = args.collect();
    let mut reporter = report::Reporter::from_env();
    let removed: Vec<_> = env::vars_os()
        .filter(|(k, _)| k.to_string_lossy().starts_with("CQ_CAPTURE_"))
        .map(|(k, _)| k)
        .collect();
    let extra: Vec<String> = env::var("CQ_CAPTURE_ENV_KEYS")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let environment = env::vars_os()
        .filter(|(k, _)| {
            let s = k.to_string_lossy();
            [
                "RUSTC",
                "RUSTUP_TOOLCHAIN",
                "RUSTFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "CARGO_MANIFEST_DIR",
                "CARGO_CRATE_NAME",
                "OUT_DIR",
            ]
            .contains(&s.as_ref())
                || s.starts_with("CARGO_PKG_")
                || s.starts_with("CARGO_FEATURE_")
                || s.starts_with("CARGO_CFG_")
                || extra.iter().any(|v| v == s.as_ref())
        })
        .map(|(key, value)| EnvironmentEntry {
            key: NativeValue::new(key),
            value: NativeValue::new(value),
        })
        .collect();
    reporter.send(Event::InvocationStarted {
        recorder_pid: std::process::id(),
        command: NativeValue::new(&command),
        args: args.iter().map(NativeValue::new).collect(),
        cwd: NativeValue::new(env::current_dir().unwrap_or_default()),
        environment,
        removed_environment: removed.iter().map(NativeValue::new).collect(),
    });
    let per_file = env_usize("CQ_CAPTURE_ARGUMENT_FILE_BYTES", 1024 * 1024);
    let mut remaining = env_usize("CQ_CAPTURE_ARGUMENT_TOTAL_BYTES", 4 * 1024 * 1024);
    for arg in &args {
        use std::os::unix::ffi::OsStrExt;
        let bytes = arg.as_bytes();
        if bytes.first() != Some(&b'@') {
            continue;
        }
        let path = NativeValue(bytes[1..].to_vec());
        let limit = per_file.min(remaining);
        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut data = Vec::new();
            std::fs::File::open(path.path())?
                .take(limit as u64 + 1)
                .read_to_end(&mut data)?;
            if data.len() > limit {
                return Err(std::io::Error::other("argument file budget exceeded"));
            }
            Ok(data)
        })();
        let (contents, error) = match result {
            Ok(data) => {
                remaining -= data.len();
                (Some(NativeValue(data)), None)
            }
            Err(e) => (None, Some(e.to_string())),
        };
        reporter.send(Event::ArgumentFileObserved {
            path,
            contents,
            error,
        });
    }
    let status = process::run(&command, &args, &removed, &mut reporter);
    match status {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            reporter.finish(ExitObservation {
                code: status.code(),
                signal: status.signal(),
                spawn_error: None,
            });
            process::exit_like(status)
        }
        Err(e) => {
            reporter.finish(ExitObservation {
                code: None,
                signal: None,
                spawn_error: Some(e.to_string()),
            });
            std::process::exit(if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            })
        }
    }
}
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
