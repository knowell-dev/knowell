//! Real launcher/engine processes in a private synthetic installation.
//!
//! Build both binaries before running this target: `cargo build -p knowell
//! -p knowell-launcher`, through the workspace build lock. No database is used.
//! The prerelease engine is recorded as synthetic installed version 1.0.0 only
//! for launcher admission tests. It is never passed off as a released candidate:
//! a signed candidate with another version must fail the real CLI handshake.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use knowell_update::install::{Image, Install, Owner, Phase, Receipt};
use knowell_update::manifest::{Artifact, Component, target_name};
use knowell_update::repository::{RepositoryConfig, TrustedRepository};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tough::editor::RepositoryEditor;
use tough::editor::signed::SignedRole;
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::{KeyHolder, RoleKeys, RoleType, Root, Signed, Target, Targets};
use url::Url;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);

fn engine_binary() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_know")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_know"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_know")))
}

fn launcher_binary(engine: &Path) -> PathBuf {
    let path = std::env::var_os("NEXTEST_BIN_EXE_know-launcher")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_know-launcher"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            engine.parent().unwrap().join(if cfg!(windows) {
                "know-launcher.exe"
            } else {
                "know-launcher"
            })
        });
    assert!(
        path.is_file(),
        "build knowell and knowell-launcher through the workspace build lock before this test"
    );
    path
}

fn digest_file(path: &Path) -> (u64, String) {
    let mut input = File::open(path).unwrap();
    let size = input.metadata().unwrap().len();
    let mut digest = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buf).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buf[..read]);
    }
    let hash = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (size, hash)
}

fn image(path: &Path, version: &str, target: &str) -> Image {
    let (size, sha256) = digest_file(path);
    Image {
        format_version: 1,
        version: version.into(),
        target: target.into(),
        sha256,
        size,
    }
}

struct Sandbox {
    temporary: tempfile::TempDir,
    install: Install,
    launcher: PathBuf,
    engine: PathBuf,
    binary_info: Value,
}

impl Sandbox {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        for name in ["software", "home", "data", "work"] {
            fs::create_dir(temporary.path().join(name)).unwrap();
        }
        fs::write(temporary.path().join("data/config.toml"), "version = 1\n").unwrap();
        let original_engine = engine_binary();
        let original_launcher = launcher_binary(&original_engine);
        let mut inspect = isolated_command(&temporary, &original_engine);
        let output = bounded_output(inspect.args(["update", "--inspect-binary"]));
        assert!(output.status.success());
        let binary_info: Value = serde_json::from_slice(&output.stdout).unwrap();
        let target = binary_info["target"].as_str().unwrap();
        let root = temporary.path().join("software").canonicalize().unwrap();
        let engine_dir = root.join("versions/1.0.0").join(target);
        fs::create_dir_all(&engine_dir).unwrap();
        let executable_name = if cfg!(windows) { "know.exe" } else { "know" };
        let engine = engine_dir.join(executable_name);
        let launcher = root.join(executable_name);
        fs::copy(&original_engine, &engine).unwrap();
        fs::copy(&original_launcher, &launcher).unwrap();
        let receipt = Receipt {
            format_version: 1,
            owner: Owner::Direct,
            target: target.into(),
            launcher_protocol: 1,
        };
        fs::write(
            root.join("install.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("current.json"),
            serde_json::to_vec(&image(&engine, "1.0.0", target)).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("launcher.json"),
            serde_json::to_vec(&image(&launcher, "1.0.0", target)).unwrap(),
        )
        .unwrap();
        let install = Install::open(&root).unwrap();
        install.verify_launcher().unwrap();
        Self {
            temporary,
            install,
            launcher,
            engine,
            binary_info,
        }
    }

    fn command(&self, executable: &Path) -> Command {
        isolated_command(&self.temporary, executable)
    }
}

