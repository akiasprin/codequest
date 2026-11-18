//! Shared build-observation models and Rust build observer. No storage implementation.
pub mod analyst;
pub mod compute;
pub mod control;
pub mod error;
pub mod input;
pub mod model;
pub mod ports;
pub use error::{Error, Result};

pub mod documents;
pub mod query;
pub mod semantic;
pub mod validation;

pub mod process;
pub mod progress;

pub mod source_tree;
