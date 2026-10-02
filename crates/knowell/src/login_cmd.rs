//! `know login <hub-url>`: makes this machine an `edge` of a hub.
//!
//! The engine configuration stores the hub URL and the token *reference*;
//! the token itself is resolved only to send it to the hub, never written
//! or printed.

use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Args;
use knowell_config::{HubConfig, ServerRole};
use knowell_core::SecretRef;
use secrecy::{ExposeSecret, SecretString};

use crate::db;
use crate::engine_file;
use crate::env::Env;
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct LoginArgs {
    /// Base URL of the hub, e.g. https://knowell.example.com.
    hub_url: String,
    /// Reference to the access token issued by the hub: `env:NAME` or
    /// `file:/path`. Never the token itself.
    #[arg(long, value_name = "REF", default_value = "env:KNOWELL_HUB_TOKEN")]
    token_ref: String,
    /// Save the settings without contacting the hub.
    #[arg(long)]
    skip_verify: bool,
}

pub(crate) fn run(args: LoginArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let url = normalize_hub_url(&args.hub_url)?;
    // SecretRefError never echoes the text (a pasted token must not leak).
    let token_ref =
        SecretRef::from_str(&args.token_ref).map_err(|e| anyhow::anyhow!("--token-ref: {e}"))?;

    let mut cfg = match env.load_engine() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => knowell_config::parse_engine("version = 1")?,
        Err(err) => return Err(anyhow::Error::new(err)),
    };
    if cfg.server.role == ServerRole::Hub {
        bail!(
            "{} is configured as a hub; a hub does not log in to another hub",
            env.engine_config.display()
        );
    }

    if !args.skip_verify {
        let token = match knowell_secrets::resolve(&token_ref) {
            Ok(token) => token,
            Err(err) => {
                tracing::error!(
                    "{err}; set it to a token issued by the hub (or pass --skip-verify)"
                );
                return Ok(ExitCode::FAILURE);
            }
        };
        let rt = db::runtime()?;
        match rt.block_on(verify(&url, &token))? {
            Ok(summary) => out.line(format!("hub {url}: {summary}"))?,
            Err(problem) => {
                tracing::error!("hub {url}: {problem}; nothing was saved");
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    cfg.server.role = ServerRole::Edge;
    cfg.hub = Some(HubConfig {
        url: url.clone(),
        token: token_ref.clone(),
    });
    engine_file::write(&env.engine_config, &cfg)?;
    out.line(format!(
        "saved {}: role edge, hub {url}, token {}",
        env.engine_config.display(),
        token_ref.describe()
    ))?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// `http(s)://host[:port][/path]` without user info, query or fragment,
/// without a trailing slash.
fn normalize_hub_url(url: &str) -> anyhow::Result<String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    let Some(rest) = rest else {
        bail!("the hub url must start with https:// (or http:// for a local hub)");
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || url.contains(['?', '#', ' '])
        || url.chars().any(char::is_control)
    {
        // Never echo: user info may carry a password.
        bail!("the hub url must look like https://host[:port] (no user info, query or fragment)");
    }
    Ok(url.trim_end_matches('/').to_owned())
}

/// `GET /api/v1/health` with the token. The inner `Err` explains a refusal.
async fn verify(url: &str, token: &SecretString) -> anyhow::Result<Result<String, String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("know/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot create the HTTP client")?;
    let response = match client
        .get(format!("{url}/api/v1/health"))
        .bearer_auth(token.expose_secret())
        .send()
        .await
    {
        Ok(response) => response,
        // reqwest errors name the URL (no credentials in it), never headers.
        Err(err) => return Ok(Err(format!("cannot reach the hub: {err}"))),
    };
    let status = response.status();
    let bytes = response.bytes().await.unwrap_or_default();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        let health = body
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let version = body
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Ok(Ok(format!(
            "token accepted, health {health}, version {version}"
        )));
    }
    // Problem codes are fixed identifiers (`invalid_token`, …), safe to show.
    let code = body
        .get("code")
        .and_then(serde_json::Value::as_str)
        .filter(|c| c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'))
        .unwrap_or("");
    Ok(Err(match status.as_u16() {
        401 => format!("the hub rejected the token ({code})"),
        403 => format!("the token may not read the hub's health ({code})"),
        _ => format!("the hub answered {status} {code}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_urls() {
        assert_eq!(
            normalize_hub_url("https://kn.example/").unwrap(),
            "https://kn.example"
        );
        assert_eq!(
            normalize_hub_url("http://127.0.0.1:7420").unwrap(),
            "http://127.0.0.1:7420"
        );
        for bad in [
            "kn.example",
            "ftp://kn.example",
            "https://",
            "https://user:KNOWELL_CANARY_pw@kn.example",
            "https://kn.example?x=1",
        ] {
            let err = normalize_hub_url(bad).unwrap_err().to_string();
            assert!(!err.contains("KNOWELL_CANARY"), "{err}");
        }
    }
}