fn isolated_command(temporary: &tempfile::TempDir, executable: &Path) -> Command {
    let mut command = Command::new(executable);
    // Retain system execution settings; no developer Knowell configuration,
    // lifetime channel, provider, database, proxy, or token reaches the fixture.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("KNOWELL_") {
            command.env_remove(name);
        }
    }
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "RUST_LOG",
    ] {
        command.env_remove(name);
    }
    command
        .env("HOME", temporary.path().join("home"))
        .env("USERPROFILE", temporary.path().join("home"))
        .env("KNOWELL_HOME", temporary.path().join("data"))
        .env("NO_COLOR", "1")
        .current_dir(temporary.path().join("work"));
    command
}

fn bounded_output(command: &mut Command) -> Output {
    let mut command = tokio::process::Command::from(std::mem::replace(
        command,
        Command::new("unused-test-command"),
    ));
    command.stdin(Stdio::null()).kill_on_drop(true);
    block_on(async {
        tokio::time::timeout(PROCESS_TIMEOUT, command.output())
            .await
            .expect("test process did not exit and close its pipes before the deadline")
            .unwrap()
    })
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

enum Event {
    Line(String),
    Eof,
}

struct Lines {
    receiver: Receiver<Event>,
    seen: Vec<String>,
    ended: bool,
}

impl Lines {
    fn new(stdout: impl Read + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("cannot read the synthetic process stdout");
                if sender.send(Event::Line(line)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Event::Eof);
        });
        Self {
            receiver,
            seen: Vec::new(),
            ended: false,
        }
    }

    fn next(&mut self, deadline: Instant) -> Option<String> {
        if self.ended {
            return None;
        }
        match self
            .receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("no process stdout before the deadline")
        {
            Event::Line(line) => {
                self.seen.push(line.clone());
                Some(line)
            }
            Event::Eof => {
                self.ended = true;
                None
            }
        }
    }

    fn response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while let Some(line) = self.next(deadline) {
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("MCP stdout contained a non-protocol line"));
            assert_eq!(message["jsonrpc"], "2.0");
            if message["id"] == id {
                return message;
            }
        }
        panic!("MCP ended before response {id}");
    }

    fn eof(&mut self) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while self.next(deadline).is_some() {}
    }

    fn assert_protocol(&self) {
        for line in &self.seen {
            let message: Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("MCP stdout contained a non-protocol line"));
            assert_eq!(message["jsonrpc"], "2.0");
        }
    }
}

struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines,
}

impl Process {
    fn start(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let lines = Lines::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn initialize(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "synthetic-updater-test", "version": "0.0.0"}
            }
        }));
        let initialized = self.lines.response(1);
        assert!(initialized["result"]["serverInfo"].is_object());
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn tools(&mut self, id: u64) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}));
        let response = self.lines.response(id);
        assert!(response["result"]["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["name"].as_str() == Some("save_checkpoint"))
        }));
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "process did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop_mcp(&mut self) {
        drop(self.stdin.take());
        assert_eq!(self.wait().code(), Some(0));
        self.lines.eof();
        self.lines.assert_protocol();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // A failed test must still allow the engine's lifetime watchdog to close
        // inherited pipes before the temporary executable directory is removed.
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while !self.lines.ended {
            match self
                .lines
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Event::Line(_)) => {}
                Ok(Event::Eof) => self.lines.ended = true,
                Err(_) => break,
            }
        }
    }
}

#[test]
fn launcher_preserves_cli_stdout_and_child_exit_codes() {
    let sandbox = Sandbox::new();
    for args in [vec!["--version"], vec!["--synthetic-invalid-option"]] {
        let direct = bounded_output(sandbox.command(&sandbox.engine).args(&args));
        let launched = bounded_output(sandbox.command(&sandbox.launcher).args(&args));
        assert_eq!(direct.status.code(), launched.status.code());
        assert_eq!(direct.stdout, launched.stdout);
        assert_eq!(direct.stderr, launched.stderr);
    }
}

