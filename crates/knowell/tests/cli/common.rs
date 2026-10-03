//! Shared harness: sandboxes, process helpers, a scratch database.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions, Connection};

/// Nextest remaps this path when tests run from an archive on another runner.
fn binary_path() -> std::ffi::OsString {
    std::env::var_os("NEXTEST_BIN_EXE_know")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_know"))
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_know").into())
}

/// Admin URL of the throwaway test server (see the knowell-store README).
pub(crate) const DB_ENV: &str = "KNOWELL_TEST_DATABASE_URL";

/// Set to `1` (CI database partitions, the Docker test runner) to make a
/// missing prerequisite (database URL, `git`) fail the test instead of
/// printing a skip line, so a suite that never ran cannot pass.
pub(crate) const STRICT_ENV: &str = "KNOWELL_TEST_STRICT";

/// Whether [`STRICT_ENV`] forbids skipping.
pub(crate) fn strict() -> bool {
    std::env::var(STRICT_ENV).is_ok_and(|value| value == "1")
}

/// Variables of the developer's shell that must not leak into a sandbox.
const SCRUBBED: &[&str] = &[
    "KNOWELL_LOG",
    "KNOWELL_PUBLIC_URL",
    "KNOWELL_ALLOWED_HOSTS",
    "KNOWELL_HUB_TOKEN",
    "KNOWELL_TOKEN_PEPPER",
    "KNOWELL_DATABASE_URL",
    "KNOWELL_TEST_DATABASE_URL",
];

/// A temporary world for one test: `root/home` is `$HOME`, `root/kh` is
/// `$KNOWELL_HOME`, `root/work` the working directory.
pub(crate) struct Sandbox {
    pub(crate) root: tempfile::TempDir,
    pub(crate) env: Vec<(String, String)>,
}

/// Result of one run.
pub(crate) struct Run {
    pub(crate) code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Run {
    pub(crate) fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {}\n--- stdout\n{}\n--- stderr\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["home", "kh", "work"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self {
            root,
            env: Vec::new(),
        }
    }

    pub(crate) fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    pub(crate) fn knowell_home(&self) -> PathBuf {
        self.root.path().join("kh")
    }

    pub(crate) fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    pub(crate) fn engine_config(&self) -> PathBuf {
        self.knowell_home().join("config.toml")
    }

    /// Writes the engine configuration.
    pub(crate) fn write_engine(&self, text: &str) {
        std::fs::write(self.engine_config(), text).unwrap();
    }

    /// Sets an environment variable for every later run.
    pub(crate) fn set_env(&mut self, name: &str, value: &str) {
        self.env.push((name.to_owned(), value.to_owned()));
    }

    pub(crate) fn command(&self, args: &[&str]) -> Command {
        self.command_in(&self.work(), args)
    }

    pub(crate) fn command_in(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(binary_path());
        cmd.args(args).current_dir(dir);
        for name in SCRUBBED {
            cmd.env_remove(name);
        }
        cmd.env("KNOWELL_HOME", self.knowell_home())
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("NO_COLOR", "1");
        for (name, value) in &self.env {
            cmd.env(name, value);
        }
        cmd
    }

    pub(crate) fn run(&self, args: &[&str]) -> Run {
        self.run_in(&self.work(), args)
    }

    /// Capture both pipes with a deadline, including EOF from descendants.
    pub(crate) fn run_with_timeout(&self, args: &[&str], timeout: Duration) -> Run {
        block_on(async {
            let mut command = tokio::process::Command::from(self.command(args));
            command.stdin(Stdio::null()).kill_on_drop(true);
            tokio::time::timeout(timeout, command.output())
                .await
                .expect("CLI did not exit and close its output pipes before the deadline")
                .unwrap()
                .into()
        })
    }

    pub(crate) fn run_in(&self, dir: &Path, args: &[&str]) -> Run {
        self.command_in(dir, args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .into()
    }
}

/// A free loopback port (released before use; good enough for tests).
pub(crate) fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Minimal HTTP/1.1 GET; returns (status code, body).
pub(crate) fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let (status, _, body) = http_request(addr, path, None);
    (status, body)
}

/// Minimal HTTP/1.1 GET with an optional `Cookie`; returns (status code,
/// header block, body).
pub(crate) fn http_request(
    addr: SocketAddr,
    path: &str,
    cookie: Option<&str>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\n{cookie}\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_owned(), b.to_owned()))
        .unwrap_or_default();
    (status, head, body)
}

/// The `name=value` of the first `Set-Cookie` header.
pub(crate) fn session_cookie(head: &str) -> Option<String> {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.trim().split(';').next())
        .map(str::to_owned)
}

