//! Immutable, bounded content and copy-on-write manifest maps (format 2).
pub mod limits;
pub mod objects;
pub mod source;
pub mod tree;
pub mod writer;

mod bulk;
mod descriptor_budget;
pub(crate) mod inventory;
mod packed;
mod statistics;
