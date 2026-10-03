//! Synthetic process tree matching CLI -> pg_ctl -> long-lived server.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::time::Instant;

const HELPER: &str = "process::pipe_tests::process_helper";
const MODE: &str = "KNOWELL_PROCESS_TEST_MODE";
const ROOT: &str = "KNOWELL_PROCESS_TEST_ROOT";
const LIMIT: Duration = Duration::from_secs(10);

fn helper_args() -> Vec<OsString> {
    ["--exact", HELPER, "--nocapture"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

struct StopServer(PathBuf);

impl Drop for StopServer {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("stop"), []);
    }
}

async fn wait_for(path: &Path) {
    tokio::time::timeout(LIMIT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("synthetic process did not create its marker");
}

#[tokio::test]
async fn caller_pipes_close_while_server_keeps_running() {
    let dir = tempfile::tempdir().unwrap();
    let _stop = StopServer(dir.path().to_path_buf());
    let mut caller = Command::new(std::env::current_exe().unwrap());
    caller
        .args(helper_args())
        .env(MODE, "caller")
        .env(ROOT, dir.path())
        .env("PGAPPNAME", "KNOWELL_CANARY_AMBIENT_PG")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    caller.creation_flags(0x0800_0000);
    let output = caller.output();
    tokio::pin!(output);
    // Poll output while the caller starts its descendants. Waiting for EOF is
    // bounded, and cleanup releases the server even when inheritance regresses.
    let result = tokio::time::timeout(LIMIT, &mut output).await;
    let server_alive = dir.path().join("ready").exists() && !dir.path().join("stopped").exists();
    std::fs::write(dir.path().join("stop"), []).unwrap();
    let closed_before_stop = result.is_ok();
    let output = match result {
        Ok(output) => output.unwrap(),
        Err(_) => tokio::time::timeout(LIMIT, output)
            .await
            .expect("caller did not exit after server cleanup")
            .unwrap(),
    };
    wait_for(&dir.path().join("stopped")).await;
    assert!(output.status.success(), "synthetic caller failed");
    assert!(server_alive, "server exited before the pipe observation");
    assert!(
        closed_before_stop,
        "server retained its caller's output pipes"
    );
}

#[tokio::test]
async fn null_stdio_reports_start_and_exit_errors_without_output() {
    let options = RunOptions::new(LIMIT).null_stdio();
    assert!(matches!(
        run(Path::new("missing-knowell-test-program"), &[], &options).await,
        Err(Error::Io { .. })
    ));
    let dir = tempfile::tempdir().unwrap();
    let options = options
        .env(MODE, "nonzero")
        .env(ROOT, dir.path().as_os_str());
    let exe = std::env::current_exe().unwrap();
    let output = run_unchecked(&exe, &helper_args(), &options).await.unwrap();
    assert_eq!(output.code, Some(17));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(matches!(
        run(&exe, &helper_args(), &options).await,
        Err(Error::Command { .. })
    ));
}

#[tokio::test]
async fn null_stdio_kills_launcher_on_timeout_and_cancellation() {
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let _stop = StopServer(dir.path().to_path_buf());
        let options = RunOptions::new(if cancel {
            LIMIT
        } else {
            Duration::from_secs(2)
        })
        .null_stdio()
        .env(MODE, "heartbeat")
        .env(ROOT, dir.path().as_os_str());
        let exe = std::env::current_exe().unwrap();
        let task = tokio::spawn(async move { run(&exe, &helper_args(), &options).await });
        wait_for(&dir.path().join("heartbeat")).await;
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert!(matches!(task.await.unwrap(), Err(Error::Timeout { .. })));
        }
        // TerminateProcess is asynchronous. Once it has settled the synthetic
        // launcher must stop making progress, even though no stop was requested.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = std::fs::read(dir.path().join("heartbeat")).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(before, std::fs::read(dir.path().join("heartbeat")).unwrap());
    }
}

#[tokio::test]
async fn null_stdio_rejects_nul_input_without_echoing_it() {
    let exe = std::env::current_exe().unwrap();
    let canary = "KNOWELL_CANARY_INVALID_PROCESS_INPUT";
    let invalid = format!("{canary}\0suffix");
    let options = RunOptions::new(LIMIT).null_stdio();
    for (args, options) in [
        (vec![OsString::from(&invalid)], options.clone()),
        (helper_args(), options.clone().env("KNOWELL_FAKE", &invalid)),
        (helper_args(), options.cwd(Path::new(&invalid))),
    ] {
        let error = run(&exe, &args, &options).await.unwrap_err();
        assert!(matches!(error, Error::Io { .. }));
        assert!(!error.to_string().contains(canary));
    }
}

#[tokio::test]
async fn process_helper() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).expect("synthetic root"));
    let exe = std::env::current_exe().unwrap();
    match mode.as_str() {
        "caller" => {
            let cwd = root.join("working directory \u{00f6}");
            std::fs::create_dir(&cwd).unwrap();
            let options = RunOptions::new(LIMIT)
                .null_stdio()
                .cwd(&cwd)
                .env(MODE, "launcher")
                .env(ROOT, root.as_os_str())
                .env("PGOPTIONS", "KNOWELL_CANARY_EXPLICIT_PG");
            run(&exe, &helper_args(), &options).await.unwrap();
        }
        "launcher" => {
            assert!(std::env::var_os("PGAPPNAME").is_none());
            assert_eq!(
                std::env::var("PGOPTIONS").unwrap(),
                "KNOWELL_CANARY_EXPLICIT_PG"
            );
            assert_eq!(
                std::env::current_dir().unwrap().canonicalize().unwrap(),
                root.join("working directory \u{00f6}")
                    .canonicalize()
                    .unwrap()
            );
            let mut server = std::process::Command::new(exe);
            server
                .args(helper_args())
                .env(MODE, "server")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                server.creation_flags(0x0800_0000);
            }
            // Like pg_ctl, this launcher returns while the server remains alive.
            #[allow(clippy::zombie_processes)]
            let _server = server.spawn().unwrap();
            wait_for(&root.join("ready")).await;
        }
        "server" => {
            std::fs::write(root.join("ready"), []).unwrap();
            let start = Instant::now();
            while !root.join("stop").exists() && start.elapsed() < LIMIT * 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            std::fs::write(root.join("stopped"), []).unwrap();
        }
        "heartbeat" => {
            let start = Instant::now();
            let mut count = 0_u64;
            while !root.join("stop").exists() && start.elapsed() < LIMIT * 3 {
                std::fs::write(root.join("heartbeat"), count.to_string()).unwrap();
                count += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        "nonzero" => std::process::exit(17),
        _ => panic!("unknown synthetic helper mode"),
    }
}