#[test]
fn launcher_keeps_mcp_protocol_clean_and_releases_admission_after_eof() {
    let sandbox = Sandbox::new();
    let mut process = Process::start(sandbox.command(&sandbox.launcher).arg("mcp"));
    process.initialize();
    process.tools(2);
    let owner = sandbox.install.update_lease().unwrap();
    assert!(matches!(
        sandbox.install.exclusive_runtime(&owner),
        Err(knowell_update::Error::Busy)
    ));
    process.stop_mcp();
    let exclusive = sandbox.install.exclusive_runtime(&owner).unwrap();
    drop(exclusive);
}

#[test]
fn killing_launcher_stops_engine_and_releases_its_runtime_lease() {
    let sandbox = Sandbox::new();
    let free = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = free.local_addr().unwrap();
    drop(free);
    let mut process = Process::start(sandbox.command(&sandbox.launcher).args([
        "serve",
        "--no-database",
        "--listen",
        &address.to_string(),
    ]));
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while let Some(line) = process.lines.next(deadline) {
        if line == "press Ctrl+C to stop" {
            break;
        }
    }
    assert!(
        process
            .lines
            .seen
            .iter()
            .any(|line| line == "press Ctrl+C to stop")
    );
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET /api/v1/health/live HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    drop(stream);
    let owner = sandbox.install.update_lease().unwrap();
    assert!(matches!(
        sandbox.install.exclusive_runtime(&owner),
        Err(knowell_update::Error::Busy)
    ));
    // Child::kill targets only the launcher on Unix and Windows. The engine's
    // private lifetime channel must close independently of stdin EOF.
    process.child.kill().unwrap();
    assert!(!process.wait().success());
    process.lines.eof();
    let exclusive = sandbox.install.exclusive_runtime(&owner).unwrap();
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
    drop(exclusive);
}

struct SignedFixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    config: RepositoryConfig,
    candidate: Image,
}

