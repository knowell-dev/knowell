//! Integration tests for knowell-server, driven through the router with
//! `tower::ServiceExt::oneshot` (and over TCP for serving and shutdown).
//! Store-backed tests need `KNOWELL_TEST_DATABASE_URL` (see
//! `crates/knowell-store/README.md`) and skip without it.

// Test helpers outside `#[test]` functions may panic and print.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

mod common;

mod auth;
mod engine;
mod identity;
mod mcp;
mod middleware;
mod panel;
mod serve;
mod session;
mod sse;
mod store;
mod webhooks;