/// Reads stdout lines of a child on a thread, so tests can wait with a
/// timeout.
pub(crate) struct Lines {
    rx: mpsc::Receiver<String>,
    pub(crate) seen: Vec<String>,
}

impl Lines {
    pub(crate) fn spawn(stdout: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    /// The next line, or `None` after `timeout` or at end of output.
    pub(crate) fn next(&mut self, timeout: Duration) -> Option<String> {
        let line = self.rx.recv_timeout(timeout).ok()?;
        self.seen.push(line.clone());
        Some(line)
    }

    /// Waits for a line satisfying `pred`.
    pub(crate) fn wait_for(
        &mut self,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let line = self.next(left)?;
            if pred(&line) {
                return Some(line);
            }
        }
    }
}

/// A running `know serve`, stopped gracefully by closing its stdin.
pub(crate) struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    pub(crate) addr: SocketAddr,
    pub(crate) lines: Lines,
}

impl Server {
    pub(crate) fn start(sandbox: &Sandbox, extra: &[&str]) -> Self {
        let port = free_port();
        let listen = format!("127.0.0.1:{port}");
        let mut args = vec!["serve", "--listen", &listen, "--stop-on-stdin-close"];
        args.extend_from_slice(extra);
        let mut child = sandbox
            .command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let mut lines = Lines::spawn(child.stdout.take().unwrap());
        let ready = lines.wait_for(Duration::from_secs(60), |l| l.starts_with("press Ctrl+C"));
        assert!(
            ready.is_some(),
            "server did not start; output so far: {:?}",
            lines.seen
        );
        Self {
            child,
            stdin,
            addr: listen.parse().unwrap(),
            lines,
        }
    }

    /// Closes stdin and waits for a clean exit; returns the exit code.
    pub(crate) fn stop(mut self) -> i32 {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("server did not stop within 30 s after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The admin URL of the test server, or `None` with a skip line.
pub(crate) fn admin_url(test: &str) -> Option<String> {
    match std::env::var(DB_ENV) {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            assert!(
                !strict(),
                "{test} cannot run: {DB_ENV} is not set, and {STRICT_ENV}=1 forbids skipping"
            );
            eprintln!("skipping {test}: {DB_ENV} is not set");
            None
        }
    }
}

/// The password part of a URL, to assert it never appears in output.
pub(crate) fn password_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let userinfo = authority.rsplit_once('@')?.0;
    let password = userinfo.split_once(':')?.1;
    (!password.is_empty()).then(|| password.to_owned())
}

/// A scratch database on the test server, dropped at the end of the test.
pub(crate) struct ScratchDb {
    admin: PgConnectOptions,
    name: String,
    /// Connection URL of the scratch database (contains the password).
    pub(crate) url: String,
}

impl ScratchDb {
    pub(crate) fn create(admin_url: &str) -> Self {
        let admin = PgConnectOptions::from_str(admin_url).unwrap();
        let name = format!("knowell_cli_{}", uuid::Uuid::now_v7().simple());
        block_on(async {
            let mut conn = admin.connect().await.unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
        });
        let url = with_database(admin_url, &name);
        Self { admin, name, url }
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        block_on(async move {
            if let Ok(mut conn) = admin.connect().await {
                let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                )))
                .execute(&mut conn)
                .await;
                let _ = conn.close().await;
            }
        });
    }
}

/// Replaces the database path of a `postgres://` URL.
fn with_database(url: &str, database: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap();
    let (before_query, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let authority = before_query.split('/').next().unwrap();
    let mut out = format!("{scheme}://{authority}/{database}");
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// Whether `git` can be run (the fixture generator needs it).
pub(crate) fn git_available() -> bool {
    let available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        available || !strict(),
        "git is not available, and {STRICT_ENV}=1 forbids skipping"
    );
    available
}

/// Generates the synthetic multi-project fixture (one git repository per
/// project) into `dir` and returns the project directory names.
pub(crate) fn generate_fixture(sandbox: &Sandbox, dir: &Path) -> Vec<String> {
    let out = sandbox.run(&["eval", "generate", "--out", dir.to_str().unwrap(), "--git"]);
    assert_eq!(out.code, 0, "{out:?}");
    // The fixture ships its own knowell.toml; the tests import from scratch.
    std::fs::remove_file(dir.join("knowell.toml")).unwrap();
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().join(".git").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        names.len() >= 2,
        "fixture has too few repositories: {names:?}"
    );
    names
}

#[test]
fn url_helpers() {
    assert_eq!(
        with_database("postgres://u:p@h:1/postgres?sslmode=disable", "x"),
        "postgres://u:p@h:1/x?sslmode=disable"
    );
    assert_eq!(password_of("postgres://u:p@h:1/db").as_deref(), Some("p"));
    assert_eq!(password_of("postgres://u@h/db"), None);
}
