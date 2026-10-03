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
    /// Base URL of the hub, e.g. <https://knowell.example.com>.
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
    let parsed = url::Url::parse(url).map_err(|_| {
        anyhow::anyhow!("the hub url must be an absolute http(s) url without credentials")
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("the hub url must look like https://host[:port] (no user info, query or fragment)");
    }
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if parsed.scheme() == "http" && !loopback {
        bail!("a remote hub requires https; http is allowed only for loopback hosts");
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// `GET /api/v1/health` with the token. The inner `Err` explains a refusal.
async fn verify(url: &str, token: &SecretString) -> anyhow::Result<Result<String, String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("know/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot create the HTTP client")?;
    let mut response = match client
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
    if !status.is_success() {
        return Ok(Err(match status.as_u16() {
            401 => "the hub rejected the token".to_owned(),
            403 => "the token may not read the hub's health".to_owned(),
            _ => format!("the hub answered {status}"),
        }));
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= 1_048_576 => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => return Ok(Err("the hub health response is too large".into())),
            Ok(None) => break,
            Err(_) => return Ok(Err("cannot read the hub health response".into())),
        }
    }
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(Err("the hub returned an invalid health response".into()));
    };
    if body.get("role").and_then(serde_json::Value::as_str) != Some("hub") {
        return Ok(Err("the endpoint did not identify itself as a hub".into()));
    }
    match body.get("status").and_then(serde_json::Value::as_str) {
        Some(health @ ("ok" | "degraded" | "down")) => {
            Ok(Ok(format!("token accepted, health {health}")))
        }
        _ => Ok(Err("the hub returned an invalid health status".into())),
    }
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
            "http://kn.example",
            "https://kn.example/#fragment",
            "https://kn.example\n",
        ] {
            let err = normalize_hub_url(bad).unwrap_err().to_string();
            assert!(!err.contains("KNOWELL_CANARY"), "{err}");
        }
    }

    #[tokio::test]
    async fn verification_rejects_malformed_responses_and_redirects_without_echo() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body) in [
            ("200 OK", "not json"),
            ("200 OK", "{}"),
            ("200 OK", r#"{"role":"standalone","status":"ok"}"#),
            (
                "200 OK",
                r#"{"role":"hub","status":"KNOWELL_CANARY_bad_status"}"#,
            ),
            ("302 Found", ""),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:1/\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut chunk = [0u8; 512];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "client closed before sending headers");
                    request.extend_from_slice(&chunk[..read]);
                    assert!(request.len() <= 8192);
                }
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let secret = SecretString::from("KNOWELL_CANARY_test_token");
            let error = verify(&url, &secret).await.unwrap().unwrap_err();
            assert!(!error.contains("KNOWELL_CANARY"));
            if status == "302 Found" {
                assert!(error.contains("302"));
            }
            server.await.unwrap();
        }
    }
}
