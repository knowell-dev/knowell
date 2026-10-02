//! Sandboxed plugin host: runs third-party analyzer plugins, compiled to
//! WebAssembly components, with explicit capabilities, resource limits and a
//! versioned interface (`knowell:plugin@0.1.0`, see [`WIT`]).
//!
//! Plugins are untrusted code. The host:
//!
//! - refuses a plugin whose component SHA-256 differs from its [`Manifest`],
//!   whose API version is incompatible, or that requests a capability the user
//!   did not grant ([`Grants`]);
//! - runs every call in a fresh instance with fuel, memory, table and
//!   wall-clock limits ([`HostConfig`]);
//! - gives plugins a WASI context that grants nothing (no files, network,
//!   environment or real clocks); file access exists only as the read-only,
//!   policy-filtered `project-files` capability;
//! - treats plugin output as untrusted input: sizes are checked before it is
//!   copied, and every key, range and enum value is validated.
//!
//! ```no_run
//! use knowell_core::RepoPath;
//! use knowell_plugin::{Grants, HostConfig, Manifest, PluginHost, PluginSource, SourceFile};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let host = PluginHost::new(HostConfig::default())?;
//! let manifest = Manifest::from_toml(&std::fs::read_to_string("plugin.toml")?)?;
//! let plugin = host.load(
//!     PluginSource::Path("plugin.wasm".as_ref()),
//!     &manifest,
//!     &Grants::none(),
//! )?;
//! let path = RepoPath::new("src/routes.toy")?;
//! let output = plugin.analyze(&SourceFile {
//!     path: &path,
//!     language: "toy",
//!     text: "route GET /users/{id} -> getUser\n",
//! })?;
//! # let _ = output;
//! # Ok(())
//! # }
//! ```

mod config;
mod error;
mod grants;
mod host;
mod manifest;
mod output;
mod plugin;
mod sanitize;
mod state;
mod wire;

pub use config::HostConfig;
pub use error::{DeclineKind, PluginError};
pub use grants::{Grants, PathFilter, ProjectFilesGrant};
pub use host::{CacheStats, PluginHost, PluginSource};
pub use manifest::{API_VERSION, ApiVersion, Capability, Manifest, Sha256Digest, WIT};
pub use output::{
    AnalyzerOutput, Contract, ContractKind, ContractRole, Edge, EdgeKind, Evidence,
    ITEM_OVERHEAD_BYTES, MAX_KEY_BYTES, PluginInfo, Resolution, SourceFile,
};
pub use plugin::Plugin;
