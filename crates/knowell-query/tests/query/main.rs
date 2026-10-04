#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Query engine tests against in-memory fake sources, graph and snippets.
//! Every fixture is synthetic and built in the test.

mod common;
mod degraded;
mod expansion;
mod fusion;
mod packing;
mod planner;
mod source_packing;
mod task_packing;
