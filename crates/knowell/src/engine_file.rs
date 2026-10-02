//! Writing the engine configuration (`$KNOWELL_HOME/config.toml`).
//!
//! The file is generated from [`EngineConfig`] with `toml` and checked by
//! parsing it back, so what is written is exactly what Knowell will read.
//! It only ever holds secret *references* (`env:NAME`, `file:/path`).

use std::path::Path;

use anyhow::{Context, bail};
use knowell_config::EngineConfig;

use crate::fsutil;

const HEADER: &str = "\
#:schema https://raw.githubusercontent.com/knowell-dev/knowell/main/schemas/engine.schema.json
# Knowell engine configuration (machine-level settings). Written by `know`; safe to edit.
# Secrets are never stored here: use references such as \"env:NAME\" or \"file:/path\".
";

/// Renders `cfg` as TOML with a header comment.
pub(crate) fn render(cfg: &EngineConfig) -> anyhow::Result<String> {
    let issues = cfg.validate();
    if !issues.is_empty() {
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        bail!("invalid engine configuration: {}", text.join("; "));
    }
    let body = toml::to_string(cfg).context("cannot render the engine configuration")?;
    let text = format!("{HEADER}{body}");
    // Guard against any rendering mismatch: never write a file that reads
    // back differently.
    match knowell_config::parse_engine(&text) {
        Ok(parsed) if &parsed == cfg => Ok(text),
        Ok(_) => bail!("internal error: the rendered engine configuration does not read back"),
        Err(err) => Err(anyhow::Error::new(err))
            .context("internal error: the rendered engine configuration is invalid"),
    }
}

/// Validates, renders and atomically writes `cfg` to `path`.
pub(crate) fn write(path: &Path, cfg: &EngineConfig) -> anyhow::Result<()> {
    let text = render(cfg)?;
    fsutil::write_atomic(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowell_config::{DatabaseMode, HubConfig, ServerRole};

    #[test]
    fn default_config_round_trips() {
        let cfg = knowell_config::parse_engine("version = 1").unwrap();
        let text = render(&cfg).unwrap();
        assert!(text.starts_with("#:schema"));
        assert_eq!(knowell_config::parse_engine(&text).unwrap(), cfg);
    }

    #[test]
    fn external_and_edge_round_trip_without_values() {
        let mut cfg = knowell_config::parse_engine("version = 1").unwrap();
        cfg.database.mode = DatabaseMode::External;
        cfg.database.url = Some("env:KNOWELL_DATABASE_URL".parse().unwrap());
        cfg.server.role = ServerRole::Edge;
        cfg.hub = Some(HubConfig {
            url: "https://hub.example".into(),
            token: "env:KNOWELL_HUB_TOKEN".parse().unwrap(),
        });
        let text = render(&cfg).unwrap();
        assert!(text.contains("env:KNOWELL_DATABASE_URL"));
        assert!(text.contains("env:KNOWELL_HUB_TOKEN"));
        assert_eq!(knowell_config::parse_engine(&text).unwrap(), cfg);
    }

    #[test]
    fn invalid_config_is_refused() {
        let mut cfg = knowell_config::parse_engine("version = 1").unwrap();
        cfg.server.role = ServerRole::Edge; // edge without [hub]
        assert!(render(&cfg).is_err());
    }
}
