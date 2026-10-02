//! Serving over TCP and graceful shutdown (progress streams end).

use std::time::Duration;

use knowell_auth::TokenScope;
use knowell_config::ServerRole;
use knowell_server::{ServerConfig, bind, build_router, serve_with_grace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::common::*;

async fn http(port: u16, request: String) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn read_until(stream: &mut TcpStream, needle: &str) -> String {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .expect("timed out waiting for the server")
            .unwrap();
        seen.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&seen).into_owned();
        if text.contains(needle) || n == 0 {
            return text;
        }
    }
}

#[tokio::test]
async fn serves_over_tcp_and_shuts_down_with_open_streams() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut cfg = config();
    cfg.listen = format!("127.0.0.1:{port}").parse().unwrap();
    let h = harness_with(cfg, |b| b);
    let token = h.token(user(1), &[TokenScope::Read]);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_with_grace(
        listener,
        build_router(h.state.clone()),
        async move {
            let _ = stop_rx.await;
        },
        Duration::from_secs(5),
    ));

    let mut live = http(
        port,
        format!("GET /api/v1/health/live HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    let text = read_until(&mut live, "\"ok\"").await;
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.to_ascii_lowercase().contains("x-request-id:"));

    // The configured port is part of the host allow-list; another is not.
    let mut wrong = http(
        port,
        "GET /api/v1/health/live HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n"
            .to_owned(),
    )
    .await;
    let text = read_until(&mut wrong, "host_not_allowed").await;
    assert!(text.starts_with("HTTP/1.1 403"), "{text}");

    let mut events = http(
        port,
        format!(
            "GET /api/v1/events HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    )
    .await;
    let text = read_until(&mut events, "heartbeat").await;
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");

    stop_tx.send(()).unwrap();
    // The open stream ends and the server returns well before the grace period.
    let result = tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .expect("server did not stop")
        .unwrap();
    assert!(result.is_ok());
    let rest = read_until(&mut events, "\u{0}never").await;
    assert!(!rest.contains("never"));
}

#[tokio::test]
async fn bind_enforces_the_loopback_rule() {
    let cfg = ServerConfig::new(
        ServerRole::Standalone,
        "0.0.0.0:7420".parse().unwrap(),
        n("acme"),
    );
    let err = bind(&cfg).await.unwrap_err();
    assert!(err.to_string().contains("loopback"));
    let err = knowell_server::AppState::builder(cfg).build().unwrap_err();
    assert!(err.to_string().contains("loopback"));
}
