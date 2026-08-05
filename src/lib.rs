#![forbid(unsafe_code)]

pub mod api;
pub mod batch;
pub mod cancel;
pub mod error;
pub mod integrity;
pub mod local;
pub mod observability;
pub mod path;
pub mod plan;
pub mod progress;
pub mod source_diagnostics;
pub mod sync;
pub mod vault;

pub use error::{Error, Result};
