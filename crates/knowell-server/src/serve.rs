//! Binding and serving with graceful shutdown.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::time::Duration;

use axum::{Extension, Router};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::config::ServerConfig;
use crate::error::ServerError;

/// How long [`serve`] waits for open connections after the shutdown signal
/// before returning anyway.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Lets long-lived streams (SSE) end when shutdown starts.
#[derive(Debug, Clone)]
pub(crate) struct ShutdownSignal(pub(crate) watch::Receiver<bool>);

/// Validates `config` (including the loopback rule) and binds its listen
/// address.
///
/// # Errors
/// [`ServerError::Config`] for an invalid configuration,
/// [`ServerError::Bind`] when the address cannot be bound.
pub async fn bind(config: &ServerConfig) -> Result<TcpListener, ServerError> {
    config.validate()?;
    TcpListener::bind(config.listen)
        .await
        .map_err(|source| ServerError::Bind {
            address: config.listen,
            source,
        })
}

/// Serves `router` on `listener` until `shutdown` resolves, then stops
/// accepting, ends progress streams and waits up to
/// [`DEFAULT_SHUTDOWN_GRACE`] for in-flight requests.
///
/// Connections carry their peer address (`ConnectInfo<SocketAddr>`), which
/// the MCP caller resolver uses. Bind with [`bind`] so the loopback rule is
/// enforced.
///
/// # Errors
/// [`ServerError::Serve`] when the accept loop fails.
pub async fn serve<F>(listener: TcpListener, router: Router, shutdown: F) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_with_grace(listener, router, shutdown, DEFAULT_SHUTDOWN_GRACE).await
}

/// [`serve`] with an explicit grace period.
///
/// # Errors
/// [`ServerError::Serve`] when the accept loop fails.
pub async fn serve_with_grace<F>(
    listener: TcpListener,
    router: Router,
    shutdown: F,
    grace: Duration,
) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = watch::channel(false);
    let mut stopping = tx.subscribe();
    let router = router.layer(Extension(ShutdownSignal(rx)));
    let signal = async move {
        shutdown.await;
        tracing::info!("shutdown requested; draining connections");
        let _ = tx.send(true);
    };
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(signal)
    .into_future();
    let deadline = async move {
        while !*stopping.borrow_and_update() {
            if stopping.changed().await.is_err() {
                break;
            }
        }
        tokio::time::sleep(grace).await;
    };
    tokio::select! {
        result = server => result.map_err(ServerError::Serve),
        () = deadline => {
            tracing::warn!(grace_ms = grace.as_millis() as u64, "connections still open after the shutdown grace period; stopping");
            Ok(())
        }
    }
}