impl SignedFixture {
    async fn new(sandbox: &Sandbox, engine_source: Option<&Path>) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let key_path = directory.join("synthetic-signing-key.der");
        let random = aws_lc_rs::rand::SystemRandom::new();
        let key = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&random).unwrap();
        fs::write(&key_path, key.as_ref()).unwrap();
        let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource { path: key_path })];
        let public_key = keys.first().unwrap().as_sign().await.unwrap().tuf_key();
        let key_id = public_key.key_id().unwrap();
        let role = RoleKeys {
            keyids: vec![key_id.clone()],
            threshold: NonZeroU64::new(1).unwrap(),
            _extra: HashMap::new(),
        };
        let root = Root {
            spec_version: "1.0.0".into(),
            consistent_snapshot: true,
            version: NonZeroU64::new(1).unwrap(),
            expires: "2100-01-01T00:00:00Z".parse().unwrap(),
            keys: HashMap::from([(key_id, public_key)]),
            roles: [
                RoleType::Root,
                RoleType::Timestamp,
                RoleType::Snapshot,
                RoleType::Targets,
            ]
            .into_iter()
            .map(|kind| (kind, role.clone()))
            .collect(),
            _extra: HashMap::new(),
        };
        let signed = SignedRole::new(root.clone(), &KeyHolder::Root(root), &keys, &random)
            .await
            .unwrap();
        let root = directory.join("root.json");
        fs::write(&root, signed.buffer()).unwrap();
        let metadata = directory.join("metadata");
        let targets = directory.join("targets");
        fs::create_dir(&metadata).unwrap();
        fs::create_dir(&targets).unwrap();
        let actual_version = sandbox.binary_info["version"].as_str().unwrap();
        let version = semver::Version::parse(if actual_version == "1.1.0" {
            "1.2.0"
        } else {
            "1.1.0"
        })
        .unwrap();
        let target = sandbox.install.receipt().target.as_str();
        let version_dir = targets.join(format!("v{version}"));
        fs::create_dir(&version_dir).unwrap();
        let engine_path = directory.join("synthetic-engine");
        match engine_source {
            Some(source) => {
                fs::copy(source, &engine_path).unwrap();
            }
            None => fs::write(&engine_path, b"synthetic verified non-serving engine").unwrap(),
        }
        let candidate = image(&engine_path, &version.to_string(), target);
        let engine = Artifact {
            name: target_name(&version, target, Component::Engine).unwrap(),
            size: candidate.size,
            sha256: candidate.sha256.clone(),
        };
        let launcher_path = directory.join("synthetic-launcher");
        fs::write(&launcher_path, b"synthetic verified non-serving launcher").unwrap();
        let launcher_identity = image(&launcher_path, &version.to_string(), target);
        let launcher = Artifact {
            name: target_name(&version, target, Component::Launcher).unwrap(),
            size: launcher_identity.size,
            sha256: launcher_identity.sha256,
        };
        for (artifact, source) in [(&engine, &engine_path), (&launcher, &launcher_path)] {
            let (_, basename) = artifact.name.split_once('/').unwrap();
            fs::copy(
                source,
                version_dir.join(format!("{}.{basename}", artifact.sha256)),
            )
            .unwrap();
        }
        let mut engine_target = Target::from_path(&engine_path).await.unwrap();
        let schema = sandbox.binary_info["schema"].as_u64().unwrap();
        engine_target.custom.insert(
            "knowell".into(),
            json!({
                "format_version": 1, "version": version, "target": target,
                "channel": "stable", "component": "engine", "revoked": false,
                "launcher": launcher,
                "compatibility": {
                    "schema": {"read_min":schema,"read_max":schema,"write_min":schema,"write_max":schema},
                    "config":{"min":1,"max":1},"index":{"min":1,"max":1},
                    "jobs":{"min":1,"max":1},"protocol":{"min":1,"max":1},"launcher":{"min":1,"max":1}
                }
            }),
        );
        let mut editor = RepositoryEditor::new(&root).await.unwrap();
        let metadata_version = NonZeroU64::new(1).unwrap();
        // Format 1 accepts only a non-delegated root targets role. Tough's
        // empty editor otherwise adds an empty delegation block by default.
        editor
            .targets(Signed {
                signed: Targets {
                    spec_version: "1.0.0".into(),
                    version: metadata_version,
                    expires: "2100-01-01T00:00:00Z".parse().unwrap(),
                    targets: HashMap::new(),
                    delegations: None,
                    _extra: HashMap::new(),
                },
                signatures: Vec::new(),
            })
            .unwrap();
        editor
            .targets_version(metadata_version)
            .unwrap()
            .targets_expires("2100-01-01T00:00:00Z".parse().unwrap())
            .unwrap()
            .snapshot_version(metadata_version)
            .snapshot_expires("2100-01-01T00:00:00Z".parse().unwrap())
            .timestamp_version(metadata_version)
            .timestamp_expires("2100-01-01T00:00:00Z".parse().unwrap());
        editor
            .add_target(tough::TargetName::new(&engine.name).unwrap(), engine_target)
            .unwrap()
            .add_target(
                tough::TargetName::new(&launcher.name).unwrap(),
                Target::from_path(&launcher_path).await.unwrap(),
            )
            .unwrap();
        editor
            .sign(&keys)
            .await
            .unwrap()
            .write(&metadata)
            .await
            .unwrap();
        // File URLs use ordinary native Windows paths. Their fixture directory
        // is canonical already; omit the verbatim prefix introduced by Windows
        // canonicalization when constructing URLs.
        #[cfg(windows)]
        let metadata = metadata
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned();
        #[cfg(windows)]
        let targets = targets
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned();
        #[cfg(unix)]
        let metadata = metadata.canonicalize().unwrap();
        #[cfg(unix)]
        let targets = targets.canonicalize().unwrap();
        let config = RepositoryConfig {
            trusted_root: Some(signed.buffer().to_vec()),
            metadata_url: Url::from_directory_path(metadata).unwrap(),
            targets_url: Url::from_directory_path(targets).unwrap(),
            datastore: sandbox.install.root().join("metadata"),
            offline: true,
            recover_pending: false,
            max_target_bytes: knowell_update::manifest::MAX_TARGET_BYTES,
        };
        Self {
            _temporary: temporary,
            root,
            config,
            candidate,
        }
    }
}

