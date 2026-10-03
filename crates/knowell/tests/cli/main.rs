//! End-to-end tests of the `know` binary: every test runs the real
//! executable in a temporary sandbox (`KNOWELL_HOME`, `HOME` and
//! `USERPROFILE` point into it), so the user's own configuration is never
//! read or written.
//!
//! Tests that need PostgreSQL use `KNOWELL_TEST_DATABASE_URL` (an admin URL,
//! see `crates/knowell-store/README.md`) and print one skip line without it.
//! The managed-PostgreSQL test downloads PostgreSQL and is `#[ignore]`d.

// Test helpers outside `#[test]` functions may panic and print.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

mod common;

mod check;
mod ci;
mod connect;
mod context;
mod database;
mod doctor;
mod eval;
mod graph;
mod local;
mod login;
mod mcp;
mod profiles;
mod project;
mod records;
mod serve;
mod token;
mod workspace;
