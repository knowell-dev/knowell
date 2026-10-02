//! Running a SCIP indexer as a child process.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::registry::IndexerSpec;

/// How much of the indexer's stderr is kept (the last bytes).
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Environment variables passed to indexers. Everything else, including
/// tokens, API keys and registry credentials, is removed from the child's
/// environment. Names are matched case-sensitively except on Windows, where
/// the OS itself is case-insensitive.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SystemDrive",
    "WINDIR",
    "COMSPEC",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMDATA",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "USER",
    "USERNAME",
    "JAVA_HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "GOCACHE",
    "GOMODCACHE",
    "DOTNET_ROOT",
    "DOTNET_CLI_HOME",
];

/// Why an indexer run did not produce an index.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// The tool is not on `PATH`. There is no fallback: the caller must report
    /// that precise analysis is unavailable and keep syntax-level results.
    #[error("indexer `{tool}` is not installed ({install_hint})")]
    NotInstalled {
        /// The missing executable.
        tool: String,
        /// How to install it.
        install_hint: String,
    },
    /// The project directory does not exist or is not a directory.
    #[error("project directory `{0}` is not a directory")]
    BadProjectDir(String),
    /// The process could not be started or waited for.
    #[error("cannot run indexer `{tool}`: {source}")]
    Spawn {
        /// The executable.
        tool: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The process ran longer than the timeout and was killed.
    #[error("indexer `{tool}` timed out after {timeout:?}; stderr tail: {stderr}")]
    TimedOut {
        /// The executable.
        tool: String,
        /// The timeout that elapsed.
        timeout: Duration,
        /// The last bytes of stderr.
        stderr: String,
    },
    /// The process exited unsuccessfully.
    #[error("indexer `{tool}` failed ({status}); stderr tail: {stderr}")]
    Failed {
        /// The executable.
        tool: String,
        /// Exit status description.
        status: String,
        /// The last bytes of stderr.
        stderr: String,
    },
    /// The process exited successfully but wrote no index file.
    #[error("indexer `{tool}` finished without writing `{path}`; stderr tail: {stderr}")]
    NoOutput {
        /// The executable.
        tool: String,
        /// The expected output path.
        path: String,
        /// The last bytes of stderr.
        stderr: String,
    },
}

/// A successful indexer run.
#[derive(Debug, Clone)]
pub struct IndexerRun {
    /// The resolved executable that ran.
    pub tool_path: PathBuf,
    /// Absolute path of the written index.
    pub out_path: PathBuf,
    /// Wall-clock time of the run.
    pub elapsed: Duration,
    /// The last bytes of stderr (warnings are common and not errors).
    pub stderr_tail: String,
}

/// Runs `spec` in `project_dir`, writing the index to `out_path`.
///
/// The process is started directly (no shell) with the project directory as
/// its working directory, stdin and stdout closed, stderr captured (only the
/// last [`STDERR_TAIL_BYTES`] are kept) and a cleared environment that gets
/// back only a short allowlist of non-secret variables. A stale `out_path`
/// is removed first so an old index is never mistaken for a fresh one. The
/// process is killed when `timeout` elapses.
pub fn run_indexer(
    spec: &IndexerSpec,
    project_dir: &Path,
    out_path: &Path,
    timeout: Duration,
) -> Result<IndexerRun, IndexerError> {
    let Some(tool_path) = spec.locate() else {
        return Err(IndexerError::NotInstalled {
            tool: spec.tool.to_owned(),
            install_hint: spec.install_hint.to_owned(),
        });
    };
    if !project_dir.is_dir() {
        return Err(IndexerError::BadProjectDir(
            project_dir.display().to_string(),
        ));
    }
    let spawn_err = |source| IndexerError::Spawn {
        tool: spec.tool.to_owned(),
        source,
    };
    let out_abs = std::path::absolute(out_path).map_err(spawn_err)?;
    match std::fs::remove_file(&out_abs) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(spawn_err(e)),
    }
    let args = (spec.args)(project_dir, &out_abs);
    let (status_ok, status_text, stderr, elapsed) =
        run_process(spec.tool, &tool_path, &args, project_dir, timeout)?;
    if !status_ok {
        return Err(IndexerError::Failed {
            tool: spec.tool.to_owned(),
            status: status_text,
            stderr,
        });
    }
    if !out_abs.is_file() {
        return Err(IndexerError::NoOutput {
            tool: spec.tool.to_owned(),
            path: out_abs.display().to_string(),
            stderr,
        });
    }
    Ok(IndexerRun {
        tool_path,
        out_path: out_abs,
        elapsed,
        stderr_tail: stderr,
    })
}

