//! Integration tests for knowell-store against PostgreSQL with and without
//! pgvector. See `common.rs` and the crate README for how to run them.

// Test helpers outside `#[test]` functions may panic and print.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

mod common;

mod audit;
mod connect;
mod content;
mod embeddings;
mod graph;
mod hierarchy;
mod identity;
mod jobs;
mod knowledge;
mod migrations;
mod switches;
mod tasks;
mod usage;
mod views;
