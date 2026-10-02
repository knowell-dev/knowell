//! Panel asset serving through the full router.

use axum::http::{Method, StatusCode};
use knowell_server::PanelMode;

use crate::common::*;

#[tokio::test]
async fn embedded_index_is_served_with_the_panel_csp() {
    // Embedded is `panel/dist` when it was built, the placeholder otherwise:
    // both have an index.html.
    let h = harness();
    let reply = h.send(get("/").build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(reply.headers["cache-control"], "no-cache");
    let csp = reply.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("unsafe-inline"));
    assert_eq!(reply.headers["x-frame-options"], "DENY");
    assert!(
        reply
            .text()
            .to_ascii_lowercase()
            .contains("<!doctype html>")
    );
    // The panel needs no session.
    let reply = h.send(get("/index.html").build()).await;
    assert_eq!(reply.status, StatusCode::OK);
}

fn directory_harness() -> (Harness, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>panel</title>",
    )
    .unwrap();
    std::fs::write(
        dist.join("favicon.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )
    .unwrap();
    std::fs::write(dist.join("assets").join("index-AbC123.js"), "export {};").unwrap();
    std::fs::write(dist.join("assets").join("index-AbC123.css"), "body{}").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "KNOWELL_CANARY_outside").unwrap();
    let mut cfg = config();
    cfg.panel = PanelMode::Directory(dist);
    (harness_with(cfg, |b| b), tmp)
}

#[tokio::test]
async fn directory_panel_types_caching_and_traversal() {
    let (h, _tmp) = directory_harness();
    let reply = h.send(get("/assets/index-AbC123.js").build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    let ct = reply.headers["content-type"].to_str().unwrap();
    assert!(ct.contains("javascript"), "{ct}");
    assert_eq!(
        reply.headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    let reply = h.send(get("/assets/index-AbC123.css").build()).await;
    assert_eq!(reply.headers["content-type"], "text/css; charset=utf-8");
    let reply = h.send(get("/favicon.svg").build()).await;
    assert_eq!(
        reply.headers["content-type"],
        "image/svg+xml; charset=utf-8"
    );
    assert_eq!(reply.headers["cache-control"], "no-cache");
    let reply = h.send(get("/").build()).await;
    assert_eq!(reply.text(), "<!doctype html><title>panel</title>");

    for path in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/assets/..%2f..%2fsecret.txt",
        "/assets%5c..%5c..%5csecret.txt",
        "/missing.js",
        "/assets/",
    ] {
        let reply = h.send(get(path).build()).await;
        reply.problem(StatusCode::NOT_FOUND, "not_found");
        assert!(!reply.text().contains("CANARY"), "{path}");
    }
    // No SPA fallback: hash routing needs none.
    h.send(get("/projects").build())
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");
    let reply = h.send(request(Method::POST, "/").build()).await;
    reply.problem(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    assert_eq!(reply.headers["allow"], "GET, HEAD");
    let reply = h.send(request(Method::HEAD, "/").build()).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn disabled_panel_and_missing_directory() {
    let mut cfg = config();
    cfg.panel = PanelMode::Disabled;
    let h = harness_with(cfg, |b| b);
    h.send(get("/").build())
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");

    let mut cfg = config();
    cfg.panel = PanelMode::Directory("/definitely/not/a/panel/dir".into());
    let err = knowell_server::AppState::builder(cfg).build().unwrap_err();
    assert!(err.to_string().contains("panel directory"));
}
