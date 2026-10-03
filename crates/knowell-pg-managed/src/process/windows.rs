//! A server launcher must never receive the CLI's ambient pipe handles.

use super::{Error, OsString, Output, Path, Result, RunOptions};
use std::time::Duration;
use windows_spawn::{Child, Command, CreationFlags, SpawnOptions, Stdio};

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // try_wait caches a completed exit, making kill a no-op on success.
        // Only the launcher is killed on cancellation; the server it started
        // must remain alive after a successful launch.
        let _ = self.0.kill();
    }
}

pub(super) async fn run_null(
    program: &Path,
    args: &[OsString],
    options: &RunOptions,
    display: &str,
) -> Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
    // windows-spawn passes an explicit PROC_THREAD_ATTRIBUTE_HANDLE_LIST with
    // only these three NUL handles. std::process::Command on stable Rust also
    // inherits unrelated handles, even when all three streams are redirected.
    let child = command
        .spawn_with(SpawnOptions::new().creation_flags(CreationFlags::NO_WINDOW))
        .map_err(|error| Error::io(format!("starting {display}"), error))?;
    let mut child = KillOnDrop(child);
    let wait = async {
        loop {
            if let Some(status) = child.0.try_wait()? {
                return Ok::<_, std::io::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let status = tokio::time::timeout(options.timeout, wait)
        .await
        .map_err(|_| Error::Timeout {
            program: display.to_string(),
            seconds: options.timeout.as_secs(),
        })?
        .map_err(|error| Error::io(format!("waiting for {display}"), error))?;
    Ok(Output {
        code: status.code(),
        stdout: String::new(),
        stderr: String::new(),
    })
}
