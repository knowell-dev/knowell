//! Knowell configuration: engine and workspace TOML files, validation,
//! workspace → project inheritance with provenance, and JSON Schema output.
//!
//! Two files exist:
//!
//! - the engine config (`~/.knowell/config.toml`) → [`EngineConfig`], loaded
//!   with [`load_engine`] / [`parse_engine`];
//! - the workspace config (`knowell.toml`) → [`WorkspaceConfig`], loaded with
//!   [`load_workspace`] / [`parse_workspace`], then resolved with
//!   [`WorkspaceConfig::resolve`] and cross-checked with
//!   [`ResolvedWorkspace::check_against`].
//!
//! Secrets are never stored: configuration holds references such as
//! `env:GEMINI_API_KEY`, and no error produced here quotes a value from the
//! file (see [`ConfigError`]).

mod engine;
mod error;
mod resolve;
mod schema;
mod workspace;

use std::path::Path;

use serde::de::DeserializeOwned;

pub use engine::{
    DatabaseConfig, DatabaseMode, EngineConfig, HubConfig, OLLAMA_DEFAULT_BASE_URL, ProviderConfig,
    ProviderKind, ServerConfig, ServerRole, TelemetryConfig,
};
pub use error::{ConfigError, ConfigIssue, ConfigIssues};
pub use resolve::{Origin, ResolvedEmbedding, ResolvedProject, ResolvedWorkspace, Sourced};
pub use schema::{engine_schema, workspace_schema};
pub use workspace::{
    DataPolicy, EmbeddingConfig, EmbeddingPreset, MAX_DIMENSIONS, MIN_DIMENSIONS, ProjectConfig,
    WorkspaceConfig, WorkspaceSection,
};

/// File name used in messages when parsing from memory.
const IN_MEMORY: &str = "<config>";

/// Parses and validates engine configuration text.
pub fn parse_engine(text: &str) -> Result<EngineConfig, ConfigError> {
    parse_engine_named(text, IN_MEMORY)
}

/// Parses and validates workspace configuration text.
pub fn parse_workspace(text: &str) -> Result<WorkspaceConfig, ConfigError> {
    parse_workspace_named(text, IN_MEMORY)
}

/// Reads, parses and validates the engine config at `path`.
pub fn load_engine(path: &Path) -> Result<EngineConfig, ConfigError> {
    let text = read(path)?;
    parse_engine_named(&text, &path.display().to_string())
}

/// Reads, parses and validates the workspace config at `path`.
pub fn load_workspace(path: &Path) -> Result<WorkspaceConfig, ConfigError> {
    let text = read(path)?;
    parse_workspace_named(&text, &path.display().to_string())
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn parse_engine_named(text: &str, file: &str) -> Result<EngineConfig, ConfigError> {
    let cfg: EngineConfig = deserialize(text, file)?;
    finish(file, cfg.validate()).map(|()| cfg)
}

fn parse_workspace_named(text: &str, file: &str) -> Result<WorkspaceConfig, ConfigError> {
    let cfg: WorkspaceConfig = deserialize(text, file)?;
    finish(file, cfg.validate()).map(|()| cfg)
}

/// Never renders the `toml` error itself: its `Display` quotes source text.
fn deserialize<T: DeserializeOwned>(text: &str, file: &str) -> Result<T, ConfigError> {
    toml::from_str(text).map_err(|err| ConfigError::from_toml(file, text, &err))
}

fn finish(file: &str, issues: Vec<ConfigIssue>) -> Result<(), ConfigError> {
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            file: file.to_owned(),
            issues: ConfigIssues(issues),
        })
    }
}

#[cfg(test)]
mod tests;
