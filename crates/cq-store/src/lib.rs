//! Filesystem persistence for capture records and immutable source content.
mod atomic;
pub mod content;
pub mod facts;
pub mod journal;
pub mod ladybug;
pub mod paged;
pub mod reports;
pub mod revisions;
pub mod runtime;
pub use reports::LocalStore;
pub mod graph;
pub mod materialize;

pub mod analysis;
pub mod checkpoint;
mod disk_batch;
pub mod execution;
pub mod raw;
thread_local! {static SNAPSHOT_DECODES:std::cell::Cell<u64>=const{std::cell::Cell::new(0)};}
pub fn snapshot_decode_count() -> u64 {
    SNAPSHOT_DECODES.with(std::cell::Cell::get)
}
pub(crate) fn record_snapshot_decode() {
    SNAPSHOT_DECODES.with(|n| n.set(n.get() + 1));
}
