use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::Instant,
};

/// A deadline spans the whole frame, including partial reads and writes.
/// Per-syscall socket timeouts alone would allow a slowly trickling ACK to
/// extend the wait indefinitely.
pub struct DeadlineIo<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl<'a> DeadlineIo<'a> {
    pub fn new(stream: &'a mut UnixStream, deadline: Instant) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self { stream, deadline })
    }
    fn remaining(&self) -> io::Result<i32> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "frame deadline"));
        }
        Ok(left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32)
    }
    fn wait(&self, event: i16) -> io::Result<()> {
        loop {
            let mut fd = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: event,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut fd, 1, self.remaining()?) };
            if n > 0 {
                return Ok(());
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "frame deadline"));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}
impl Read for DeadlineIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.remaining()?;
            match self.stream.read(bytes) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(libc::POLLIN)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}
impl Write for DeadlineIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.remaining()?;
            match self.stream.write(bytes) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(libc::POLLOUT)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
