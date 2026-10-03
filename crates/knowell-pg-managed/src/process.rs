//! Running PostgreSQL programs as child processes.
//!
//! Secrets reach children only through a `PGPASSFILE` path in the child's
//! environment, never through arguments. Captured output is redacted before it
//! can end up in an error message.

use crate::error::{Error, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[cfg(windows)]
mod windows;

/// How much of a failing program's output is kept in an error, in characters.
const OUTPUT_TAIL_CHARS: usize = 2000;

#[derive(Debug, Clone, Default)]
enum OutputMode {
    #[default]
    Capture,
    File(PathBuf),
    Null,
}

/// Per-invocation options.
#[derive(Debug, Clone)]
pub(crate) struct RunOptions {
    /// Extra environment variables for the child only.
    pub(crate) env: Vec<(String, OsString)>,
    /// Working directory, if it matters (`pg_upgrade` writes logs there).
    pub(crate) cwd: Option<PathBuf>,
    /// Kill the child after this long.
    pub(crate) timeout: Duration,
    /// Text to mask in captured output.
    pub(crate) redact: Option<String>,
    /// Where the child's standard output and error go.
    output: OutputMode,
}

impl RunOptions {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            env: Vec::new(),
            cwd: None,
            timeout,
            redact: None,
            output: OutputMode::Capture,
        }
    }

    pub(crate) fn env(mut self, key: &str, value: impl Into<OsString>) -> Self {
        self.env.push((key.to_string(), value.into()));
        self
    }

    pub(crate) fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    pub(crate) fn output_file(mut self, path: &Path) -> Self {
        self.output = OutputMode::File(path.to_path_buf());
        self
    }

    /// Disconnect a server launcher from the caller's standard streams.
    pub(crate) fn null_stdio(mut self) -> Self {
        self.output = OutputMode::Null;
        self
    }

    pub(crate) fn redact(mut self, secret: &str) -> Self {
        if !secret.is_empty() {
            self.redact = Some(secret.to_string());
        }
        self
    }
}

/// Captured result of a finished child.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    /// Exit code, `None` if killed by a signal.
    pub(crate) code: Option<i32>,
    /// Standard output (redacted).
    pub(crate) stdout: String,
    /// Standard error (redacted).
    pub(crate) stderr: String,
}

impl Output {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn tail(&self) -> String {
        let mut text = String::new();
        text.push_str(self.stderr.trim());
        if !self.stdout.trim().is_empty() {
            if !text.is_empty() {
                text.push_str(" | ");
            }
            text.push_str(self.stdout.trim());
        }
        let total = text.chars().count();
        if total > OUTPUT_TAIL_CHARS {
            let skip = total.saturating_sub(OUTPUT_TAIL_CHARS);
            let kept: String = text.chars().skip(skip).collect();
            format!("...{kept}")
        } else {
            text
        }
    }
}

/// Name of a program file inside a `bin` directory, with the platform suffix.
pub(crate) fn program_path(bin_dir: &Path, name: &str) -> PathBuf {
    bin_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Run `program` and return its output whatever the exit code.
///
/// # Errors
/// [`Error::Io`] if the program cannot be started, [`Error::Timeout`] if it
/// exceeds `options.timeout` (the child is killed).
pub(crate) async fn run_unchecked(
    program: &Path,
    args: &[OsString],
    options: &RunOptions,
) -> Result<Output> {
    let display = program.file_stem().map_or_else(
        || "program".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    #[cfg(windows)]
    if matches!(options.output, OutputMode::Null) {
        return windows::run_null(program, args, options, &display).await;
    }
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    match &options.output {
        OutputMode::File(path) => {
            let file = std::fs::File::create(path)
                .map_err(|err| Error::io(format!("creating {}", path.display()), err))?;
            let second = file
                .try_clone()
                .map_err(|err| Error::io(format!("duplicating {}", path.display()), err))?;
            command
                .stdout(Stdio::from(file))
                .stderr(Stdio::from(second));
        }
        OutputMode::Capture => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        OutputMode::Null => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Do not let ambient PG* variables (PGPASSWORD, PGHOST, PGDATA, ...) steer the child.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().to_ascii_uppercase().starts_with("PG") {
            command.env_remove(key);
        }
    }
    for (key, value) in &options.env {
        command.env(key, value);
    }
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = command
        .spawn()
        .map_err(|err| Error::io(format!("starting {display}"), err))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();

    let work = async {
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(pipe) = out_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(pipe) = err_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        };
        let (status, out, err) = tokio::join!(child.wait(), read_out, read_err);
        (status, out, err)
    };

    let (status, mut out, err) = match tokio::time::timeout(options.timeout, work).await {
        Ok(done) => done,
        Err(_) => {
            return Err(Error::Timeout {
                program: display,
                seconds: options.timeout.as_secs(),
            });
        }
    };
    let status = status.map_err(|e| Error::io(format!("waiting for {display}"), e))?;
    if let OutputMode::File(path) = &options.output {
        out = std::fs::read(path).unwrap_or_default();
    }
    let mask = |bytes: Vec<u8>| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        match &options.redact {
            Some(secret) => text.replace(secret.as_str(), "***"),
            None => text,
        }
    };
    Ok(Output {
        code: status.code(),
        stdout: mask(out),
        stderr: mask(err),
    })
}

/// Run `program`; a non-zero exit becomes [`Error::Command`].
///
/// # Errors
/// As [`run_unchecked`], plus [`Error::Command`] on failure.
pub(crate) async fn run(program: &Path, args: &[OsString], options: &RunOptions) -> Result<Output> {
    let output = run_unchecked(program, args, options).await?;
    if output.success() {
        Ok(output)
    } else {
        Err(Error::Command {
            program: program.file_stem().map_or_else(
                || "program".to_string(),
                |s| s.to_string_lossy().into_owned(),
            ),
            code: output
                .code
                .map_or_else(|| "none".to_string(), |c| c.to_string()),
            output: output.tail(),
        })
    }
}

/// Convenience: build an argument list from string-likes.
pub(crate) fn args<const N: usize>(items: [&str; N]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

#[cfg(test)]
mod pipe_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stdout: &str, stderr: &str) -> Output {
        Output {
            code: Some(1),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn tail_combines_and_truncates_on_char_boundaries() {
        assert_eq!(out("o", "e").tail(), "e | o");
        let long = "é".repeat(OUTPUT_TAIL_CHARS + 50);
        let tail = out("", &long).tail();
        assert!(tail.starts_with("..."));
        assert_eq!(tail.chars().count(), OUTPUT_TAIL_CHARS + 3);
    }

    #[test]
    fn program_path_adds_platform_suffix() {
        let path = program_path(Path::new("bin"), "pg_dump");
        let expected = format!("pg_dump{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(path, Path::new("bin").join(expected));
    }

    #[tokio::test]
    async fn missing_program_is_an_io_error() {
        let options = RunOptions::new(Duration::from_secs(5));
        let err = run(
            Path::new("definitely-not-a-real-program-xyz"),
            &[],
            &options,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }
}
