//! Packaging includes the real launcher, whose unknown ownership fails closed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

struct Fixture {
    directory: PathBuf,
    executable: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("knowell-launcher-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&directory).unwrap();
        let executable = directory.join(if cfg!(windows) { "know.exe" } else { "know" });
        let original = std::env::var_os("NEXTEST_BIN_EXE_know-launcher")
            .or_else(|| std::env::var_os("CARGO_BIN_EXE_know-launcher"))
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_know-launcher").into());
        fs::copy(original, &executable).unwrap();
        Self {
            directory,
            executable,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The fixture owns precisely these two paths; no recursive deletion of
        // a computed temporary directory or unrelated installation is needed.
        let _ = fs::remove_file(&self.executable);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[test]
fn a_launcher_without_an_installation_receipt_fails_closed() {
    let fixture = Fixture::new();
    let mut command = tokio::process::Command::new(&fixture.executable);
    command
        .arg("--version")
        .current_dir(&fixture.directory)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .expect("the launcher did not fail closed before its deadline")
            .unwrap()
    });
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid installation receipt"));
    assert!(!fixture.directory.join("install.json").exists());
    assert!(!fixture.directory.join("versions").exists());
}
