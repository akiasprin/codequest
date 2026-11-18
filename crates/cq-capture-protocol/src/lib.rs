//! Small, language-neutral process-observation wire contract.
pub mod event;
pub mod native;
pub mod wire;
pub use event::*;
pub use native::NativeValue;

use std::io::Read;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
pub const ENDPOINT_ENV: &str = "CQ_CAPTURE_ENDPOINT";
pub const RUN_ENV: &str = "CQ_CAPTURE_RUN";

pub fn new_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
