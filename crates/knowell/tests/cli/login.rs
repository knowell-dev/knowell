//! `know login`.

use crate::common::{Sandbox, Server};

const CANARY_TOKEN: &str = "kn_KNOWELL_CANARY_token_7a1c";

#[test]
fn rejected_token_saves_nothing_and_never_echoes() {
    let hub = Sandbox::new();
    hub.write_engine("version = 1\n");
    let server = Server::start(&hub, &["--no-database"]);
    let url = format!("http://{}", server.addr);

    let mut edge = Sandbox::new();
    edge.set_env("KNOWELL_CLI_TOKEN", CANARY_TOKEN);
    let out = edge.run(&["login", &url, "--token-ref", "env:KNOWELL_CLI_TOKEN"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(!out.all().contains(CANARY_TOKEN), "token leaked: {out:?}");
    assert!(out.stderr.contains("rejected"), "{out:?}");
    assert!(!edge.engine_config().exists());

    let saved = edge.run(&[
        "login",
        &url,
        "--token-ref",
        "env:KNOWELL_CLI_TOKEN",
        "--skip-verify",
    ]);
    assert_eq!(saved.code, 0, "{saved:?}");
    let config = std::fs::read_to_string(edge.engine_config()).unwrap();
    assert!(!config.contains(CANARY_TOKEN), "{config}");
    assert!(config.contains("env:KNOWELL_CLI_TOKEN"), "{config}");
    let parsed = knowell_config::load_engine(&edge.engine_config()).unwrap();
    assert_eq!(parsed.server.role, knowell_config::ServerRole::Edge);
    assert_eq!(server.stop(), 0);
}

#[test]
fn a_pasted_token_is_refused_without_echo() {
    let sb = Sandbox::new();
    let out = sb.run(&[
        "login",
        "https://hub.example",
        "--token-ref",
        CANARY_TOKEN,
        "--skip-verify",
    ]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(!out.all().contains(CANARY_TOKEN), "{out:?}");
}
