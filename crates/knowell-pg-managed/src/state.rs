//! The small state file that remembers the loopback port.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::Path;

/// Current schema of [`State`]; bumped on incompatible changes.
const SCHEMA: u32 = 1;

/// Persisted per-major state. Contains no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Schema version of this file.
    pub schema: u32,
    /// TCP port on 127.0.0.1 chosen for this cluster.
    pub port: u16,
}

impl State {
    /// A state for `port` at the current schema.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            schema: SCHEMA,
            port,
        }
    }

    /// Read the state file. `Ok(None)` when it does not exist.
    ///
    /// # Errors
    /// Returns [`Error::State`] if the file is malformed, has an unknown
    /// schema, or names port 0.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io(format!("reading {}", path.display()), err)),
        };
        let state: Self = serde_json::from_str(&text).map_err(|err| Error::State {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        if state.schema != SCHEMA {
            return Err(Error::State {
                path: path.to_path_buf(),
                message: format!("unsupported schema {}", state.schema),
            });
        }
        if state.port == 0 {
            return Err(Error::State {
                path: path.to_path_buf(),
                message: "port must not be 0".to_string(),
            });
        }
        Ok(Some(state))
    }

    /// Write the state file atomically (temp file in the same directory, then rename).
    ///
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(|err| Error::State {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|err| Error::io(format!("creating {}", dir.display()), err))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .map_err(|err| Error::io(format!("writing {}", tmp.display()), err))?;
        std::fs::rename(&tmp, path)
            .map_err(|err| Error::io(format!("replacing {}", path.display()), err))
    }
}

/// Ask the OS for a free port on the loopback interface only.
///
/// There is an unavoidable window between releasing the probe socket and
/// PostgreSQL binding it; `start()` re-checks right before launching.
///
/// # Errors
/// Returns [`Error::Io`] if no socket can be bound.
pub fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(|err| Error::io("binding a probe socket on 127.0.0.1", err))?;
    let addr = listener
        .local_addr()
        .map_err(|err| Error::io("reading the probe socket address", err))?;
    Ok(addr.port())
}

/// Whether `port` can currently be bound on 127.0.0.1.
#[must_use]
pub fn port_is_free(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("17").join("state.json");
        State::new(54321).save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), Some(State::new(54321)));
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(State::load(&dir.path().join("nope.json")).unwrap(), None);
    }

    #[test]
    fn malformed_and_hostile_files_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        for bad in [
            "",
            "{",
            "null",
            "[]",
            r#"{"schema":1}"#,
            r#"{"schema":1,"port":70000}"#,
            r#"{"schema":1,"port":-1}"#,
            r#"{"schema":1,"port":0}"#,
            r#"{"schema":99,"port":5432}"#,
            "\u{0}\u{0}",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(State::load(&path).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn picked_port_is_nonzero_and_loopback_bindable() {
        let port = pick_free_port().unwrap();
        assert_ne!(port, 0);
        assert!(port_is_free(port));
    }

    #[test]
    fn occupied_port_is_reported_busy() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(port));
    }
}
