use crate::report::Reporter;
use cq_capture_protocol::Event;
use std::{
    ffi::OsString,
    process::{Command, ExitStatus, Stdio},
};

pub fn run(
    command: &OsString,
    args: &[OsString],
    removed: &[OsString],
    reporter: &mut Reporter,
) -> std::io::Result<ExitStatus> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for key in removed {
        cmd.env_remove(key);
    }
    let mut child = cmd.spawn()?;
    reporter.send(Event::ProcessStarted { pid: child.id() });
    loop {
        match child.wait() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}
pub fn exit_like(status: ExitStatus) -> ! {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        // Restore default handling so Cargo observes the same terminating signal.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        std::process::exit(128 + signal)
    }
    std::process::exit(status.code().unwrap_or(1))
}
