//! A launcher's private lifetime channel, independent of managed PostgreSQL.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio_util::sync::CancellationToken;

pub(crate) struct Parent {
    token: CancellationToken,
    stopped: Arc<AtomicBool>,
    stream: TcpStream,
    monitor: Option<std::thread::JoinHandle<()>>,
}

impl Parent {
    pub(crate) fn connect() -> anyhow::Result<Option<Self>> {
        let endpoint = std::env::var_os("KNOWELL_PARENT_ENDPOINT");
        let secret = std::env::var_os("KNOWELL_PARENT_TOKEN");
        Self::from_parts(endpoint, secret)
    }

    fn from_parts(
        endpoint: Option<std::ffi::OsString>,
        secret: Option<std::ffi::OsString>,
    ) -> anyhow::Result<Option<Self>> {
        let (endpoint, secret) = match (endpoint, secret) {
            (Some(endpoint), Some(secret)) => (endpoint, secret),
            (None, None) => return Ok(None),
            _ => bail!("incomplete launcher lifetime channel"),
        };
        let address: SocketAddr = endpoint
            .to_str()
            .and_then(|s| s.parse().ok())
            .context("invalid launcher lifetime endpoint")?;
        if !address.ip().is_loopback() {
            bail!("launcher lifetime endpoint must be loopback")
        }
        let secret = secret
            .to_str()
            .context("invalid launcher lifetime identity")?;
        uuid::Uuid::parse_str(secret)
            .map_err(|_| anyhow::anyhow!("invalid launcher lifetime identity"))?;
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .context("launcher lifetime channel is unavailable")?;
        stream
            .write_all(secret.as_bytes())
            .context("launcher lifetime handshake failed")?;
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        let mut reader = stream.try_clone()?;
        let token = CancellationToken::new();
        let cancelled = token.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let finish = stopped.clone();
        let monitor = std::thread::Builder::new()
            .name("knowell-parent".into())
            .spawn(move || {
                let mut buf = [0_u8; 1];
                while !finish.load(Ordering::Acquire) {
                    match reader.read(&mut buf) {
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        _ => break,
                    }
                }
                if finish.load(Ordering::Acquire) {
                    return;
                }
                cancelled.cancel();
                let deadline = Instant::now() + Duration::from_secs(12);
                while !finish.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !finish.load(Ordering::Acquire) {
                    // A client that killed its launcher cannot leave an indexing
                    // engine indefinitely alive. OS teardown rolls back open SQL
                    // transactions; managed PostgreSQL remains independently alive.
                    std::process::exit(130);
                }
            })?;
        Ok(Some(Self {
            token,
            stopped,
            stream,
            monitor: Some(monitor),
        }))
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn malformed_or_incomplete_channels_do_not_echo_input() {
        for (endpoint, secret) in [
            (Some("KNOWELL_CANARY_ENDPOINT"), None),
            (None, Some("KNOWELL_CANARY_IDENTITY")),
            (Some("203.0.113.1:12345"), Some("KNOWELL_CANARY_IDENTITY")),
            (Some("127.0.0.1:12345"), Some("KNOWELL_CANARY_IDENTITY")),
        ] {
            let result = Parent::from_parts(endpoint.map(Into::into), secret.map(Into::into));
            let error = result.err().unwrap().to_string();
            assert!(!error.contains("KNOWELL_CANARY"));
            assert!(!error.contains("203.0.113"));
        }
        assert!(Parent::from_parts(None, None).unwrap().is_none());
    }

    #[test]
    fn parent_eof_cancels_and_clean_shutdown_joins_monitor() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let nonce = "6b6e6f77-656c-4c00-8000-000000000001";
        let parent = Parent::from_parts(Some(address.into()), Some(nonce.into()))
            .unwrap()
            .unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = [0_u8; 36];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, nonce.as_bytes());
        drop(stream);
        let token = parent.token();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !token.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(token.is_cancelled());
        drop(parent);
    }
}
