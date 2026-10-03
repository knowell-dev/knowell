//! Engine facade: one object that owns the store, the indexer and the
//! search sources, and implements both the agent-facing MCP tools and the
//! REST API's engine operations on top of them.

mod access;
mod engine;
mod error;
mod eval;
mod evidence;
mod graph;
mod ids;
mod memory;
mod patch;
mod rest;
mod scope;
mod search;
mod settings;
mod snapshot;
mod tools;
mod usage;

pub use access::{Access, AccessResolver, StaticAccess, StoreAccess};
pub use engine::{Engine, EngineBuilder};
pub use error::EngineError;
pub use eval::{HYBRID_RETRIEVER, HybridRetriever};
pub use memory::{
    BoxFuture, CheckpointRow, InMemoryMemory, MemoryError, MemoryRepo, RecordQuery, RecordRow,
    StoreMemory, TaskRow,
};
pub use settings::{DomainConfig, EngineSettings, RelationStageInfo};