fn run_process(
    tool: &str,
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    timeout: Duration,
) -> Result<(bool, String, String, Duration), IndexerError> {
    let spawn_err = |source| IndexerError::Spawn {
        tool: tool.to_owned(),
        source,
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    // Telemetry prompts would only add noise to captured stderr.
    cmd.env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");

    let start = Instant::now();
    let mut child = cmd.spawn().map_err(spawn_err)?;

    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader = child.stderr.take().map(|mut pipe| {
        let tail = Arc::clone(&tail);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut guard = tail.lock().unwrap_or_else(|p| p.into_inner());
                guard.extend(buf.iter().take(n));
                let len = guard.len();
                if len > STDERR_TAIL_BYTES {
                    guard.drain(..len - STDERR_TAIL_BYTES);
                }
            }
        })
    });

    let poll = Duration::from_millis(20);
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // Best effort: the process may already be gone.
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(None);
                }
                std::thread::sleep(poll);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(Some(e));
            }
        }
    };

    // A grandchild may keep the pipe open after the child died; never wait for it forever.
    if let Some(handle) = reader {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
    let stderr = {
        let guard = tail.lock().unwrap_or_else(|p| p.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    };

    match exit {
        Ok(status) => Ok((
            status.success(),
            status.to_string(),
            stderr,
            start.elapsed(),
        )),
        Err(None) => Err(IndexerError::TimedOut {
            tool: tool.to_owned(),
            timeout,
            stderr,
        }),
        Err(Some(e)) => Err(spawn_err(e)),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::registry::{Language, Marker};

    fn no_args(_: &Path, _: &Path) -> Vec<OsString> {
        Vec::new()
    }

    fn spec(tool: &'static str, args: crate::registry::ArgsFn) -> IndexerSpec {
        IndexerSpec {
            id: tool,
            language: Language::Rust,
            tool,
            markers: &[Marker::File("Cargo.toml")],
            install_hint: "install the fake tool",
            args,
        }
    }

    #[test]
    fn missing_tool_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec("knowell-definitely-missing-indexer-7731", no_args);
        let err = run_indexer(
            &s,
            dir.path(),
            &dir.path().join("index.scip"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        match err {
            IndexerError::NotInstalled { tool, install_hint } => {
                assert_eq!(tool, "knowell-definitely-missing-indexer-7731");
                assert_eq!(install_hint, "install the fake tool");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[cfg(windows)]
    fn sleeper_args(_: &Path, _: &Path) -> Vec<OsString> {
        vec!["-n".into(), "30".into(), "127.0.0.1".into()]
    }
    #[cfg(windows)]
    const SLEEPER: &str = "ping";

    #[cfg(unix)]
    fn sleeper_args(_: &Path, _: &Path) -> Vec<OsString> {
        vec!["30".into()]
    }
    #[cfg(unix)]
    const SLEEPER: &str = "sleep";

    #[test]
    fn long_running_tool_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec(SLEEPER, sleeper_args);
        if s.locate().is_none() {
            return; // the platform has no such helper; skip cleanly
        }
        let started = Instant::now();
        let err = run_indexer(
            &s,
            dir.path(),
            &dir.path().join("index.scip"),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(matches!(err, IndexerError::TimedOut { .. }), "got {err}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    fn cargo_version(_: &Path, _: &Path) -> Vec<OsString> {
        vec!["--version".into()]
    }

    fn cargo_bad_flag(_: &Path, _: &Path) -> Vec<OsString> {
        vec!["--knowell-no-such-flag".into()]
    }

    #[test]
    fn success_without_output_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec("cargo", cargo_version);
        if s.locate().is_none() {
            return;
        }
        let err = run_indexer(
            &s,
            dir.path(),
            &dir.path().join("index.scip"),
            Duration::from_secs(60),
        )
        .unwrap_err();
        assert!(matches!(err, IndexerError::NoOutput { .. }), "got {err}");
    }

    #[test]
    fn nonzero_exit_reports_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec("cargo", cargo_bad_flag);
        if s.locate().is_none() {
            return;
        }
        let err = run_indexer(
            &s,
            dir.path(),
            &dir.path().join("index.scip"),
            Duration::from_secs(60),
        )
        .unwrap_err();
        match err {
            IndexerError::Failed { stderr, .. } => assert!(!stderr.is_empty()),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn bad_project_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec("cargo", cargo_version);
        if s.locate().is_none() {
            return;
        }
        let err = run_indexer(
            &s,
            &dir.path().join("missing"),
            &dir.path().join("index.scip"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(err, IndexerError::BadProjectDir(_)));
    }

    #[test]
    fn stale_output_is_removed_before_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("index.scip");
        std::fs::write(&out, b"stale").unwrap();
        let s = spec("cargo", cargo_version);
        if s.locate().is_none() {
            return;
        }
        let err = run_indexer(&s, dir.path(), &out, Duration::from_secs(60)).unwrap_err();
        assert!(matches!(err, IndexerError::NoOutput { .. }));
        assert!(!out.exists());
    }
}