#[test]
fn signed_prepare_preserves_active_mcp_and_activation_reports_busy() {
    let sandbox = Sandbox::new();
    let mut process = Process::start(sandbox.command(&sandbox.launcher).arg("mcp"));
    process.initialize();
    let before = sandbox.install.current().unwrap();
    let fixture = block_on(SignedFixture::new(&sandbox, None));
    let owner = sandbox.install.update_lease().unwrap();
    block_on(async {
        let repository = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let release = repository.releases().first().unwrap();
        let (staged, file) = sandbox.install.stage(&fixture.candidate, &owner).unwrap();
        let mut staged_file = tokio::fs::File::from_std(file);
        repository
            .download(&release.artifact, &mut staged_file)
            .await
            .unwrap();
        drop(staged_file);
        process.tools(2);
        let launcher = &release.metadata.launcher;
        let launcher_image = Image {
            sha256: launcher.sha256.clone(),
            size: launcher.size,
            ..fixture.candidate.clone()
        };
        let (launcher_staged, file) = sandbox.install.stage(&launcher_image, &owner).unwrap();
        let mut launcher_file = tokio::fs::File::from_std(file);
        repository
            .download(launcher, &mut launcher_file)
            .await
            .unwrap();
        drop(launcher_file);
        sandbox
            .install
            .retain_launcher(&launcher_image, &launcher_staged, &owner)
            .unwrap();
        let transaction = sandbox
            .install
            .prepare(fixture.candidate.clone(), &staged, &owner)
            .unwrap();
        assert_eq!(transaction.phase, Phase::Prepared);
    });
    assert_eq!(sandbox.install.current().unwrap(), before);
    assert!(matches!(
        sandbox.install.exclusive_runtime(&owner),
        Err(knowell_update::Error::Busy)
    ));
    process.tools(3);
    process.stop_mcp();
    let exclusive = sandbox.install.exclusive_runtime(&owner).unwrap();
    drop(exclusive);
    sandbox.install.abort(&owner).unwrap();
    assert_eq!(sandbox.install.current().unwrap(), before);
}

#[test]
fn cli_rejects_signed_candidate_version_mismatch_without_interrupting_mcp() {
    let sandbox = Sandbox::new();
    let mut process = Process::start(sandbox.command(&sandbox.launcher).arg("mcp"));
    process.initialize();
    let before = sandbox.install.current().unwrap();
    let fixture = block_on(SignedFixture::new(&sandbox, Some(&sandbox.engine)));
    let configured = bounded_output(sandbox.command(&sandbox.engine).args([
        "update",
        "--configure-source",
        "--offline",
        "--trust-root",
        fixture.root.to_str().unwrap(),
        "--metadata-url",
        fixture.config.metadata_url.as_str(),
        "--targets-url",
        fixture.config.targets_url.as_str(),
    ]));
    assert!(
        configured.status.success(),
        "synthetic signed source configuration failed: {}",
        String::from_utf8_lossy(&configured.stderr)
    );
    assert!(sandbox.install.update_source().unwrap().is_some());
    process.tools(2);
    let plan = bounded_output(sandbox.command(&sandbox.engine).args([
        "update",
        "--plan",
        "--version",
        &fixture.candidate.version,
    ]));
    assert!(
        plan.status.success(),
        "persisted synthetic source planning failed: {}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(String::from_utf8_lossy(&plan.stdout).contains(&fixture.candidate.version));
    process.tools(3);
    let output = bounded_output(sandbox.command(&sandbox.engine).args([
        "update",
        "--prepare",
        "--version",
        &fixture.candidate.version,
    ]));
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("candidate executable identity differs from its signed release manifest")
    );
    assert!(sandbox.install.transaction().unwrap().is_none());
    assert_eq!(sandbox.install.current().unwrap(), before);
    process.tools(4);
    process.stop_mcp();
}
