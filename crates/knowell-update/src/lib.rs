//! Authenticated releases and recoverable, explicitly owned local installations.
//!
//! Release checks never open an engine, database, or provider. Runtime admission
//! and update transactions use operating-system locks rather than stale PID files.

mod error;
pub mod install;
pub mod manifest;
pub mod publisher;
pub mod repository;

pub use error::{Error, Result};
